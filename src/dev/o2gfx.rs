//! O2 graphics: GBE, the display back end, and CRIME's rendering engine.
//!
//! The O2 has no video memory. The frame buffer is ordinary RAM in 64 KB
//! tiles; GBE fetches it through a tile list and turns it into pixels, and
//! the rendering engine (RE) draws into it through its own tile tables. Both
//! are DMA masters on the memory controller, so neither sees what the CPU
//! still holds in its caches, exactly as on the hardware.
//!
//! Everything here is the plain CPU version: scan-out is a pure function of
//! RAM and the GBE registers ([`Gbe::render`]), and each RE primitive runs to
//! completion when it is started, writing RAM through [`O2Memory::write`] so
//! jitv2's page generations see it. RAM is the only store. That keeps this a
//! usable oracle for a later GPU (compute shader) version of the same thing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::dev::crime_mem::O2Memory;
use crate::traits::{BusDevice, BusRead32, BusRead64, BUS_OK};

/// GBE register offsets from its base, `0x1600_0000`.
pub mod gbe_reg {
    pub const CTRLSTAT: u32 = 0x00000;
    pub const DOTCLOCK: u32 = 0x00004;
    pub const I2C: u32 = 0x00008;
    pub const I2CFP: u32 = 0x00010;
    pub const DEVICE_ID: u32 = 0x00014;
    pub const VT_XY: u32 = 0x10000;
    pub const VT_XYMAX: u32 = 0x10004;
    pub const VT_FLAGS: u32 = 0x10018;
    pub const VT_INTR01: u32 = 0x10020;
    pub const VT_INTR23: u32 = 0x10024;
    pub const VT_HCMAP: u32 = 0x1003c;
    pub const VT_VCMAP: u32 = 0x10040;
    pub const OVR_WIDTH_TILE: u32 = 0x20000;
    pub const OVR_CONTROL: u32 = 0x20004;
    pub const OVR_CONTROL_SHADOW: u32 = 0x20008;
    pub const FRM_SIZE_TILE: u32 = 0x30000;
    pub const FRM_SIZE_PIXEL: u32 = 0x30004;
    pub const FRM_CONTROL: u32 = 0x30008;
    pub const FRM_CONTROL_SHADOW: u32 = 0x3000c;
    pub const DID_CONTROL: u32 = 0x40000;
    pub const DID_CONTROL_SHADOW: u32 = 0x40004;
    pub const MODE: u32 = 0x48000;
    pub const CMAP: u32 = 0x50000;
    pub const CM_FIFO: u32 = 0x58000;
    pub const GMAP: u32 = 0x60000;
    pub const CRS_POS: u32 = 0x70000;
    pub const CRS_CTL: u32 = 0x70004;
    pub const CRS_CMAP1: u32 = 0x70008;
    pub const CRS_GLYPH: u32 = 0x78000;
}

use gbe_reg::*;

/// Offsets 0 to the end of the video-capture block.
const GBE_WORDS: usize = 0x81000 / 4;
const VT_FREEZE: u32 = 1 << 31;
const DOTCLOCK_RUN: u32 = 1 << 20;
/// Dots the beam moves on every read of VT_XY, on top of guest time, so a
/// poll for blanking always sees it move even if guest time stands still.
const DOTS_PER_READ: u64 = 32;

struct GbeState {
    r: Vec<u32>,
    /// Guest microseconds when the beam was last unfrozen.
    t0: u64,
    /// VT_XY reads since then.
    reads: u64,
    /// Beam position (in dots since t0) when vt_crossings last looked.
    seen_dots: u64,
    /// Shadow writes waiting for vertical blanking: (live offset, value,
    /// frame number when written).
    pending: Vec<(u32, u32, u64)>,
}

pub struct Gbe {
    s: Mutex<GbeState>,
    /// Reads per register offset, for finding a driver spinning on one.
    pub hot: Mutex<BTreeMap<u32, u64>>,
    /// Guest time in microseconds (MACE UST).
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// The beam's clock when it should follow the CPU instead: microseconds
    /// from the instruction count (see `use_cpu_clock`).
    cpu_clock: std::sync::OnceLock<(crate::cpu::mips_core::CyclesPtr, u64)>,
    /// The 64 KB tiles the last rendered frame came from (physical >> 16),
    /// for a memory watch that wants only what is on screen.
    pub displayed: Mutex<std::collections::HashSet<u32>>,
}

impl Gbe {
    pub fn new(now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        let mut r = vec![0u32; GBE_WORDS];
        // GPIO pins unconfigured (inputs), chip id 1.
        r[(CTRLSTAT / 4) as usize] = 0x03ff_ffc1;
        r[(VT_XY / 4) as usize] = VT_FREEZE;
        Self { s: Mutex::new(GbeState { r, t0: 0, reads: 0, seen_dots: 0, pending: Vec::new() }), hot: Mutex::new(BTreeMap::new()), now, cpu_clock: std::sync::OnceLock::new(), displayed: Mutex::new(Default::default()) }
    }

    /// Run the beam on the CPU's instruction count at `mhz` rather than on
    /// the host clock. The vertical-timing interrupts then come at the rate
    /// the guest would see on its own hardware: on the host clock a slow
    /// emulated CPU gets sixty a real second whatever it manages to do, and
    /// IRIX's interrupt dispatcher -- which re-reads CRIME until nothing is
    /// pending -- found GBE latched again before each pass was out and never
    /// returned.
    pub fn use_cpu_clock(&self, cycles: crate::cpu::mips_core::CyclesPtr, mhz: u32) {
        let _ = self.cpu_clock.set((cycles, mhz.max(1) as u64));
    }

    fn now(&self) -> u64 {
        match self.cpu_clock.get() {
            Some((c, mhz)) => c.get() / mhz,
            None => (self.now)(),
        }
    }

    fn get(r: &[u32], off: u32) -> u32 {
        r.get((off / 4) as usize).copied().unwrap_or(0)
    }

    /// The dot clock in Hz: 20 MHz x (M+1) / ((N+1) x 2^P).
    fn dot_hz(r: &[u32]) -> u64 {
        let d = Self::get(r, DOTCLOCK);
        let m = (d & 0xff) as u64 + 1;
        let n = ((d >> 8) & 0x3f) as u64 + 1;
        let p = (d >> 14) & 3;
        20_000_000 * m / (n << p)
    }

    fn beam(&self, s: &mut GbeState) -> u32 {
        let xy = Self::get(&s.r, VT_XY);
        if xy & VT_FREEZE != 0 {
            return VT_FREEZE;
        }
        let max = Self::get(&s.r, VT_XYMAX);
        let (maxx, maxy) = ((max & 0xfff) as u64, ((max >> 12) & 0xfff) as u64);
        if Self::get(&s.r, DOTCLOCK) & DOTCLOCK_RUN == 0 || maxx == 0 || maxy == 0 {
            return xy & 0x00ff_ffff;
        }
        s.reads += 1;
        let us = self.now().saturating_sub(s.t0);
        let dots = us * Self::dot_hz(&s.r) / 1_000_000 + s.reads * DOTS_PER_READ;
        let x = dots % (maxx + 1);
        let y = (dots / (maxx + 1)) % (maxy + 1);
        ((y as u32) << 12) | x as u32
    }

    /// Frames the beam has completed since t0, or None while the timing
    /// generator is stopped (frozen, no dot clock, or no timing set).
    fn frame(&self, s: &GbeState) -> Option<u64> {
        if Self::get(&s.r, VT_XY) & VT_FREEZE != 0 || Self::get(&s.r, DOTCLOCK) & DOTCLOCK_RUN == 0 {
            return None;
        }
        let max = Self::get(&s.r, VT_XYMAX);
        let (maxx, maxy) = ((max & 0xfff) as u64, ((max >> 12) & 0xfff) as u64);
        if maxx == 0 || maxy == 0 {
            return None;
        }
        let us = self.now().saturating_sub(s.t0);
        let dots = us * Self::dot_hz(&s.r) / 1_000_000 + s.reads * DOTS_PER_READ;
        Some(dots / ((maxx + 1) * (maxy + 1)))
    }

    /// Copy every shadow written before the current frame into its live
    /// register, as the start of vertical blanking does; all of them at once
    /// if the timing generator has stopped.
    fn latch(&self, s: &mut GbeState) {
        if s.pending.is_empty() {
            return;
        }
        let now = self.frame(s);
        let pending = std::mem::take(&mut s.pending);
        for (live, val, at) in pending {
            if now.is_none_or(|f| f > at) {
                s.r[(live / 4) as usize] = val;
            } else {
                s.pending.push((live, val, at));
            }
        }
    }

    /// Which of the four vertical-timing interrupts fired since the last call:
    /// bit n when the beam reached line VT_INTRn (CRIME latches them as its
    /// bits 16..19). A line beyond the frame never fires.
    pub fn vt_crossings(&self) -> u32 {
        let mut s = self.s.lock().unwrap();
        if Self::get(&s.r, VT_XY) & VT_FREEZE != 0 || Self::get(&s.r, DOTCLOCK) & DOTCLOCK_RUN == 0 {
            return 0;
        }
        let max = Self::get(&s.r, VT_XYMAX);
        let (maxx, maxy) = ((max & 0xfff) as u64, ((max >> 12) & 0xfff) as u64);
        if maxx == 0 || maxy == 0 {
            return 0;
        }
        let us = self.now().saturating_sub(s.t0);
        let now = us * Self::dot_hz(&s.r) / 1_000_000 + s.reads * DOTS_PER_READ;
        let last = std::mem::replace(&mut s.seen_dots, now);
        if now <= last {
            return 0;
        }
        let (i01, i23) = (Self::get(&s.r, VT_INTR01), Self::get(&s.r, VT_INTR23));
        let lines = [(i01 >> 12) & 0xfff, i01 & 0xfff, (i23 >> 12) & 0xfff, i23 & 0xfff];
        let period = (maxx + 1) * (maxy + 1);
        // Crossings of `off` (mod period) at or before `p`.
        let count = |p: u64, off: u64| if p < off { 0 } else { (p - off) / period + 1 };
        let mut fired = 0;
        for (n, &line) in lines.iter().enumerate() {
            let line = line as u64;
            if line > maxy {
                continue;
            }
            let off = line * (maxx + 1);
            if count(now, off) != count(last, off) {
                fired |= 1 << n;
            }
        }
        fired
    }

    fn read_reg(&self, off: u32) -> u32 {
        *self.hot.lock().unwrap().entry(off).or_insert(0) += 1;
        let mut s = self.s.lock().unwrap();
        self.latch(&mut s);
        match off {
            CTRLSTAT => {
                let v = Self::get(&s.r, CTRLSTAT);
                // Chip id 1, monitor sense 0, and board revision 1 on the
                // strap pin io_3 while it is an input: with revision 0 the
                // PROM loads a bit-reversed gamma ramp for early boards.
                let strap = if v & (1 << 13) != 0 { 1 << 12 } else { 0 };
                (v & !0x1f) | 1 | strap
            }
            // Inverted I2C with nothing on the bus: the lines read as driven.
            I2C | I2CFP => Self::get(&s.r, off) & 3,
            DEVICE_ID => 0x666,
            VT_XY => self.beam(&mut s),
            // An idle rev-1 colour-map FIFO, the one value every poller takes.
            CM_FIFO => 0,
            _ => Self::get(&s.r, off),
        }
    }

    fn write_reg(&self, off: u32, val: u32) {
        let mut s = self.s.lock().unwrap();
        let i = (off / 4) as usize;
        if i >= s.r.len() {
            return;
        }
        if off == VT_XY && val & VT_FREEZE == 0 && s.r[i] & VT_FREEZE != 0 {
            // Frame numbers restart with the beam: nothing pending may wait
            // on the old count.
            for (live, v, _) in std::mem::take(&mut s.pending) {
                s.r[(live / 4) as usize] = v;
            }
            s.t0 = self.now();
            s.reads = 0;
            s.seen_dots = 0;
        }
        s.r[i] = val;
        // The hardware copies each DMA control's shadow into the live
        // register at the start of vertical blanking, so until then the live
        // one still reads what it held. IRIX depends on that: its mode set
        // writes the overlay shadow with DMA on and then waits for the live
        // bit to read the old state, which it would never see if the copy
        // were immediate. Pollers that wait for the new value get it a frame
        // later. With the timing generator stopped there is no blanking to
        // wait for, and the copy is immediate.
        let live = match off {
            OVR_CONTROL_SHADOW => Some(OVR_CONTROL),
            FRM_CONTROL_SHADOW => Some(FRM_CONTROL),
            DID_CONTROL_SHADOW => Some(DID_CONTROL),
            _ => None,
        };
        if let Some(l) = live {
            self.latch(&mut s);
            match self.frame(&s) {
                Some(f) => {
                    s.pending.retain(|p| p.0 != l);
                    s.pending.push((l, val, f));
                }
                None => s.r[(l / 4) as usize] = val,
            }
        }
    }

    /// The registers that decide what is on screen, for a report.
    pub fn summary(&self) -> String {
        let s = self.s.lock().unwrap();
        let g = |o: u32| Self::get(&s.r, o);
        format!(
            "ctrlstat {:#010x} dotclock {:#010x} vt_xy {:#010x} xymax {:#010x} hcmap {:#010x} vcmap {:#010x} \
             flags {:#x} tile {:#010x} pix {:#010x} frm {:#010x}/{:#010x} did {:#010x} mode0 {:#x}",
            g(CTRLSTAT), g(DOTCLOCK), g(VT_XY), g(VT_XYMAX), g(VT_HCMAP), g(VT_VCMAP), g(VT_FLAGS),
            g(FRM_SIZE_TILE), g(FRM_SIZE_PIXEL), g(FRM_CONTROL), g(FRM_CONTROL_SHADOW), g(DID_CONTROL), g(MODE)
        )
    }

    /// Scan the frame out of RAM into `out` (R, G, B, A bytes, row 0 at the
    /// top), as GBE would. Returns the size drawn, or None when the display
    /// is off. `out` must hold the largest mode (2048 x 2048 pixels).
    pub fn render(&self, mem: &O2Memory, out: &mut [u8]) -> Option<(u32, u32)> {
        let r = {
            let mut s = self.s.lock().unwrap();
            self.latch(&mut s);
            s.r.clone()
        };
        let g = |off: u32| Self::get(&r, off);
        let w = (g(VT_HCMAP) >> 12) & 0xfff;
        let h = (g(VT_VCMAP) >> 12) & 0xfff;
        if (g(CTRLSTAT) >> 28) & 3 != 3
            || g(DOTCLOCK) & DOTCLOCK_RUN == 0
            || g(VT_XY) & VT_FREEZE != 0
            || w == 0
            || h == 0
            || g(VT_FLAGS) & 0xa == 0xa
            || (w as usize * h as usize * 4) > out.len()
        {
            return None;
        }
        let (w, h) = (w as usize, h as usize);

        // The normal planes as one byte stream (section 2.3 of the spec).
        let tile = g(FRM_SIZE_TILE);
        let rhs = (tile & 0x1f) as usize;
        let wt = ((tile >> 5) & 0xff) as usize;
        let bpp = 1usize << ((tile >> 13) & 3).min(2);
        let fb_h = (g(FRM_SIZE_PIXEL) >> 16) as usize;
        let per_row = wt + (rhs != 0) as usize;
        let line_bytes = wt * 512 + rhs * 32;
        let frm = g(FRM_CONTROL);
        let dma = frm & 1 != 0 && per_row != 0 && line_bytes != 0;
        // Each tile's offset in `mem`, resolved once per frame.
        let mut tiles: Vec<Option<usize>> = Vec::new();
        if dma {
            let list = frm & 0xffff_ffe0;
            let n = per_row * fb_h.div_ceil(128);
            tiles.reserve(n);
            for i in 0..n.min(65536) {
                let e = mem
                    .dma_offset(list + 2 * i as u32)
                    .filter(|&o| o + 1 < mem.len())
                    .map(|o| u16::from_be_bytes([mem[o], mem[o + 1]]))
                    .unwrap_or(0);
                tiles.push(if e == 0 { None } else { mem.dma_offset((e as u32) << 16).filter(|&o| o + 0x10000 <= mem.len()) });
            }
            *self.displayed.lock().unwrap() = (0..n.min(65536))
                .filter_map(|i| mem.dma_offset(list + 2 * i as u32).filter(|&o| o + 1 < mem.len()))
                .map(|o| u16::from_be_bytes([mem[o], mem[o + 1]]) as u32)
                .filter(|&e| e != 0)
                .collect();
        }
        let fetch = |s: usize| -> u32 {
            if !dma {
                return 0;
            }
            let (line, col) = (s / line_bytes, s % line_bytes);
            if line >= fb_h {
                return 0;
            }
            let Some(Some(base)) = tiles.get((line / 128) * per_row + col / 512) else { return 0 };
            let o = base + (line % 128) * 512 + col % 512;
            match bpp {
                1 => mem[o] as u32,
                2 => u16::from_be_bytes([mem[o], mem[o + 1]]) as u32,
                _ => u32::from_be_bytes([mem[o], mem[o + 1], mem[o + 2], mem[o + 3]]),
            }
        };

        // The overlay planes: 8-bit pixels over the normal ones, in their own
        // tile list (same width-in-tiles layout as the normal planes). Zero is
        // clear; anything else is colour map entry 0x1100 + the pixel. The X
        // server draws its pop-up menus here.
        let ovr = g(OVR_CONTROL);
        let ovr_tile = g(OVR_WIDTH_TILE);
        let ovr_rhs = (ovr_tile & 0x1f) as usize;
        let ovr_per_row = ((ovr_tile >> 5) & 0xff) as usize + (ovr_rhs != 0) as usize;
        let ovr_line = ((ovr_tile >> 5) & 0xff) as usize * 512 + ovr_rhs * 32;
        let ovr_dma = ovr & 1 != 0 && ovr_per_row != 0 && ovr_line != 0;
        let ovr_tiles: Vec<Option<usize>> = if ovr_dma {
            let list = ovr & 0xffff_ffe0;
            (0..(ovr_per_row * h.div_ceil(128)).min(4096))
                .map(|i| {
                    let e = mem
                        .dma_offset(list + 2 * i as u32)
                        .filter(|&o| o + 1 < mem.len())
                        .map(|o| u16::from_be_bytes([mem[o], mem[o + 1]]))
                        .unwrap_or(0);
                    if e == 0 { None } else { mem.dma_offset((e as u32) << 16).filter(|&o| o + 0x10000 <= mem.len()) }
                })
                .collect()
        } else {
            Vec::new()
        };
        let ovr_at = |x: usize, y: usize| -> u32 {
            if !ovr_dma || x >= ovr_line {
                return 0;
            }
            let Some(Some(base)) = ovr_tiles.get((y / 128) * ovr_per_row + x / 512) else { return 0 };
            mem[base + (y % 128) * 512 + x % 512] as u32
        };

        // Each pixel's WID (mode-table entry) comes from the DID tables when
        // DID DMA is on and the 32 WIDs differ; otherwise WID 0, which is
        // all the PROM and the BSD consoles use. IRIX's X server runs 8-bit
        // indexed windows (the colour index in a pixel's low byte) beside
        // 24-bit ones, and only the DIDs say which a pixel belongs to.
        let did = g(DID_CONTROL);
        let wids: Vec<u32> = (0..32).map(|i| g(MODE + 4 * i)).collect();
        let use_did = did & (1 << 16) != 0 && wids.iter().any(|&m| m != wids[0]);
        let mut line_did = vec![0u8; if use_did { w } else { 0 }];
        let table = (did & 0xffff) << 16;
        let rd16 = |a: u32| mem.dma_offset(a).filter(|&o| o + 1 < mem.len()).map(|o| u16::from_be_bytes([mem[o], mem[o + 1]])).unwrap_or(0);
        let rd32 = |a: u32| (rd16(a) as u32) << 16 | rd16(a + 2) as u32;
        let mut frame_i = 0u32;
        let cmap = |i: u32| g(CMAP + 4 * (i % 6144));
        let gmap = |v: u32, shift: u32| (g(GMAP + 4 * (v & 0xff)) >> shift) & 0xff;
        let crs_on = g(CRS_CTL) & 1 != 0;
        let crs = g(CRS_POS);
        let (cx, cy) = ((crs & 0xffff) as i16 as i32, (crs >> 16) as i16 as i32);

        for y in 0..h {
            if use_did {
                // Frame entry: 25:19 block, 18:11 offset, 10:0 last line. Its
                // line table (16-bit entries, 15:11 DID, 10:0 last x) sits in
                // one 512-byte block of the 64 KB table.
                let fe = rd32(table + 4 * frame_i.min(16383));
                let (block, off, yend) = ((fe >> 19) & 0x7f, (fe >> 11) & 0xff, fe & 0x7ff);
                let lt = table + 512 * block + 2 * off;
                let (mut x, mut k) = (0usize, 0u32);
                while x < w && k < 256 - off {
                    let le = rd16(lt + 2 * k);
                    let (d, xend) = ((le >> 11) as u8, (le & 0x7ff) as usize);
                    while x < w && x <= xend {
                        line_did[x] = d;
                        x += 1;
                    }
                    k += 1;
                }
                // Anything past the table's last span keeps that span's DID.
                let last = if x > 0 { line_did[x - 1] } else { 0 };
                line_did[x..].fill(last);
                if y as u32 >= yend {
                    frame_i += 1;
                }
            }
            for x in 0..w {
                let wid = if use_did { wids[line_did[x] as usize & 31] } else { wids[0] };
                let typ = (wid >> 2) & 7;
                let cm = (wid >> 5) & 0x1f;
                let gamma = wid & (1 << 10) == 0;
                let p = fetch((y * w + x) * bpp);
                // A type narrower than the frame buffer reads one half of the
                // word: buf 10 the upper half, otherwise the lower.
                let p = if typ <= 4 && bpp > 1 {
                    let half = bpp as u32 * 4;
                    let mask = (1u32 << half) - 1;
                    if wid & 3 == 2 { (p >> half) & mask } else { p & mask }
                } else {
                    p
                };
                let (mut rr, mut gg, mut bb) = match typ {
                    0 => {
                        let c = cmap((cm << 8) | (p & 0xff));
                        (c >> 24, (c >> 16) & 0xff, (c >> 8) & 0xff)
                    }
                    1 => {
                        let c = cmap(p & 0xfff);
                        (c >> 24, (c >> 16) & 0xff, (c >> 8) & 0xff)
                    }
                    2 => {
                        let (r3, g3, b2) = ((p >> 5) & 7, (p >> 2) & 7, p & 3);
                        ((r3 << 5) | (r3 << 2) | (r3 >> 1), (g3 << 5) | (g3 << 2) | (g3 >> 1), b2 * 0x55)
                    }
                    3 => (((p >> 8) & 0xf) * 0x11, ((p >> 4) & 0xf) * 0x11, (p & 0xf) * 0x11),
                    4 => {
                        let e = |v: u32| (v << 3) | (v >> 2);
                        (e((p >> 10) & 0x1f), e((p >> 5) & 0x1f), e(p & 0x1f))
                    }
                    _ => (p >> 24, (p >> 16) & 0xff, (p >> 8) & 0xff),
                };
                if typ >= 2 && cm != 0 {
                    rr = cmap((cm << 8) | rr) >> 24;
                    gg = (cmap((cm << 8) | gg) >> 16) & 0xff;
                    bb = (cmap((cm << 8) | bb) >> 8) & 0xff;
                }
                if gamma {
                    (rr, gg, bb) = (gmap(rr, 24), gmap(gg, 16), gmap(bb, 8));
                }
                let ov = ovr_at(x, y);
                if ov != 0 {
                    let c = cmap(0x1100 + ov);
                    (rr, gg, bb) = (c >> 24, (c >> 16) & 0xff, (c >> 8) & 0xff);
                }
                if crs_on {
                    let (dx, dy) = (x as i32 - cx, y as i32 - cy);
                    if (0..32).contains(&dx) && (0..32).contains(&dy) {
                        let word = g(CRS_GLYPH + 4 * (2 * dy as u32 + (dx as u32 / 16)));
                        let v = (word >> (30 - 2 * (dx as u32 % 16))) & 3;
                        if v != 0 {
                            let c = g(CRS_CMAP1 + 4 * (v - 1));
                            (rr, gg, bb) = (c >> 24, (c >> 16) & 0xff, (c >> 8) & 0xff);
                        }
                    }
                }
                let o = (y * w + x) * 4;
                out[o] = rr as u8;
                out[o + 1] = gg as u8;
                out[o + 2] = bb as u8;
                out[o + 3] = 255;
            }
        }
        Some((w as u32, h as u32))
    }
}

impl BusDevice for Gbe {
    fn read32(&self, addr: u32) -> BusRead32 {
        BusRead32::ok(self.read_reg(addr & 0xffffc))
    }
    fn read64(&self, addr: u32) -> BusRead64 {
        let a = addr & 0xffff8;
        BusRead64::ok((self.read_reg(a) as u64) << 32 | self.read_reg(a + 4) as u64)
    }
    fn write32(&self, addr: u32, val: u32) -> u32 {
        self.write_reg(addr & 0xffffc, val);
        BUS_OK
    }
    fn write64(&self, addr: u32, val: u64) -> u32 {
        let a = addr & 0xffff8;
        self.write_reg(a, (val >> 32) as u32);
        self.write_reg(a + 4, val as u32);
        BUS_OK
    }
}

/// CRIME rendering-engine register offsets from `0x1500_0000`.
pub mod re_reg {
    pub const IB_CONTROL: u32 = 0x0400;
    pub const TLB_A: u32 = 0x1000;
    pub const TLB_LINEAR_A: u32 = 0x1700;
    pub const BUFMODE_SRC: u32 = 0x2000;
    pub const BUFMODE_DST: u32 = 0x2008;
    pub const DRAWMODE: u32 = 0x2018;
    /// Clip mode (low four bits enable the rectangles) and the rectangles.
    pub const CLIP_MODE: u32 = 0x2010;
    pub const CLIP_RECT0: u32 = 0x2020;
    pub const WINOFFSET_SRC: u32 = 0x2050;
    pub const WINOFFSET_DST: u32 = 0x2058;
    pub const PRIMITIVE: u32 = 0x2060;
    pub const VERTEX_0: u32 = 0x2070;
    pub const VERTEX_1: u32 = 0x2074;
    pub const XFER_SRC: u32 = 0x20a0;
    /// The linear end's address steps, in bytes: from one pixel to the
    /// next, and from a row's last pixel to the next row's first.
    pub const XFER_STEP_X: u32 = 0x20a8;
    pub const XFER_STEP_ROW: u32 = 0x20ac;
    /// Linear destination byte address.
    pub const XFER_DST: u32 = 0x20b0;
    pub const STIPPLE_MODE: u32 = 0x20c0;
    pub const STIPPLE_PATTERN: u32 = 0x20c4;
    pub const FG: u32 = 0x20d0;
    /// Triangle vertices, 64 bits each: x float high, y float low.
    pub const TRI_V0: u32 = 0x2080;
    /// Shade colour R, G, B, A (8.12 fixed), then their gradients.
    pub const SHADE_R: u32 = 0x20e0;
    pub const SHADE_GRAD: u32 = 0x20f0;
    pub const BG: u32 = 0x20d8;
    pub const LOGIC_OP: u32 = 0x21b0;
    pub const COLOR_MASK: u32 = 0x21b8;
    pub const MTE_MODE: u32 = 0x3000;
    pub const MTE_BYTEMASK: u32 = 0x3008;
    pub const MTE_STIPPLE: u32 = 0x3010;
    pub const MTE_FG: u32 = 0x3018;
    pub const MTE_SRC0: u32 = 0x3020;
    pub const MTE_DST0: u32 = 0x3030;
    pub const MTE_DST1: u32 = 0x3038;
    pub const MTE_SRC_STEP: u32 = 0x3040;
    pub const MTE_DST_STEP: u32 = 0x3048;
    pub const STATUS: u32 = 0x4000;
}

use re_reg::*;

const RE_WORDS: usize = 0x6000 / 4;
/// Idle everywhere, interface buffer empty.
const RE_STATUS_IDLE: u32 = 0x1e00_0000;

mod draw {
    pub const PIXEL_XFER: u32 = 1 << 21;
    pub const LINE_STIPPLE: u32 = 1 << 19;
    pub const POLY_STIPPLE: u32 = 1 << 18;
    pub const OPAQUE_STIPPLE: u32 = 1 << 17;
    pub const LOGIC_OP: u32 = 1 << 9;
    pub const PLANE_MASK: u32 = 1 << 7;
}

pub struct CrimeRe {
    regs: Mutex<Vec<u32>>,
    /// Reads per register offset, for finding a driver spinning on one.
    pub hot: Mutex<BTreeMap<u32, u64>>,
    ram: Arc<Mutex<O2Memory>>,
    /// What the guest asked for that is not modelled, with counts, so a
    /// missing primitive shows up instead of drawing nothing silently.
    unhandled: Mutex<BTreeMap<String, u64>>,
    /// Primitives run, for the end-of-run report.
    pub drawn: std::sync::atomic::AtomicU64,
    /// Register writes as they happen, `(offset, value, width)`, kept while
    /// fewer than `trace_cap` (IRIS_IP32_RE_TRACE): the evidence for what a
    /// driver does with parts the model does not know yet.
    trace: Mutex<Vec<(u32, u64, u8)>>,
    trace_cap: usize,
}

/// A buffer the RE draws through: which TLB, and bytes per pixel word.
#[derive(Clone, Copy)]
struct Buf {
    tlb: u32,
    bd: u32,
}

impl CrimeRe {
    pub fn new(ram: Arc<Mutex<O2Memory>>) -> Self {
        // byteMask resets to all ones: NetBSD's MTE clears never write it.
        let mut regs = vec![0; RE_WORDS];
        regs[(MTE_BYTEMASK / 4) as usize] = 0xffff_ffff;
        Self {
            regs: Mutex::new(regs),
            hot: Mutex::new(BTreeMap::new()),
            ram,
            unhandled: Mutex::new(BTreeMap::new()),
            drawn: std::sync::atomic::AtomicU64::new(0),
            trace: Mutex::new(Vec::new()),
            trace_cap: std::env::var("IRIS_IP32_RE_TRACE").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        }
    }

    fn record(&self, off: u32, val: u64, width: u8) {
        if self.trace_cap == 0 {
            return;
        }
        let mut t = self.trace.lock().unwrap();
        if t.len() < self.trace_cap {
            t.push((off, val, width));
        }
    }

    /// The recorded writes, one per line: `w32 OFFSET VALUE` (offsets from
    /// the RE base, 0x1500_0000).
    pub fn trace_text(&self) -> String {
        let t = self.trace.lock().unwrap();
        let mut s = String::with_capacity(t.len() * 28);
        for &(off, val, w) in t.iter() {
            s.push_str(&format!("w{w} {off:05x} {val:0pad$x}\n", pad = w as usize / 4));
        }
        s
    }

    pub fn unhandled(&self) -> BTreeMap<String, u64> {
        self.unhandled.lock().unwrap().clone()
    }

    fn note(&self, what: String) {
        let mut u = self.unhandled.lock().unwrap();
        let n = u.entry(what.clone()).or_insert(0);
        *n += 1;
        if *n == 1 {
            eprintln!("crime-re: not modelled: {what}");
        }
    }

    fn read_reg(&self, off: u32) -> u32 {
        *self.hot.lock().unwrap().entry(off & !3).or_insert(0) += 1;
        match off & !3 {
            STATUS | 0x4004 => RE_STATUS_IDLE,
            IB_CONTROL | 0x0404 => 0,
            o => self.regs.lock().unwrap().get((o / 4) as usize).copied().unwrap_or(0),
        }
    }

    fn write_reg(&self, off: u32, val: u32) {
        let page = off & 0xf000;
        let start = (page == 0x2000 || page == 0x3000) && off & 0x800 != 0;
        let reg = if start { off & !0x800 } else { off };
        {
            let mut r = self.regs.lock().unwrap();
            if let Some(slot) = r.get_mut((reg / 4) as usize) {
                *slot = val;
            }
        }
        if start {
            if page == 0x2000 {
                self.pixel_pipe();
            } else {
                self.mte();
            }
        }
    }

    fn buf(&self, r: &[u32], mode: u32, what: &str) -> Option<Buf> {
        let b = (mode >> 10) & 7;
        let bd = 1 << ((mode >> 8) & 3).min(2);
        if b > 2 {
            self.note(format!("{what} buffer {b} (linear/other)"));
            return None;
        }
        let _ = r;
        Some(Buf { tlb: TLB_A + 0x200 * b, bd })
    }

    /// Byte offset in `mem` of pixel (x, y) of `buf`, through its tile TLB.
    fn locate(r: &[u32], mem: &O2Memory, buf: Buf, x: u32, y: u32) -> Option<usize> {
        let tw = 512 / buf.bd;
        let entry = (y / 128) * 16 + x / tw;
        if entry >= 256 {
            return None;
        }
        let word = buf.tlb + 8 * (entry / 4);
        let w64 = ((r[(word / 4) as usize] as u64) << 32) | r[(word / 4 + 1) as usize] as u64;
        let e = (w64 >> (48 - 16 * (entry % 4))) & 0xffff;
        if e & 0x8000 == 0 {
            return None;
        }
        let pa = (((e & 0x7fff) as u32) << 16) + (y % 128) * 512 + (x % tw) * buf.bd;
        mem.dma_offset(pa).filter(|&o| o + buf.bd as usize <= mem.len())
    }

    fn get_px(mem: &O2Memory, o: usize, bd: u32) -> u32 {
        match bd {
            1 => mem[o] as u32,
            2 => u16::from_be_bytes([mem[o], mem[o + 1]]) as u32,
            _ => u32::from_be_bytes([mem[o], mem[o + 1], mem[o + 2], mem[o + 3]]),
        }
    }

    /// A register colour as a pixel of `bd` bytes: its low bytes, wherever
    /// the pixel sits in its word. IRIX's X server writes an 8-bit index as
    /// is (FG 0x00000002); the BSDs repeat it in every byte, which reads
    /// the same either way.
    fn lane(v: u32, _o: usize, bd: u32) -> u32 {
        if bd >= 4 { v } else { v & ((1 << (8 * bd)) - 1) }
    }

    fn logic(op: u32, s: u32, d: u32) -> u32 {
        match op & 0xf {
            0 => 0,
            1 => s & d,
            2 => s & !d,
            3 => s,
            4 => !s & d,
            5 => d,
            6 => s ^ d,
            7 => s | d,
            8 => !(s | d),
            9 => !(s ^ d),
            10 => !d,
            11 => s | !d,
            12 => !s,
            13 => !s | d,
            14 => !(s & d),
            _ => u32::MAX,
        }
    }

    /// Write `src` to the pixel at `o`, through the logic op, plane mask and
    /// byte-lane enables.
    fn put(r: &[u32], mem: &mut O2Memory, o: usize, bd: u32, src: u32) {
        let dm = r[(DRAWMODE / 4) as usize];
        let old = Self::get_px(mem, o, bd);
        let mut v = if dm & draw::LOGIC_OP != 0 { Self::logic(r[(LOGIC_OP / 4) as usize], src, old) } else { src };
        if dm & draw::PLANE_MASK != 0 {
            let m = Self::lane(r[(COLOR_MASK / 4) as usize], o, bd);
            v = (v & m) | (old & !m);
        }
        let bytes = v.to_be_bytes();
        let bytes = &bytes[4 - bd as usize..];
        let lanes = (dm >> 3) & 0xf;
        if lanes == 0xf {
            mem.write(o, bytes);
        } else {
            for (k, b) in bytes.iter().enumerate() {
                let a = o + k;
                if lanes & (1 << (3 - (a & 3))) != 0 {
                    mem.write(a, &[*b]);
                }
            }
        }
    }

    fn xy(v: u32) -> (i32, i32) {
        ((v >> 16) as i32, (v & 0xffff) as i32)
    }

    /// Whether the clip rectangles let pixel (x, y) be drawn. CLIP_MODE's
    /// low four bits enable rectangles 0-3 (CLIP_RECT0 on, 64 bits each:
    /// x0, y0 high, x1, y1 low, inclusive); with any enabled, a pixel must
    /// lie in one of them. 4Dwm fills a minimized window's frame with the
    /// four rectangles around the icon picture enabled (mode 0x1ef), which
    /// keeps the fill off the picture it has just copied in.
    fn clip_ok(r: &[u32], x: i32, y: i32) -> bool {
        let mode = r[(CLIP_MODE / 4) as usize];
        if mode & 0xf == 0 {
            return true;
        }
        (0..4).any(|i| {
            if mode & (1 << i) == 0 {
                return false;
            }
            let (x0, y0) = Self::xy(r[((CLIP_RECT0 + 8 * i) / 4) as usize]);
            let (x1, y1) = Self::xy(r[((CLIP_RECT0 + 8 * i) / 4 + 1) as usize]);
            (x0..=x1).contains(&x) && (y0..=y1).contains(&y)
        })
    }

    /// A window offset: two signed 16-bit halves. IRIX's GL windows sit at
    /// offsets like (-3443, -3477) -- the window origin less a 4096 guard --
    /// which read unsigned put every triangle 62,000 pixels off screen.
    fn offset_xy(v: u32) -> (i32, i32) {
        ((v >> 16) as u16 as i16 as i32, v as u16 as i16 as i32)
    }

    /// Run the pixel pipe's current primitive.
    /// A 32-bit pixel moved between buffers whose pixel types (BufMode
    /// 7:4) differ: through RGBA. RGB keeps its colour in the low three
    /// bytes (0x00RRGGBB) and ABGR is RGBA reversed. IRIX's X server keeps
    /// pixmaps as RGB in linear B and the screen as RGBA; copying the word
    /// unchanged gave 0x00c0c0c0's grey back as 0x00c0c0 -- cyan.
    fn convert(v: u32, from_mode: u32, to_mode: u32, bd: u32) -> u32 {
        let (from, to) = ((from_mode >> 4) & 0xf, (to_mode >> 4) & 0xf);
        if from == to || bd != 4 {
            return v;
        }
        let rgba = match from {
            1 => (v << 8) | 0xff,
            3 => v.swap_bytes(),
            _ => v,
        };
        match to {
            1 => rgba >> 8,
            3 => rgba.swap_bytes(),
            _ => rgba,
        }
    }

    /// A rectangle pixel transfer with a linear buffer at either end: the
    /// X server keeps icons and saved screen areas in linear B and copies
    /// them in and out this way. The linear end starts at a byte address
    /// (XFER_SRC for a source, XFER_DST for a destination) for the
    /// rectangle's top-left pixel and moves XFER_STEP_X bytes per pixel and
    /// XFER_STEP_ROW from the end of one row to the start of the next. The
    /// login screen's copies out of a 16-pixel-wide pixmap all have steps 4
    /// and 4 + (16 - width) x 4: a 64-byte pitch whatever the width.
    fn xfer_linear(&self, r: &[u32]) {
        let g = |off: u32| r[(off / 4) as usize];
        let (sm, dmode) = (g(BUFMODE_SRC), g(BUFMODE_DST));
        let (sb, db) = ((sm >> 10) & 7, (dmode >> 10) & 7);
        let (sbd, dbd) = (1u32 << ((sm >> 8) & 3).min(2), 1u32 << ((dmode >> 8) & 3).min(2));
        let lin = |b: u32| b == 4 || b == 5;
        if (!lin(sb) && sb > 2) || (!lin(db) && db > 2) {
            self.note(format!("xfer buffers src {sb} dst {db}"));
            return;
        }
        let (ox, oy) = Self::offset_xy(g(WINOFFSET_DST));
        let (x0, y0) = Self::xy(g(VERTEX_0));
        let (x1, y1) = Self::xy(g(VERTEX_1));
        let (x0, y0, x1, y1) = (x0 + ox, y0 + oy, x1 + ox, y1 + oy);
        let (xa, xb) = (x0.min(x1).max(0), x0.max(x1).max(0));
        let (ya, yb) = (y0.min(y1).max(0), y0.max(y1).max(0));
        if xb - xa > 4096 || yb - ya > 4096 {
            self.note(format!("xfer rect {xa},{ya}..{xb},{yb}"));
            return;
        }
        let (step_x, step_row) = (g(XFER_STEP_X), g(XFER_STEP_ROW));
        let width = (xb - xa) as u32;
        // Offset of pixel (x, y) from the top-left one, along the linear end.
        let lin_off = |x: i32, y: i32| -> u32 {
            let row = width.wrapping_mul(step_x).wrapping_add(step_row);
            ((y - ya) as u32).wrapping_mul(row).wrapping_add(((x - xa) as u32).wrapping_mul(step_x))
        };
        let (sx0, sy0) = Self::xy(g(XFER_SRC));
        let src_tiled = Buf { tlb: TLB_A + 0x200 * sb.min(2), bd: sbd };
        let dst_tiled = Buf { tlb: TLB_A + 0x200 * db.min(2), bd: dbd };
        let mut ram = self.ram.lock().unwrap();
        let mem: &mut O2Memory = &mut ram;
        let src_at = |mem: &O2Memory, x: i32, y: i32| -> Option<usize> {
            if lin(sb) {
                let a = g(XFER_SRC).wrapping_add(lin_off(x, y));
                Self::linear(r, mem, sb - 4, a)
            } else {
                let (px, py) = (x + sx0 - x0, y + sy0 - y0);
                if px < 0 || py < 0 {
                    return None;
                }
                Self::locate(r, mem, src_tiled, px as u32, py as u32)
            }
        };
        let dst_at = |mem: &O2Memory, x: i32, y: i32| -> Option<usize> {
            if lin(db) {
                let a = g(XFER_DST).wrapping_add(lin_off(x, y));
                Self::linear(r, mem, db - 4, a)
            } else {
                Self::locate(r, mem, dst_tiled, x as u32, y as u32)
            }
        };
        // Read the whole source first, as the tiled copy does.
        let mut tmp = Vec::with_capacity(((xb - xa + 1) * (yb - ya + 1)) as usize);
        for y in ya..=yb {
            for x in xa..=xb {
                let v = src_at(mem, x, y).map(|o| Self::get_px(mem, o, sbd)).unwrap_or(0);
                tmp.push(Self::convert(v, sm, dmode, sbd.min(dbd)));
            }
        }
        let mut it = tmp.into_iter();
        for y in ya..=yb {
            for x in xa..=xb {
                let v = it.next().unwrap_or(0);
                if !Self::clip_ok(r, x, y) {
                    continue;
                }
                if let Some(o) = dst_at(mem, x, y) {
                    Self::put(r, mem, o, dbd, v);
                }
            }
        }
    }

    fn pixel_pipe(&self) {
        self.drawn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let r = self.regs.lock().unwrap().clone();
        let g = |off: u32| r[(off / 4) as usize];
        let dm = g(DRAWMODE);
        let prim = g(PRIMITIVE);
        if dm & draw::PIXEL_XFER != 0 && (prim >> 24) & 3 == 3 {
            let (sb, db) = ((g(BUFMODE_SRC) >> 10) & 7, (g(BUFMODE_DST) >> 10) & 7);
            if matches!(sb, 4 | 5) || matches!(db, 4 | 5) {
                self.xfer_linear(&r);
                return;
            }
        }
        let Some(dst) = self.buf(&r, g(BUFMODE_DST), "dst") else { return };
        let (ox, oy) = Self::offset_xy(g(WINOFFSET_DST));
        let (x0, y0) = Self::xy(g(VERTEX_0));
        let (x1, y1) = Self::xy(g(VERTEX_1));
        let (x0, y0, x1, y1) = (x0 + ox, y0 + oy, x1 + ox, y1 + oy);
        let fg = g(FG);
        let bg = g(BG);
        let smode = g(STIPPLE_MODE);
        let (sstart, smax) = ((smode >> 24) & 0x1f, (smode >> 16) & 0x1f);
        let pattern = g(STIPPLE_PATTERN);
        let stipple = |k: u32| -> Option<bool> {
            let span = smax + 1;
            let idx = (sstart + k) % span.max(1);
            Some((pattern >> (31 - idx)) & 1 != 0)
        };
        let mut ram = self.ram.lock().unwrap();
        let mem: &mut O2Memory = &mut ram;

        // Where each destination pixel's value comes from.
        let colour_at = |mem: &O2Memory, o: usize, k: u32, stippled: bool| -> Option<u32> {
            if stippled {
                match stipple(k) {
                    Some(true) => Some(Self::lane(fg, o, dst.bd)),
                    _ if dm & draw::OPAQUE_STIPPLE != 0 => Some(Self::lane(bg, o, dst.bd)),
                    _ => None,
                }
            } else {
                let _ = mem;
                Some(Self::lane(fg, o, dst.bd))
            }
        };

        match (prim >> 24) & 3 {
            // Point.
            0 => {
                if !Self::clip_ok(&r, x0, y0) {
                    return;
                }
                if let Some(o) = Self::locate(&r, mem, dst, x0.max(0) as u32, y0.max(0) as u32) {
                    let v = Self::lane(fg, o, dst.bd);
                    Self::put(&r, mem, o, dst.bd, v);
                }
            }
            // Line, Bresenham, the end pixel unless told to skip it.
            1 => {
                let stippled = dm & draw::LINE_STIPPLE != 0;
                let skip_last = prim & (1 << 18) != 0;
                let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
                let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
                let (mut x, mut y, mut err, mut k) = (x0, y0, dx + dy, 0u32);
                loop {
                    let last = x == x1 && y == y1;
                    if !(last && skip_last) && x >= 0 && y >= 0 && Self::clip_ok(&r, x, y) {
                        if let Some(o) = Self::locate(&r, mem, dst, x as u32, y as u32) {
                            if let Some(v) = colour_at(mem, o, k, stippled) {
                                Self::put(&r, mem, o, dst.bd, v);
                            }
                        }
                    }
                    if last || k > 8192 {
                        break;
                    }
                    k += 1;
                    let e2 = 2 * err;
                    if e2 >= dy {
                        err += dy;
                        x += sx;
                    }
                    if e2 <= dx {
                        err += dx;
                        y += sy;
                    }
                }
            }
            // Rectangle: both corners inclusive.
            3 => {
                let (xa, xb) = (x0.min(x1).max(0) as u32, x0.max(x1).max(0) as u32);
                let (ya, yb) = (y0.min(y1).max(0) as u32, y0.max(y1).max(0) as u32);
                if (xb - xa) > 4096 || (yb - ya) > 4096 {
                    self.note(format!("rect {xa},{ya}..{xb},{yb}"));
                    return;
                }
                if dm & draw::PIXEL_XFER != 0 {
                    // A copy, read whole before any write, so overlaps come
                    // out as the drivers' traversal order intends.
                    let Some(src) = self.buf(&r, g(BUFMODE_SRC), "src") else { return };
                    let (sx0, sy0) = Self::xy(g(XFER_SRC));
                    let (dxs, dys) = (sx0 - x0, sy0 - y0);
                    let mut tmp = Vec::with_capacity(((xb - xa + 1) * (yb - ya + 1)) as usize);
                    for y in ya..=yb {
                        for x in xa..=xb {
                            let (px, py) = (x as i32 + dxs, y as i32 + dys);
                            let v = if px >= 0 && py >= 0 {
                                Self::locate(&r, mem, src, px as u32, py as u32).map(|o| Self::get_px(mem, o, src.bd)).unwrap_or(0)
                            } else {
                                0
                            };
                            tmp.push(Self::convert(v, g(BUFMODE_SRC), g(BUFMODE_DST), src.bd.min(dst.bd)));
                        }
                    }
                    let mut it = tmp.into_iter();
                    for y in ya..=yb {
                        for x in xa..=xb {
                            let v = it.next().unwrap_or(0);
                            if !Self::clip_ok(&r, x as i32, y as i32) {
                                continue;
                            }
                            if let Some(o) = Self::locate(&r, mem, dst, x, y) {
                                Self::put(&r, mem, o, dst.bd, v);
                            }
                        }
                    }
                } else {
                    let stippled = dm & draw::POLY_STIPPLE != 0;
                    for y in ya..=yb {
                        for (k, x) in (xa..=xb).enumerate() {
                            if !Self::clip_ok(&r, x as i32, y as i32) {
                                continue;
                            }
                            if let Some(o) = Self::locate(&r, mem, dst, x, y) {
                                if let Some(v) = colour_at(mem, o, k as u32, stippled) {
                                    Self::put(&r, mem, o, dst.bd, v);
                                }
                            }
                        }
                    }
                }
            }
            // Triangle: three float vertices at 0x2080/0x2088/0x2090 (x in
            // the high word, y in the low), biased by 2^17 so the mantissa
            // holds 1/64-pixel fixed point, plus the signed window offset.
            // Colour from the shade registers, 8.12 fixed RGBA. IRIX's
            // desktop fills its icon backgrounds and vector icons this way,
            // flat-shaded and opaque.
            2 => {
                let fv = |off: u32| -> (f64, f64) {
                    let f = |w: u32| f32::from_bits(w) as f64 - 131072.0;
                    (f(g(off)) + ox as f64, f(g(off + 4)) + oy as f64)
                };
                let v = [fv(TRI_V0), fv(TRI_V0 + 8), fv(TRI_V0 + 16)];
                if (0..8).any(|i| g(SHADE_GRAD + 4 * i) != 0) {
                    self.note("gouraud triangle (drawn flat)".into());
                }
                let ch = |off: u32| (g(off) >> 12) & 0xff;
                let (cr, cg, cb, ca) = (ch(SHADE_R), ch(SHADE_R + 4), ch(SHADE_R + 8), ch(SHADE_R + 12));
                if dm & (1 << 10) != 0 && ca != 0xff {
                    self.note("blended triangle (drawn opaque)".into());
                }
                let rgba = (cr << 24) | (cg << 16) | (cb << 8) | ca;
                let dmode = g(BUFMODE_DST);
                // Double-pix (BufMode bit 1) with 16-bit pixels (3:2 = 1): a
                // double-buffered GL window keeps front and back in the two
                // halves of each 32-bit word, bit 0 choosing the half (0 the
                // low one, which IRIX's WID for it, 0x11, shows). The 16-bit
                // pixel is A1R5G5B5, as GBE decodes it.
                let half = dmode & 2 != 0 && (dmode >> 2) & 3 == 1 && dst.bd == 4;
                let colour = if half {
                    let c5 = |c: u32| c >> 3;
                    (u32::from(ca >= 0x80) << 15) | (c5(cr) << 10) | (c5(cg) << 5) | c5(cb)
                } else {
                    match (dmode >> 4) & 0xf {
                        0 => cr,
                        1 => rgba >> 8,
                        3 => rgba.swap_bytes(),
                        _ => rgba,
                    }
                };
                let half_off = if dmode & 1 == 0 { 2 } else { 0 };
                // Signed area, so the edge functions agree in sign inside.
                let area = (v[1].0 - v[0].0) * (v[2].1 - v[0].1) - (v[1].1 - v[0].1) * (v[2].0 - v[0].0);
                if area == 0.0 {
                    return;
                }
                let s = area.signum();
                let edge = |a: (f64, f64), b: (f64, f64), px: f64, py: f64| s * ((b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (px - a.0));
                // Top-left rule: a pixel centre exactly on an edge belongs to
                // the triangle only for a top or left edge.
                let top_left = |a: (f64, f64), b: (f64, f64)| {
                    let (dx, dy) = (s * (b.0 - a.0), s * (b.1 - a.1));
                    (dy == 0.0 && dx < 0.0) || dy > 0.0
                };
                let xmin = v.iter().map(|p| p.0).fold(f64::MAX, f64::min).floor().max(0.0) as i32;
                let xmax = v.iter().map(|p| p.0).fold(f64::MIN, f64::max).ceil().min(4095.0) as i32;
                let ymin = v.iter().map(|p| p.1).fold(f64::MAX, f64::min).floor().max(0.0) as i32;
                let ymax = v.iter().map(|p| p.1).fold(f64::MIN, f64::max).ceil().min(4095.0) as i32;
                let edges = [(v[0], v[1]), (v[1], v[2]), (v[2], v[0])];
                for y in ymin..=ymax {
                    for x in xmin..=xmax {
                        let (px, py) = (x as f64, y as f64);
                        let inside = edges.iter().all(|&(a, b)| {
                            let e = edge(a, b, px, py);
                            e > 0.0 || (e == 0.0 && top_left(a, b))
                        });
                        if inside && Self::clip_ok(&r, x, y) {
                            if let Some(o) = Self::locate(&r, mem, dst, x as u32, y as u32) {
                                if half {
                                    Self::put(&r, mem, o + half_off, 2, colour);
                                } else {
                                    Self::put(&r, mem, o, dst.bd, colour);
                                }
                            }
                        }
                    }
                }
            }
            _ => self.note("primitive type 3?".into()),
        }
    }

    /// Run the memory transfer engine's current transfer (NetBSD's clears
    /// and scrolls).
    /// RAM offset of byte `addr` of linear buffer `which` (0 = A, 1 = B).
    ///
    /// A linear buffer is 32 pages of 4 KB: entry `(addr >> 12) & 31`, kept
    /// in pairs, the even page in a word's high half and the odd one in its
    /// low half. An entry is bit 31 valid and the page number below it, so
    /// the byte is at `page << 12 | addr & 0xfff`. IRIX maps a buffer onto
    /// the pages it is about to clear or copy, with the page number equal to
    /// the address it then hands the MTE.
    fn linear(r: &[u32], mem: &O2Memory, which: u32, addr: u32) -> Option<usize> {
        let idx = (addr >> 12) & 31;
        let word = TLB_LINEAR_A + 0x80 * which + 8 * (idx / 2);
        let e = r[(word / 4 + (idx & 1)) as usize];
        if e & (1 << 31) == 0 {
            return None;
        }
        mem.dma_offset(((e & 0x000f_ffff) << 12) | (addr & 0xfff))
    }

    /// An MTE clear or copy whose buffers are linear: DST0..=DST1 (and SRC0
    /// on) are byte addresses, not x/y. The kernel zeroes and copies pages
    /// this way; doing nothing left them holding whatever was there before.
    fn mte_linear(&self, r: &[u32], copy: bool, sbuf: u32, dbuf: u32) {
        let g = |off: u32| r[(off / 4) as usize];
        let (d0, d1) = (g(MTE_DST0), g(MTE_DST1));
        if d1 < d0 || d1 - d0 >= 32 * 4096 {
            self.note(format!("mte linear span {d0:#x}..{d1:#x}"));
            return;
        }
        let s0 = g(MTE_SRC0);
        let fg = g(MTE_FG);
        let mask = g(MTE_BYTEMASK);
        let mut ram = self.ram.lock().unwrap();
        let mem: &mut O2Memory = &mut ram;
        for a in d0..=d1 {
            let lane = 24 - 8 * (a & 3);
            if (mask >> (31 - (a & 31))) & 1 == 0 {
                continue;
            }
            let Some(o) = Self::linear(r, mem, dbuf - 4, a) else { continue };
            let b = if copy {
                match Self::linear(r, mem, sbuf - 4, s0.wrapping_add(a - d0)) {
                    Some(so) => mem[so],
                    None => continue,
                }
            } else {
                (fg >> lane) as u8
            };
            mem.write(o, &[b]);
        }
    }

    fn mte(&self) {
        self.drawn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let r = self.regs.lock().unwrap().clone();
        let g = |off: u32| r[(off / 4) as usize];
        let mode = g(MTE_MODE);
        let copy = mode & (1 << 11) != 0;
        let dbuf = (mode >> 2) & 7;
        let sbuf = (mode >> 5) & 7;
        let linear = |b: u32| b == 4 || b == 5;
        if linear(dbuf) && (!copy || linear(sbuf)) {
            self.mte_linear(&r, copy, sbuf, dbuf);
            return;
        }
        if dbuf > 2 || (copy && sbuf > 2) {
            self.note(format!("mte buffers src {sbuf} dst {dbuf}"));
            return;
        }
        // MTE x is in bytes: one byte per "pixel" through the tile TLB.
        let (dst, src) = (Buf { tlb: TLB_A + 0x200 * dbuf, bd: 1 }, Buf { tlb: TLB_A + 0x200 * sbuf, bd: 1 });
        let (dx0, dy0) = Self::xy(g(MTE_DST0));
        let (dx1, dy1) = Self::xy(g(MTE_DST1));
        let (sx0, sy0) = Self::xy(g(MTE_SRC0));
        let dstep = g(MTE_DST_STEP) as i32;
        let sstep = g(MTE_SRC_STEP) as i32;
        let (dstep, sstep) = (if dstep < 0 { -1 } else { 1 }, if sstep < 0 { -1 } else { 1 });
        let rows = (dy1 - dy0).unsigned_abs() + 1;
        let (xa, xb) = (dx0.min(dx1).max(0), dx0.max(dx1).max(0));
        if rows > 4096 || xb - xa > 8192 {
            self.note(format!("mte span {xa}..{xb} x {rows}"));
            return;
        }
        let fg = g(MTE_FG);
        let bytemask = g(MTE_BYTEMASK);
        let stippled = mode & (1 << 10) != 0;
        let stipple = g(MTE_STIPPLE);
        let bpp = 1u32 << ((mode >> 8) & 3).min(2);
        let mut ram = self.ram.lock().unwrap();
        let mem: &mut O2Memory = &mut ram;
        for i in 0..rows as i32 {
            let dy = dy0 + i * dstep;
            if dy < 0 {
                continue;
            }
            if copy {
                let sy = sy0 + i * sstep;
                let row: Vec<u8> = (xa..=xb)
                    .map(|x| {
                        let sx = sx0 + (x - xa);
                        if sx < 0 || sy < 0 {
                            return 0;
                        }
                        Self::locate(&r, mem, src, sx as u32, sy as u32).map(|o| mem[o]).unwrap_or(0)
                    })
                    .collect();
                for (k, x) in (xa..=xb).enumerate() {
                    if let Some(o) = Self::locate(&r, mem, dst, x as u32, dy as u32) {
                        mem.write(o, &[row[k]]);
                    }
                }
            } else {
                for x in xa..=xb {
                    // A stippled clear writes only the pixels whose mask bit
                    // is set: one bit per pixel, most significant first,
                    // counted from the 32-byte block holding the row's first
                    // byte (eight pixels at 32 bpp, 32 at 8 bpp). IRIX's X
                    // server draws text a row and a mask at a time; replaying
                    // its recorded draws, only this count spells the login
                    // screen's labels (from the row start, letters that do
                    // not begin on a block boundary come out shifted).
                    if stippled {
                        let k = (x - (xa & !31)) as u32 / bpp;
                        if k >= 32 || (stipple >> (31 - k)) & 1 == 0 {
                            continue;
                        }
                    }
                    // byteMask: one bit per byte of a 32-byte block, most
                    // significant first. The kernel's page clears pass all
                    // ones; IRIX's X server passes 0x11111111 to clear only
                    // the low byte of each 32-bit pixel, and writing the
                    // whole pixel instead blacked out what it had drawn.
                    if (bytemask >> (31 - (x as u32 & 31))) & 1 == 0 {
                        continue;
                    }
                    if let Some(o) = Self::locate(&r, mem, dst, x as u32, dy as u32) {
                        let b = (fg >> (24 - 8 * (o as u32 & 3))) as u8;
                        mem.write(o, &[b]);
                    }
                }
            }
        }
    }
}

impl BusDevice for CrimeRe {
    fn read32(&self, addr: u32) -> BusRead32 {
        BusRead32::ok(self.read_reg(addr & 0x7ffc))
    }
    fn read64(&self, addr: u32) -> BusRead64 {
        let a = addr & 0x7ff8;
        if a == STATUS {
            return BusRead64::ok(RE_STATUS_IDLE as u64);
        }
        BusRead64::ok((self.read_reg(a) as u64) << 32 | self.read_reg(a + 4) as u64)
    }
    fn write32(&self, addr: u32, val: u32) -> u32 {
        self.record(addr & 0x7ffc, val as u64, 32);
        self.write_reg(addr & 0x7ffc, val);
        BUS_OK
    }
    fn write64(&self, addr: u32, val: u64) -> u32 {
        // Both halves land before a start acts, so a 64-bit write to a start
        // address (vertices 0 and 1 at once) draws with both.
        let a = addr & 0x7ff8;
        self.record(a, val, 64);
        let page = a & 0xf000;
        let start = (page == 0x2000 || page == 0x3000) && a & 0x800 != 0;
        let base = if start { a & !0x800 } else { a };
        {
            let mut r = self.regs.lock().unwrap();
            if let Some(s) = r.get_mut((base / 4) as usize) {
                *s = (val >> 32) as u32;
            }
            if let Some(s) = r.get_mut((base / 4 + 1) as usize) {
                *s = val as u32;
            }
        }
        if start {
            if page == 0x2000 {
                self.pixel_pipe();
            } else {
                self.mte();
            }
        }
        BUS_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gbe() -> Gbe {
        let t = Arc::new(std::sync::atomic::AtomicU64::new(0));
        Gbe::new(Arc::new(move || t.load(std::sync::atomic::Ordering::Relaxed)))
    }

    #[test]
    fn the_polls_the_prom_and_the_bsds_run_terminate() {
        let g = gbe();
        assert_eq!(g.read32(GBE_CTRLSTAT_ADDR).data & 0xf, 1, "chip id 1");
        assert_ne!(g.read32(GBE_CTRLSTAT_ADDR).data & (1 << 12), 0, "board revision strap");
        assert_eq!(g.read32(0x1600_0000 + CM_FIFO).data, 0);
        g.write32(0x1600_0000 + FRM_CONTROL_SHADOW, 0x00d6_0001);
        assert_eq!(g.read32(0x1600_0000 + FRM_CONTROL).data, 0x00d6_0001, "live follows shadow exactly");
        g.write32(0x1600_0000 + DOTCLOCK, 0x0010_020f);
        assert_ne!(g.read32(0x1600_0000 + DOTCLOCK).data & DOTCLOCK_RUN, 0);
        assert_eq!(g.read32(0x1600_0000 + VT_XY).data, VT_FREEZE, "frozen at reset");
        g.write32(0x1600_0000 + VT_XYMAX, 0x0042_9690);
        g.write32(0x1600_0000 + VT_XY, 0);
        // The PROM's wait for blanking: x in 1280..=1480 or y in 1024..=1044.
        let mut seen = false;
        for _ in 0..100_000 {
            let xy = g.read32(0x1600_0000 + VT_XY).data;
            let (x, y) = (xy & 0xfff, (xy >> 12) & 0xfff);
            if (1280..=1480).contains(&x) || (1024..=1044).contains(&y) {
                seen = true;
                break;
            }
        }
        assert!(seen, "the beam reaches blanking with guest time standing still");
    }

    const GBE_CTRLSTAT_ADDR: u32 = 0x1600_0000;

    /// Replay a recorded register trace (IRIS_IP32_RE_TRACE, saved with the
    /// control port's `retrace`) into a fresh RE and save what TLB A shows
    /// at 32 bpp as a PNG: RE_REPLAY=trace RE_REPLAY_PNG=out.png. CPU writes
    /// to the frame buffer are not in the trace, so only drawing shows.
    #[test]
    #[ignore]
    fn replay_a_recorded_trace() {
        let Ok(path) = std::env::var("RE_REPLAY") else { return };
        let out = std::env::var("RE_REPLAY_PNG").unwrap_or("replay.png".into());
        // 256 MB as two 128 MB banks, placed the way the PROM places them.
        let mut m = O2Memory::from_kits(&[128, 128]).unwrap();
        m.set_ctrl(0, 0x100);
        m.set_ctrl(2, 0x100 | 4);
        let ram = Arc::new(Mutex::new(m));
        let re = CrimeRe::new(ram.clone());
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let mut f = line.split_whitespace();
            let (w, off, val) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
            let (off, val) = (u32::from_str_radix(off, 16).unwrap(), u64::from_str_radix(val, 16).unwrap());
            if w == "w64" {
                re.write64(0x1500_0000 + off, val);
            } else {
                re.write32(0x1500_0000 + off, val as u32);
            }
        }
        let r = re.regs.lock().unwrap().clone();
        let mem = ram.lock().unwrap();
        let (w, h) = (1280u32, 1024u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        // RE_REPLAY_BUF=1 or 2: an 8-bit buffer through TLB B or C (the
        // overlay is C) instead, each index as its own grey.
        let eight: u32 = std::env::var("RE_REPLAY_BUF").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if eight != 0 {
                    if let Some(o) = CrimeRe::locate(&r, &mem, Buf { tlb: TLB_A + 0x200 * eight, bd: 1 }, x, y) {
                        let v = mem[o].wrapping_mul(85);
                        rgba[i..i + 4].copy_from_slice(&[v, v, v, 255]);
                    }
                } else if let Some(o) = CrimeRe::locate(&r, &mem, Buf { tlb: TLB_A, bd: 4 }, x, y) {
                    rgba[i..i + 3].copy_from_slice(&mem[o..o + 3]);
                    rgba[i + 3] = 255;
                }
            }
        }
        crate::ip32_screen::save_png(&out, &rgba, w, h).unwrap();
        // RE_REPLAY_PX="x,y x,y": the raw 32-bit words there.
        for xy in std::env::var("RE_REPLAY_PX").unwrap_or_default().split_whitespace() {
            let (x, y) = xy.split_once(',').unwrap();
            let (x, y) = (x.parse::<u32>().unwrap(), y.parse::<u32>().unwrap());
            if let Some(o) = CrimeRe::locate(&r, &mem, Buf { tlb: TLB_A, bd: 4 }, x, y) {
                eprintln!("replay px {x},{y} = {:02x}{:02x}{:02x}{:02x}", mem[o], mem[o + 1], mem[o + 2], mem[o + 3]);
            }
        }
    }

    /// 4Dwm's minimized-window icon: an 8-bit pixmap in linear B copied to
    /// the 8-bit root window (32-bit words, index in the low byte), with
    /// the registers IRIX used.
    #[test]
    fn an_8bit_pixmap_copies_out_of_linear_b_into_the_root_window() {
        let ram = Arc::new(Mutex::new(O2Memory::single(32 << 20)));
        let re = CrimeRe::new(ram.clone());
        let base = 0x1500_0000;
        // Screen: TLB A entry 0 at 0x0100_0000. Pixmap page at 0x0050_0000.
        re.write64(base + TLB_A, (0x8000u64 | 0x100) << 48);
        let src = 0x1038_fb60u32;
        let page = 0x0050_0000u32;
        let idx = (src >> 12) & 31;
        re.write64(base + TLB_LINEAR_A + 0x80 + 8 * (idx / 2), if idx & 1 == 1 { 0x8000_0000 | (page >> 12) as u64 } else { (0x8000_0000 | (page >> 12) as u64) << 32 });
        for i in 0..88u32 {
            ram.lock().unwrap().write((page | (src & 0xfff)) as usize + i as usize, &[0x21]);
        }
        re.write32(base + BUFMODE_SRC, 0x1400);
        re.write32(base + BUFMODE_DST, 0x8000_0200);
        re.write32(base + DRAWMODE, 0x0020_0208);
        re.write32(base + LOGIC_OP, 3);
        re.write32(base + PRIMITIVE, 0x0302_0000);
        re.write32(base + VERTEX_0, (10 << 16) | 0);
        re.write32(base + VERTEX_1, (20 << 16) | 0);
        re.write32(base + XFER_STEP_X, 1);
        re.write32(base + XFER_STEP_ROW, 4);
        re.write32(base + (XFER_SRC | 0x800), src);
        let m = ram.lock().unwrap();
        // Pixel (10, 0): word 10 of the first tile row.
        assert_eq!(&m[0x0100_0000 + 40..0x0100_0000 + 44], &[0, 0, 0, 0x21]);
    }

    #[test]
    fn rgb_pixmaps_become_rgba_on_the_way_to_the_screen() {
        // BufMode pixel types: 1 RGB, 2 RGBA, 3 ABGR, in bits 7:4.
        assert_eq!(CrimeRe::convert(0x00c0_c0c0, 0x1618, 0x228, 4), 0xc0c0_c0ff);
        assert_eq!(CrimeRe::convert(0x1122_3344, 0x228, 0x1618, 4), 0x0011_2233);
        assert_eq!(CrimeRe::convert(0x1122_3344, 0x228, 0x238, 4), 0x4433_2211);
        assert_eq!(CrimeRe::convert(0x1122_3344, 0x228, 0x228, 4), 0x1122_3344);
    }

    /// IRIX zeroes pages with the MTE: linear A mapped onto the pages in
    /// pairs, then a clear from DST0 to DST1 with the fill colour.
    #[test]
    fn the_mte_clears_pages_through_linear_a_as_irix_asks() {
        let ram = Arc::new(Mutex::new(O2Memory::single(32 << 20)));
        ram.lock().unwrap().write(0x0011_3fff, &[0x55; 0x2_0002]);
        let re = CrimeRe::new(ram.clone());
        let base = 0x1500_0000;
        let addr = 0x0011_4000u32;
        let page = addr >> 12;
        for i in 0..16u32 {
            let p = page + 2 * i;
            let pair = ((1u64 << 31 | p as u64) << 32) | (1u64 << 31 | (p + 1) as u64);
            re.write64(base + TLB_LINEAR_A + 8 * ((p & 31) >> 1), pair);
        }
        re.write32(base + MTE_DST0, addr);
        re.write32(base + MTE_DST1, addr + 32 * 4096 - 1);
        re.write32(base + MTE_FG, 0);
        re.write32(base + MTE_BYTEMASK, 0xffff_ffff);
        re.write32(base + (MTE_MODE | 0x800), 0x11);
        let m = ram.lock().unwrap();
        assert!(m[addr as usize..(addr + 32 * 4096) as usize].iter().all(|&b| b == 0), "all 128 KB cleared");
        assert_eq!(m[addr as usize - 1], 0x55, "the byte before is untouched");
        assert_eq!(m[(addr + 32 * 4096) as usize], 0x55, "the byte after is untouched");
    }

    /// IRIX's mode set writes the overlay shadow with DMA on and then waits
    /// for the live register to read the old state; it must see it until
    /// vertical blanking, and the new value after.
    #[test]
    fn a_running_beam_latches_shadow_controls_at_the_next_frame() {
        let t = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let tc = t.clone();
        let g = Gbe::new(Arc::new(move || tc.load(std::sync::atomic::Ordering::Relaxed)));
        let base = 0x1600_0000;
        g.write32(base + DOTCLOCK, 0x0010_020f);
        g.write32(base + VT_XYMAX, 0x0042_9690);
        g.write32(base + VT_XY, 0);
        g.write32(base + OVR_CONTROL_SHADOW, 0x0035_b9c1);
        assert_eq!(g.read32(base + OVR_CONTROL).data & 1, 0, "the old state until blanking");
        // A frame at 108 MHz is about 15 ms of guest time.
        t.store(40_000, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(g.read32(base + OVR_CONTROL).data, 0x0035_b9c1, "the new value after it");
    }

    /// Fill a rectangle through TLB A, then scan it out through a tile list.
    #[test]
    fn a_filled_rectangle_comes_out_of_scanout_in_its_cmap_colour() {
        let ram = Arc::new(Mutex::new(O2Memory::single(32 << 20)));
        let re = CrimeRe::new(ram.clone());
        let g = gbe();
        // One 64 KB tile at 0x0010_0000, so a 512 x 128 screen at 8 bpp.
        let tile = 0x0010_0000u32;
        let list = 0x0020_0000u32;
        ram.lock().unwrap().write(list as usize, &((tile >> 16) as u16).to_be_bytes());
        // TLB A entry 0: valid, tile address >> 16.
        re.write64(0x1500_0000 + TLB_A, (0x8000u64 | (tile >> 16) as u64) << 48);
        re.write32(0x1500_0000 + BUFMODE_DST, 0);
        re.write32(0x1500_0000 + DRAWMODE, 0xf << 3);
        re.write32(0x1500_0000 + PRIMITIVE, 3 << 24);
        re.write32(0x1500_0000 + FG, 0x0505_0505);
        re.write32(0x1500_0000 + VERTEX_0, (10 << 16) | 20);
        re.write32(0x1500_0000 + (VERTEX_1 | 0x800), (19 << 16) | 29);
        {
            let m = ram.lock().unwrap();
            assert_eq!(m[(tile + 20 * 512 + 10) as usize], 5);
            assert_eq!(m[(tile + 29 * 512 + 19) as usize], 5, "inclusive corner");
            assert_eq!(m[(tile + 29 * 512 + 20) as usize], 0);
        }
        // GBE: 512 x 128, one whole tile per row, 8 bpp, WID 0 = I8, gamma bypass.
        for (off, v) in [
            (CTRLSTAT, 0x300a_a000u32),
            (DOTCLOCK, 0x0010_020f),
            (VT_XYMAX, 0x0042_9690),
            // Before the beam runs, so the copy to live is immediate: guest
            // time stands still here and no blanking would come.
            (FRM_CONTROL_SHADOW, list | 1),
            (VT_XY, 0),
            (VT_HCMAP, 512 << 12),
            (VT_VCMAP, 128 << 12),
            (FRM_SIZE_TILE, 1 << 5),
            (FRM_SIZE_PIXEL, 128 << 16),
            (MODE, 0x403),
            (CMAP + 4 * 5, 0x11223300),
        ] {
            g.write32(0x1600_0000 + off, v);
        }
        let mut out = vec![0u8; 2048 * 2048 * 4];
        let (w, h) = g.render(&ram.lock().unwrap(), &mut out).expect("display on");
        assert_eq!((w, h), (512, 128));
        let px = |x: usize, y: usize| &out[(y * 512 + x) * 4..(y * 512 + x) * 4 + 3];
        assert_eq!(px(10, 20), [0x11, 0x22, 0x33]);
        assert_eq!(px(9, 20), [0, 0, 0]);
    }

    /// IRIX draws menu text into the 8-bit overlay as stippled lines with
    /// FG 0x00000002: every set bit is index 2, whatever its byte in the word.
    #[test]
    fn an_8bit_stippled_line_takes_its_colour_from_the_low_byte() {
        let ram = Arc::new(Mutex::new(O2Memory::single(32 << 20)));
        let re = CrimeRe::new(ram.clone());
        let tile = 0x0010_0000u32;
        let b = 0x1500_0000;
        // TLB C (buffer 2), entry 16: the second row of tiles, y 128-255.
        re.write64(b + TLB_A + 0x400 + 0x20, (0x8000u64 | (tile >> 16) as u64) << 48);
        re.write32(b + BUFMODE_DST, 0x800);
        re.write32(b + DRAWMODE, 0x0008_0278);
        re.write32(b + PRIMITIVE, 0x0104_0020);
        re.write32(b + FG, 2);
        re.write32(b + LOGIC_OP, 3);
        re.write32(b + STIPPLE_MODE, 0x001f_0000);
        re.write64(b + VERTEX_0, 0x0081_00b4_0089_00b4);
        re.write32(b + (STIPPLE_PATTERN | 0x800), 0xff00_0000);
        let m = ram.lock().unwrap();
        let row = (tile + (0xb4 - 128) * 512 + 0x81) as usize;
        assert_eq!(&m[row..row + 9], &[2, 2, 2, 2, 2, 2, 2, 2, 0], "eight pixels, the end one skipped");
    }

    #[test]
    fn overlay_pixels_cover_the_normal_planes_and_zero_is_clear() {
        let ram = Arc::new(Mutex::new(O2Memory::single(32 << 20)));
        let g = gbe();
        // Normal planes: one tile of 5s. Overlay: its own tile, one pixel 7.
        let (tile, list, otile, olist) = (0x0010_0000u32, 0x0020_0000u32, 0x0030_0000u32, 0x0020_0100u32);
        {
            let mut m = ram.lock().unwrap();
            m.write(list as usize, &((tile >> 16) as u16).to_be_bytes());
            m.write(olist as usize, &((otile >> 16) as u16).to_be_bytes());
            m.write(tile as usize, &[5u8; 0x10000]);
            m.write((otile + 4 * 512 + 3) as usize, &[7]);
        }
        for (off, v) in [
            (CTRLSTAT, 0x300a_a000u32),
            (DOTCLOCK, 0x0010_020f),
            (VT_XYMAX, 0x0042_9690),
            (FRM_CONTROL_SHADOW, list | 1),
            (OVR_CONTROL_SHADOW, olist | 1),
            (VT_XY, 0),
            (VT_HCMAP, 512 << 12),
            (VT_VCMAP, 128 << 12),
            (FRM_SIZE_TILE, 1 << 5),
            (OVR_WIDTH_TILE, 1 << 5),
            (FRM_SIZE_PIXEL, 128 << 16),
            (MODE, 0x403),
            (CMAP + 4 * 5, 0x11223300),
            (CMAP + 4 * 0x1107, 0xaabbcc00),
        ] {
            g.write32(0x1600_0000 + off, v);
        }
        let mut out = vec![0u8; 2048 * 2048 * 4];
        g.render(&ram.lock().unwrap(), &mut out).expect("display on");
        let px = |x: usize, y: usize| &out[(y * 512 + x) * 4..(y * 512 + x) * 4 + 3];
        assert_eq!(px(3, 4), [0xaa, 0xbb, 0xcc], "overlay colour from entry 0x1100 + 7");
        assert_eq!(px(4, 4), [0x11, 0x22, 0x33], "overlay 0 shows the normal planes");
    }
}

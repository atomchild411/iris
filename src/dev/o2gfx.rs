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
    pub const VT_HCMAP: u32 = 0x1003c;
    pub const VT_VCMAP: u32 = 0x10040;
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
}

pub struct Gbe {
    s: Mutex<GbeState>,
    /// Guest time in microseconds (MACE UST).
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Gbe {
    pub fn new(now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        let mut r = vec![0u32; GBE_WORDS];
        // GPIO pins unconfigured (inputs), chip id 1.
        r[(CTRLSTAT / 4) as usize] = 0x03ff_ffc1;
        r[(VT_XY / 4) as usize] = VT_FREEZE;
        Self { s: Mutex::new(GbeState { r, t0: 0, reads: 0 }), now }
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
        let us = (self.now)().saturating_sub(s.t0);
        let dots = us * Self::dot_hz(&s.r) / 1_000_000 + s.reads * DOTS_PER_READ;
        let x = dots % (maxx + 1);
        let y = (dots / (maxx + 1)) % (maxy + 1);
        ((y as u32) << 12) | x as u32
    }

    fn read_reg(&self, off: u32) -> u32 {
        let mut s = self.s.lock().unwrap();
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
            s.t0 = (self.now)();
            s.reads = 0;
        }
        s.r[i] = val;
        // The hardware copies each DMA control's shadow into the live
        // register at the start of vertical blanking; doing it at once
        // satisfies every poll that waits for it.
        let live = match off {
            OVR_CONTROL_SHADOW => Some(OVR_CONTROL),
            FRM_CONTROL_SHADOW => Some(FRM_CONTROL),
            DID_CONTROL_SHADOW => Some(DID_CONTROL),
            _ => None,
        };
        if let Some(l) = live {
            s.r[(l / 4) as usize] = val;
        }
    }

    /// Scan the frame out of RAM into `out` (R, G, B, A bytes, row 0 at the
    /// top), as GBE would. Returns the size drawn, or None when the display
    /// is off. `out` must hold the largest mode (2048 x 2048 pixels).
    pub fn render(&self, mem: &O2Memory, out: &mut [u8]) -> Option<(u32, u32)> {
        let r = self.s.lock().unwrap().r.clone();
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

        // DID tables are not modelled yet: WID 0 for every pixel, which is
        // what the PROM and both BSD consoles program (all 32 alike).
        let wid = g(MODE);
        let typ = (wid >> 2) & 7;
        let cm = (wid >> 5) & 0x1f;
        let gamma = wid & (1 << 10) == 0;
        let cmap = |i: u32| g(CMAP + 4 * (i % 6144));
        let gmap = |v: u32, shift: u32| (g(GMAP + 4 * (v & 0xff)) >> shift) & 0xff;
        let crs_on = g(CRS_CTL) & 1 != 0;
        let crs = g(CRS_POS);
        let (cx, cy) = ((crs & 0xffff) as i16 as i32, (crs >> 16) as i16 as i32);

        for y in 0..h {
            for x in 0..w {
                let p = fetch((y * w + x) * bpp);
                let p = if typ <= 3 && bpp > 1 { p & 0xffff } else { p };
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
    pub const BUFMODE_SRC: u32 = 0x2000;
    pub const BUFMODE_DST: u32 = 0x2008;
    pub const DRAWMODE: u32 = 0x2018;
    pub const WINOFFSET_SRC: u32 = 0x2050;
    pub const WINOFFSET_DST: u32 = 0x2058;
    pub const PRIMITIVE: u32 = 0x2060;
    pub const VERTEX_0: u32 = 0x2070;
    pub const VERTEX_1: u32 = 0x2074;
    pub const XFER_SRC: u32 = 0x20a0;
    pub const STIPPLE_MODE: u32 = 0x20c0;
    pub const STIPPLE_PATTERN: u32 = 0x20c4;
    pub const FG: u32 = 0x20d0;
    pub const BG: u32 = 0x20d8;
    pub const LOGIC_OP: u32 = 0x21b0;
    pub const COLOR_MASK: u32 = 0x21b8;
    pub const MTE_MODE: u32 = 0x3000;
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
    ram: Arc<Mutex<O2Memory>>,
    /// What the guest asked for that is not modelled, with counts, so a
    /// missing primitive shows up instead of drawing nothing silently.
    unhandled: Mutex<BTreeMap<String, u64>>,
    /// Primitives run, for the end-of-run report.
    pub drawn: std::sync::atomic::AtomicU64,
}

/// A buffer the RE draws through: which TLB, and bytes per pixel word.
#[derive(Clone, Copy)]
struct Buf {
    tlb: u32,
    bd: u32,
}

impl CrimeRe {
    pub fn new(ram: Arc<Mutex<O2Memory>>) -> Self {
        Self {
            regs: Mutex::new(vec![0; RE_WORDS]),
            ram,
            unhandled: Mutex::new(BTreeMap::new()),
            drawn: std::sync::atomic::AtomicU64::new(0),
        }
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

    /// A register colour's lanes for a pixel at memory offset `o` (the
    /// 32-bit value lies over the aligned memory word).
    fn lane(v: u32, o: usize, bd: u32) -> u32 {
        let shift = 8 * (4 - bd - (o as u32 & 3).min(4 - bd));
        let mask = if bd == 4 { u32::MAX } else { (1 << (8 * bd)) - 1 };
        (v >> shift) & mask
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

    /// Run the pixel pipe's current primitive.
    fn pixel_pipe(&self) {
        self.drawn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let r = self.regs.lock().unwrap().clone();
        let g = |off: u32| r[(off / 4) as usize];
        let dm = g(DRAWMODE);
        let prim = g(PRIMITIVE);
        let Some(dst) = self.buf(&r, g(BUFMODE_DST), "dst") else { return };
        let (ox, oy) = Self::xy(g(WINOFFSET_DST));
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
                    if !(last && skip_last) && x >= 0 && y >= 0 {
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
                            tmp.push(v);
                        }
                    }
                    let mut it = tmp.into_iter();
                    for y in ya..=yb {
                        for x in xa..=xb {
                            let v = it.next().unwrap_or(0);
                            if let Some(o) = Self::locate(&r, mem, dst, x, y) {
                                Self::put(&r, mem, o, dst.bd, v);
                            }
                        }
                    }
                } else {
                    let stippled = dm & draw::POLY_STIPPLE != 0;
                    for y in ya..=yb {
                        for (k, x) in (xa..=xb).enumerate() {
                            if let Some(o) = Self::locate(&r, mem, dst, x, y) {
                                if let Some(v) = colour_at(mem, o, k as u32, stippled) {
                                    Self::put(&r, mem, o, dst.bd, v);
                                }
                            }
                        }
                    }
                }
            }
            _ => self.note("triangle".into()),
        }
    }

    /// Run the memory transfer engine's current transfer (NetBSD's clears
    /// and scrolls).
    fn mte(&self) {
        self.drawn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let r = self.regs.lock().unwrap().clone();
        let g = |off: u32| r[(off / 4) as usize];
        let mode = g(MTE_MODE);
        let copy = mode & (1 << 11) != 0;
        let dbuf = (mode >> 2) & 7;
        let sbuf = (mode >> 5) & 7;
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
        self.write_reg(addr & 0x7ffc, val);
        BUS_OK
    }
    fn write64(&self, addr: u32, val: u64) -> u32 {
        // Both halves land before a start acts, so a 64-bit write to a start
        // address (vertices 0 and 1 at once) draws with both.
        let a = addr & 0x7ff8;
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
            (VT_XY, 0),
            (VT_HCMAP, 512 << 12),
            (VT_VCMAP, 128 << 12),
            (FRM_SIZE_TILE, 1 << 5),
            (FRM_SIZE_PIXEL, 128 << 16),
            (FRM_CONTROL_SHADOW, list | 1),
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
}

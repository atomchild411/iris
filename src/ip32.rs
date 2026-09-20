//! IP32 (SGI O2) bring-up scaffold: CRIME and a minimal physical bus.
//!
//! This is deliberately *not* wired into `Physical`. The O2's memory map has
//! little in common with the Indy's — RAM starts at 0, CRIME sits where the
//! Indy has GIO — and `MipsCpu::new` takes any `Arc<dyn BusDevice>`, so the
//! real CPU can be driven against the map below without touching the IP22/IP24
//! path at all.
//!
//! Scope is the first bring-up milestone only: get the PROM's `sloader`
//! section far enough to hand off to `post1` at 0xa0004000. Nothing here
//! pretends to be a complete CRIME.
//!
//! Everything was derived from observing the PROM's own behaviour and from
//! NetBSD's published register names. No code or comments were taken from the
//! GPL-3.0 `ip32prom-decompiler` annotations; IRIS is BSD-3-Clause.
//!
//! See `docs/ip32-o2-bringup.md`.

use std::sync::Mutex;

use crate::traits::{BusDevice, BusRead8, BusRead16, BusRead32, BusRead64, BUS_OK, BUS_ERR};

// ── IP32 physical map (the part this milestone needs) ───────────────────────
/// RAM. Unlike the Indy, an O2's memory starts at physical zero.
pub const RAM_BASE: u32 = 0x0000_0000;
/// CRIME: memory controller, interrupt controller and video DMA.
pub const CRIME_BASE: u32 = 0x1400_0000;
pub const CRIME_SIZE: u32 = 0x0000_1000;
/// MACE: the I/O ASIC. Only present here so probes read as empty rather than
/// taking a bus error — none of it is implemented yet.
pub const MACE_BASE: u32 = 0x1f00_0000;
pub const MACE_SIZE: u32 = 0x0080_0000;
/// Boot PROM. Same address as the Indy's, which is the one thing that carries
/// over: the MIPS reset vector is 0xbfc00000 on both.
pub const PROM_BASE: u32 = 0x1fc0_0000;
pub const PROM_SIZE: u32 = 0x0008_0000; // 512 KiB

/// Where `post1` is loaded and entered. Reaching this PC is the milestone.
pub const POST1_ENTRY: u32 = 0xa000_4000;

// ── CRIME registers ─────────────────────────────────────────────────────────
// Offsets follow the names NetBSD's crimereg.h uses, so the two can be read
// side by side. CRIME is a 64-bit register file: the PROM reaches it with
// `sd`/`ld` through uncached KSEG1 (0xb4000000), never 32-bit accesses.
pub mod crime_reg {
    pub const REVISION: u32 = 0x0000;
    pub const CONTROL: u32 = 0x0008;
    pub const INT_STAT: u32 = 0x0010;
    pub const INT_MASK: u32 = 0x0018;
    pub const INT_SOFT: u32 = 0x0020;
    pub const WATCHDOG: u32 = 0x0030;
    pub const TIME: u32 = 0x0038;
    pub const CPU_ERROR_ADDR: u32 = 0x0040;
    pub const CPU_ERROR_STAT: u32 = 0x0048;
    pub const CPU_ERROR_ENA: u32 = 0x0050;

    pub const MEM_CONTROL: u32 = 0x0200;
    pub const MEM_BANK_CTRL0: u32 = 0x0208;
    pub const MEM_BANK_CTRL1: u32 = 0x0218;
    pub const MEM_REFRESH_CNTR: u32 = 0x0248;
    pub const MEM_ERROR_STAT: u32 = 0x0250;
    pub const MEM_ERROR_ADDR: u32 = 0x0258;
    pub const MEM_ERROR_ECC_SYN: u32 = 0x0260;
    pub const MEM_ERROR_ECC_CHK: u32 = 0x0268;
    pub const MEM_ERROR_ECC_REPL: u32 = 0x0270;
}

/// CRIME revision as reported to the guest.
///
/// The low half identifies the ASIC revision; O2s in the field report a small
/// number here. Which value the PROM demands is one of the things the first
/// run is meant to discover, so it is a field rather than a constant.
pub const CRIME_REV_DEFAULT: u64 = 0x0000_0000_0000_0011;

/// Minimal CRIME: a 64-bit register file that remembers what it is told, with
/// the handful of reads the PROM cannot be fobbed off with zero for.
pub struct Crime {
    regs: Mutex<Vec<u64>>,
    /// Every distinct offset the guest has touched, in first-touch order, with
    /// whether it was read or written. This is the actual product of the first
    /// milestone: the list of CRIME registers POST depends on.
    trace: Mutex<Vec<(u32, bool)>>,
    revision: u64,
}

impl Default for Crime {
    fn default() -> Self {
        Self::new()
    }
}

impl Crime {
    pub fn new() -> Self {
        Self {
            regs: Mutex::new(vec![0u64; (CRIME_SIZE / 8) as usize]),
            trace: Mutex::new(Vec::new()),
            revision: CRIME_REV_DEFAULT,
        }
    }

    pub fn with_revision(revision: u64) -> Self {
        Self { revision, ..Self::new() }
    }

    fn note(&self, off: u32, write: bool) {
        let mut t = self.trace.lock().unwrap();
        if !t.iter().any(|&(o, w)| o == off && w == write) {
            t.push((off, write));
        }
    }

    /// Registers touched so far, in first-touch order: `(offset, was_write)`.
    pub fn touched(&self) -> Vec<(u32, bool)> {
        self.trace.lock().unwrap().clone()
    }

    fn load(&self, off: u32) -> u64 {
        match off {
            // Identity. Returning zero here is the one thing guaranteed to make
            // a PROM decide it is running on hardware it does not recognise.
            crime_reg::REVISION => self.revision,
            // A free-running microsecond-ish counter. POST loops on this to time
            // out, so a constant would hang it forever.
            crime_reg::TIME => {
                let mut r = self.regs.lock().unwrap();
                let i = (crime_reg::TIME / 8) as usize;
                r[i] = r[i].wrapping_add(1);
                r[i] & 0x0000_ffff_ffff_ffff
            }
            _ => self.regs.lock().unwrap()[(off / 8) as usize],
        }
    }

    fn store(&self, off: u32, val: u64) {
        self.regs.lock().unwrap()[(off / 8) as usize] = val;
    }
}

impl BusDevice for Crime {
    fn read64(&self, addr: u32) -> BusRead64 {
        let off = addr & (CRIME_SIZE - 1) & !7;
        self.note(off, false);
        BusRead64::ok(self.load(off))
    }

    fn write64(&self, addr: u32, val: u64) -> u32 {
        let off = addr & (CRIME_SIZE - 1) & !7;
        self.note(off, true);
        self.store(off, val);
        BUS_OK
    }

    // CRIME is a 64-bit register file. The PROM only ever uses sd/ld, but a
    // 32-bit access should read the correct half rather than fail, so a stray
    // one shows up in the trace instead of as a bus error we would then have to
    // chase.
    fn read32(&self, addr: u32) -> BusRead32 {
        let q = self.read64(addr & !7).data;
        let hi = (addr & 4) == 0;
        BusRead32::ok(if hi { (q >> 32) as u32 } else { q as u32 })
    }

    fn write32(&self, addr: u32, val: u32) -> u32 {
        let off = addr & (CRIME_SIZE - 1) & !7;
        let cur = self.load(off);
        let merged = if (addr & 4) == 0 {
            (cur & 0x0000_0000_ffff_ffff) | ((val as u64) << 32)
        } else {
            (cur & 0xffff_ffff_0000_0000) | val as u64
        };
        self.note(off, true);
        self.store(off, merged);
        BUS_OK
    }
}

/// Reads as zero, absorbs writes, and counts both. Stands in for MACE until
/// there is a reason to build it: a probe that bus-errors tells us nothing,
/// whereas one that reads zero lets POST proceed to whatever it does next.
pub struct Stub {
    pub name: &'static str,
    reads: Mutex<u64>,
    writes: Mutex<u64>,
}

impl Stub {
    pub fn new(name: &'static str) -> Self {
        Self { name, reads: Mutex::new(0), writes: Mutex::new(0) }
    }
    pub fn counts(&self) -> (u64, u64) {
        (*self.reads.lock().unwrap(), *self.writes.lock().unwrap())
    }
    fn r(&self) {
        *self.reads.lock().unwrap() += 1;
    }
    fn w(&self) {
        *self.writes.lock().unwrap() += 1;
    }
}

impl BusDevice for Stub {
    fn read8(&self, _a: u32) -> BusRead8 { self.r(); BusRead8::ok(0) }
    fn read16(&self, _a: u32) -> BusRead16 { self.r(); BusRead16::ok(0) }
    fn read32(&self, _a: u32) -> BusRead32 { self.r(); BusRead32::ok(0) }
    fn read64(&self, _a: u32) -> BusRead64 { self.r(); BusRead64::ok(0) }
    fn write8(&self, _a: u32, _v: u8) -> u32 { self.w(); BUS_OK }
    fn write16(&self, _a: u32, _v: u16) -> u32 { self.w(); BUS_OK }
    fn write32(&self, _a: u32, _v: u32) -> u32 { self.w(); BUS_OK }
    fn write64(&self, _a: u32, _v: u64) -> u32 { self.w(); BUS_OK }
}

/// The IP32 physical bus for bring-up: RAM, CRIME, a MACE stub and the PROM.
///
/// Deliberately a flat `match` rather than the 64 KB dispatch table `Physical`
/// uses. At this stage legibility and being able to log an unmapped access are
/// worth more than the lookup.
pub struct Ip32Bus {
    ram: Mutex<Vec<u8>>,
    prom: Vec<u8>,
    pub crime: Crime,
    pub mace: Stub,
    /// Accesses that hit nothing, first 64 kept, as `(addr, is_write)`.
    unmapped: Mutex<Vec<(u32, bool)>>,
}

impl Ip32Bus {
    /// `ram_bytes` is rounded down to a multiple of 8. `prom` must be the raw
    /// 512 KiB IP32 PROM image.
    pub fn new(ram_bytes: usize, prom: Vec<u8>) -> Self {
        Self {
            ram: Mutex::new(vec![0u8; ram_bytes & !7]),
            prom,
            crime: Crime::new(),
            mace: Stub::new("mace"),
            unmapped: Mutex::new(Vec::new()),
        }
    }

    pub fn unmapped(&self) -> Vec<(u32, bool)> {
        self.unmapped.lock().unwrap().clone()
    }

    fn miss(&self, addr: u32, write: bool) {
        let mut u = self.unmapped.lock().unwrap();
        if u.len() < 64 && !u.iter().any(|&(a, w)| a == addr && w == write) {
            u.push((addr, write));
        }
    }

    fn ram_len(&self) -> u32 {
        self.ram.lock().unwrap().len() as u32
    }

    fn in_ram(&self, addr: u32) -> bool {
        addr >= RAM_BASE && addr < RAM_BASE.wrapping_add(self.ram_len())
    }

    fn in_prom(&self, addr: u32) -> bool {
        addr >= PROM_BASE && addr < PROM_BASE + PROM_SIZE
    }

    fn in_crime(&self, addr: u32) -> bool {
        addr >= CRIME_BASE && addr < CRIME_BASE + CRIME_SIZE
    }

    fn in_mace(&self, addr: u32) -> bool {
        addr >= MACE_BASE && addr < MACE_BASE + MACE_SIZE
    }

    fn read_bytes(&self, addr: u32, n: usize) -> Option<u64> {
        if self.in_ram(addr) {
            let r = self.ram.lock().unwrap();
            let i = (addr - RAM_BASE) as usize;
            if i + n > r.len() {
                return None;
            }
            let mut v = 0u64;
            for k in 0..n {
                v = (v << 8) | r[i + k] as u64;
            }
            return Some(v);
        }
        if self.in_prom(addr) {
            let i = (addr - PROM_BASE) as usize;
            if i + n > self.prom.len() {
                return None;
            }
            let mut v = 0u64;
            for k in 0..n {
                v = (v << 8) | self.prom[i + k] as u64;
            }
            return Some(v);
        }
        None
    }

    fn write_bytes(&self, addr: u32, n: usize, val: u64) -> bool {
        if self.in_ram(addr) {
            let mut r = self.ram.lock().unwrap();
            let i = (addr - RAM_BASE) as usize;
            if i + n > r.len() {
                return false;
            }
            for k in 0..n {
                r[i + k] = (val >> (8 * (n - 1 - k))) as u8;
            }
            return true;
        }
        // The PROM is read-only; a write to it is a real finding, not a no-op.
        false
    }
}

macro_rules! bus_read {
    ($self:ident, $addr:expr, $n:expr, $ok:path, $conv:expr) => {{
        let addr = $addr;
        if $self.in_crime(addr) || $self.in_mace(addr) {
            unreachable!("device reads are dispatched before this point")
        }
        match $self.read_bytes(addr, $n) {
            Some(v) => $ok($conv(v)),
            None => {
                $self.miss(addr, false);
                $ok($conv(0u64))
            }
        }
    }};
}

impl BusDevice for Ip32Bus {
    fn read8(&self, addr: u32) -> BusRead8 {
        if self.in_crime(addr) {
            return BusRead8::ok(self.crime.read32(addr & !3).data as u8);
        }
        if self.in_mace(addr) {
            return self.mace.read8(addr);
        }
        bus_read!(self, addr, 1, BusRead8::ok, |v: u64| v as u8)
    }

    fn read16(&self, addr: u32) -> BusRead16 {
        if self.in_crime(addr) {
            return BusRead16::ok(self.crime.read32(addr & !3).data as u16);
        }
        if self.in_mace(addr) {
            return self.mace.read16(addr);
        }
        bus_read!(self, addr, 2, BusRead16::ok, |v: u64| v as u16)
    }

    fn read32(&self, addr: u32) -> BusRead32 {
        if self.in_crime(addr) {
            return self.crime.read32(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read32(addr);
        }
        bus_read!(self, addr, 4, BusRead32::ok, |v: u64| v as u32)
    }

    fn read64(&self, addr: u32) -> BusRead64 {
        if self.in_crime(addr) {
            return self.crime.read64(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read64(addr);
        }
        bus_read!(self, addr, 8, BusRead64::ok, |v: u64| v)
    }

    fn write8(&self, addr: u32, val: u8) -> u32 {
        if self.in_crime(addr) {
            return self.crime.write32(addr & !3, val as u32);
        }
        if self.in_mace(addr) {
            return self.mace.write8(addr, val);
        }
        if self.write_bytes(addr, 1, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write16(&self, addr: u32, val: u16) -> u32 {
        if self.in_crime(addr) {
            return self.crime.write32(addr & !3, val as u32);
        }
        if self.in_mace(addr) {
            return self.mace.write16(addr, val);
        }
        if self.write_bytes(addr, 2, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write32(&self, addr: u32, val: u32) -> u32 {
        if self.in_crime(addr) {
            return self.crime.write32(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write32(addr, val);
        }
        if self.write_bytes(addr, 4, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write64(&self, addr: u32, val: u64) -> u32 {
        if self.in_crime(addr) {
            return self.crime.write64(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write64(addr, val);
        }
        if self.write_bytes(addr, 8, val) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }
}

/// Parsed IP32 PROM section header. The PROM is a run of `SHDR` records, and
/// knowing where `post1` and `firmware` load is what lets a trace say "it got
/// to POST" rather than "the PC is somewhere".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromSection {
    pub name: String,
    pub version: String,
    pub file_off: u32,
    pub length: u32,
}

/// Walk the `SHDR` records. Returns them in file order.
pub fn parse_prom_sections(prom: &[u8]) -> Vec<PromSection> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 64 <= prom.len() {
        if &prom[i..i + 4] == b"SHDR" {
            let length = u32::from_be_bytes([prom[i + 4], prom[i + 5], prom[i + 6], prom[i + 7]]);
            let cstr = |off: usize, max: usize| {
                let s = &prom[off..(off + max).min(prom.len())];
                let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
                String::from_utf8_lossy(&s[..end]).to_string()
            };
            out.push(PromSection {
                name: cstr(i + 12, 32),
                version: cstr(i + 44, 8),
                file_off: i as u32,
                length,
            });
        }
        i += 4;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crime_is_a_64_bit_register_file() {
        let c = Crime::new();
        c.write64(CRIME_BASE + crime_reg::MEM_BANK_CTRL0, 0x0123_4567_89ab_cdef);
        assert_eq!(
            c.read64(CRIME_BASE + crime_reg::MEM_BANK_CTRL0).data,
            0x0123_4567_89ab_cdef,
            "a qword written to CRIME must read back whole",
        );
    }

    /// The PROM identifies the machine from this register before it will do
    /// anything else, so zero is the one answer guaranteed to be wrong.
    #[test]
    fn revision_does_not_read_as_zero() {
        let c = Crime::new();
        assert_ne!(c.read64(CRIME_BASE + crime_reg::REVISION).data, 0);
    }

    /// POST times its waits against this counter. A constant would spin forever.
    #[test]
    fn the_time_counter_advances() {
        let c = Crime::new();
        let a = c.read64(CRIME_BASE + crime_reg::TIME).data;
        let b = c.read64(CRIME_BASE + crime_reg::TIME).data;
        assert!(b > a, "CRIME_TIME must advance between reads, got {a} then {b}");
    }

    #[test]
    fn every_touched_register_is_recorded_once_per_direction() {
        let c = Crime::new();
        c.write64(CRIME_BASE + crime_reg::MEM_CONTROL, 1);
        c.write64(CRIME_BASE + crime_reg::MEM_CONTROL, 2);
        let _ = c.read64(CRIME_BASE + crime_reg::MEM_CONTROL);
        assert_eq!(
            c.touched(),
            vec![(crime_reg::MEM_CONTROL, true), (crime_reg::MEM_CONTROL, false)],
        );
    }

    #[test]
    fn ram_starts_at_physical_zero_unlike_the_indy() {
        let bus = Ip32Bus::new(1 << 20, vec![0u8; PROM_SIZE as usize]);
        assert_eq!(bus.write32(0, 0xdead_beef), BUS_OK);
        assert_eq!(bus.read32(0).data, 0xdead_beef);
    }

    #[test]
    fn the_prom_is_readable_and_not_writable() {
        let mut image = vec![0u8; PROM_SIZE as usize];
        image[..4].copy_from_slice(&0x1000_0011u32.to_be_bytes());
        let bus = Ip32Bus::new(1 << 20, image);
        assert_eq!(bus.read32(PROM_BASE).data, 0x1000_0011, "reset vector must read back");
        assert_eq!(bus.write32(PROM_BASE, 0), BUS_ERR, "the PROM must reject writes");
        assert_eq!(bus.unmapped(), vec![(PROM_BASE, true)], "and record the attempt");
    }

    #[test]
    fn section_headers_parse() {
        // Two records: a name, a version, a length.
        let mut img = vec![0u8; 256];
        img[8..12].copy_from_slice(b"SHDR");
        img[12..16].copy_from_slice(&0x4000u32.to_be_bytes());
        img[20..27].copy_from_slice(b"sloader");
        img[52..55].copy_from_slice(b"1.0");
        let s = parse_prom_sections(&img);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "sloader");
        assert_eq!(s[0].version, "1.0");
        assert_eq!(s[0].length, 0x4000);
    }
}

/// Driving the real CPU against the map above, to find out how far the PROM
/// gets and what it touches on the way.
///
/// These need a PROM image, which is SGI's and is not in the repo. Point
/// `IRIS_IP32_PROM` at one (or drop it at `ip32/ip32prom.rev4.18.bin` beside
/// the checkout) and they run; otherwise they skip, so CI stays green.
#[cfg(test)]
mod bringup {
    use super::*;
    use crate::mips_cache_v2::R5000Cache;
    use crate::mips_exec::{MipsCpuConfig, MipsExecutor};
    use crate::mips_tlb::MipsTlb;
    use std::sync::Arc;

    fn prom_image() -> Option<Vec<u8>> {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Some(p) = std::env::var_os("IRIS_IP32_PROM") {
            candidates.push(p.into());
        }
        for rel in [
            "ip32/ip32prom.rev4.18.bin",
            "../ip32/ip32prom.rev4.18.bin",
            "ip32/ip32prom.rev4.3.bin",
            "../ip32/ip32prom.rev4.3.bin",
        ] {
            candidates.push(rel.into());
        }
        for c in candidates {
            if let Ok(d) = std::fs::read(&c) {
                if d.len() == PROM_SIZE as usize {
                    eprintln!("ip32: using PROM {}", c.display());
                    return Some(d);
                }
            }
        }
        eprintln!("ip32: no PROM image found — set IRIS_IP32_PROM; skipping");
        None
    }

    /// Run the PROM from reset and report where it goes.
    ///
    /// Not an assertion about reaching POST — we do not yet know that it can.
    /// The output *is* the deliverable: which CRIME registers POST depends on,
    /// and what it reaches for that we have not built.
    #[test]
    fn trace_the_prom_from_reset() {
        let Some(prom) = prom_image() else { return };

        let sections = parse_prom_sections(&prom);
        eprintln!("ip32: PROM sections:");
        for s in &sections {
            eprintln!("   {:<10} v{:<5} @0x{:06x} len 0x{:05x}", s.name, s.version, s.file_off, s.length);
        }

        // 128 MB: a common O2 configuration, and enough that a memory sizing
        // loop has something to find.
        let bus = Arc::new(Ip32Bus::new(128 << 20, prom));
        let sysad: Arc<dyn crate::traits::BusDevice> = bus.clone();
        let cfg = MipsCpuConfig::indy();
        let tlb = MipsTlb::new(cfg.tlb_entries);
        let mut exec: MipsExecutor<MipsTlb, R5000Cache> = MipsExecutor::new(sysad, tlb, &cfg);

        let mut reached_post1 = false;
        let mut steps = 0u64;
        const LIMIT: u64 = 2_000_000;

        while steps < LIMIT {
            let pc = exec.core.pc as u32;
            if pc == POST1_ENTRY {
                reached_post1 = true;
                break;
            }
            exec.step_int();
            steps += 1;
        }

        let pc = exec.core.pc as u32;
        eprintln!("ip32: stopped after {steps} steps at PC 0x{pc:08x}{}",
                  if reached_post1 { "  <-- post1 entry" } else { "" });

        let touched = bus.crime.touched();
        eprintln!("ip32: CRIME registers touched ({}):", touched.len());
        for (off, write) in &touched {
            eprintln!("   0x{:04x} {}", off, if *write { "W" } else { "R" });
        }

        let (mr, mw) = bus.mace.counts();
        eprintln!("ip32: MACE accesses: {mr} reads, {mw} writes");

        let un = bus.unmapped();
        eprintln!("ip32: unmapped accesses ({}):", un.len());
        for (a, w) in un.iter().take(24) {
            eprintln!("   0x{:08x} {}", a, if *w { "W" } else { "R" });
        }
    }
}

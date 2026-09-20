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
/// RAM base.
///
/// **Not zero.** The PROM's SizeMEM writes its probe patterns at `base + 0`,
/// `+0x01fffff8`, `+0x02000000` and `+0x07fffff8`, and the physical address
/// those land on is 0x40000000 — visible as the target of the `sd` at
/// 0xbfc05c9c. This is the "RAM at a non-zero base address" the O2 is known
/// for, and it collides with what `macereg.h` calls `MACE_PCI_NATIVE_VIEW`;
/// main memory wins that address, so the PCI window is not mapped here.
pub const RAM_BASE: u32 = 0x4000_0000;
/// CRIME: memory controller, interrupt controller and video DMA.
pub const CRIME_BASE: u32 = 0x1400_0000;
pub const CRIME_SIZE: u32 = 0x0000_1000;
/// MACE: the I/O ASIC. Only present here so probes read as empty rather than
/// taking a bus error — none of it is implemented yet.
pub const MACE_BASE: u32 = 0x1f00_0000;
pub const MACE_SIZE: u32 = 0x0080_0000;
/// MACE's ISA-block register holding the flash write-enable and the Dallas
/// 1-Wire ("NIC") data line. The PROM bit-bangs the serial-ID chip here.
pub const MACE_ISA_FLASH_NIC_REG: u32 = MACE_BASE + 0x0031_0008;

/// Bits in [`MACE_ISA_FLASH_NIC_REG`], as named by NetBSD's `macereg.h`.
pub mod nic_bit {
    /// 1 => flash writes enabled.
    pub const FLASH_WE: u8 = 0x01;
    /// Release the 1-Wire line (let it float high).
    pub const DEASSERT: u8 = 0x04;
    /// The 1-Wire data line itself.
    pub const DATA: u8 = 0x08;
}

/// MACE's free-running timer (`MACE_UST_MSC`, i.e. `MACE_PERIF + 0x40000`).
/// The PROM busy-waits on this; a constant hangs it forever.
pub const MACE_UST_MSC: u32 = MACE_BASE + 0x0034_0000;
pub const MACE_UST_MSC_SIZE: u32 = 0x0001_0000;

/// MACE's PCI host bridge register block, at MACE + 0x080000.
pub const MACEPCI_BASE: u32 = MACE_BASE + 0x0008_0000;
pub const MACEPCI_SIZE: u32 = 0x0001_0000;
/// The window where PCI memory appears 1:1 (`MACE_PCI_NATIVE_VIEW`). An O2
/// PROM walks this while looking for the Adaptec SCSI controller.
pub const PCI_NATIVE_VIEW_BASE: u32 = 0x4000_0000;
pub const PCI_NATIVE_VIEW_SIZE: u32 = 0x0800_0000;

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
    /// Values written, in order, as `(offset, value)` — the bank programming
    /// is only meaningful if you can see what it was told.
    writes: Mutex<Vec<(u32, u64)>>,
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
            writes: Mutex::new(Vec::new()),
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
        let mut w = self.writes.lock().unwrap();
        if w.len() < 256 {
            w.push((off, val));
        }
    }

    /// Values written, in order.
    pub fn written(&self) -> Vec<(u32, u64)> {
        self.writes.lock().unwrap().clone()
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

/// MACE's PCI host bridge, with nothing plugged into it.
///
/// Config cycles go through an address/data register pair, exactly like a PC's
/// 0xcf8/0xcfc. The detail that matters for an empty bus: a config read of an
/// absent device must return **all ones**, not zero. Zero reads back as vendor
/// ID 0x0000, which firmware takes for a device that is present but broken;
/// 0xffffffff is the architectural "nobody home".
pub struct MacePci {
    config_addr: Mutex<u32>,
    regs: Mutex<[u32; (MACEPCI_SIZE / 4) as usize]>,
    /// Distinct config addresses the guest selected, in order.
    probed: Mutex<Vec<u32>>,
}

pub mod macepci_reg {
    pub const ERROR_ADDR: u32 = 0x0000;
    pub const ERROR_FLAGS: u32 = 0x0004;
    pub const CONTROL: u32 = 0x0008;
    pub const REVISION: u32 = 0x000c;
    pub const CONFIG_ADDR: u32 = 0x0cf8;
    pub const CONFIG_DATA: u32 = 0x0cfc;
}

/// What a MACE PCI bridge reports for itself. Zero here would make the PROM
/// conclude the bridge is missing.
pub const MACEPCI_REVISION_VALUE: u32 = 1;

impl Default for MacePci {
    fn default() -> Self { Self::new() }
}

impl MacePci {
    pub fn new() -> Self {
        Self {
            config_addr: Mutex::new(0),
            regs: Mutex::new([0u32; (MACEPCI_SIZE / 4) as usize]),
            probed: Mutex::new(Vec::new()),
        }
    }

    /// Config addresses selected so far, in first-touch order.
    pub fn probed(&self) -> Vec<u32> {
        self.probed.lock().unwrap().clone()
    }
}

impl BusDevice for MacePci {
    fn read32(&self, addr: u32) -> BusRead32 {
        let off = addr & (MACEPCI_SIZE - 1) & !3;
        match off {
            macepci_reg::CONFIG_DATA => {
                // Empty bus: every device is absent.
                BusRead32::ok(0xffff_ffff)
            }
            macepci_reg::CONFIG_ADDR => BusRead32::ok(*self.config_addr.lock().unwrap()),
            macepci_reg::REVISION => BusRead32::ok(MACEPCI_REVISION_VALUE),
            _ => BusRead32::ok(self.regs.lock().unwrap()[(off / 4) as usize]),
        }
    }

    fn write32(&self, addr: u32, val: u32) -> u32 {
        let off = addr & (MACEPCI_SIZE - 1) & !3;
        match off {
            macepci_reg::CONFIG_ADDR => {
                *self.config_addr.lock().unwrap() = val;
                let mut p = self.probed.lock().unwrap();
                if val != 0 && !p.contains(&val) {
                    p.push(val);
                }
            }
            macepci_reg::CONFIG_DATA => { /* writes to an absent device evaporate */ }
            _ => self.regs.lock().unwrap()[(off / 4) as usize] = val,
        }
        BUS_OK
    }

    fn read64(&self, addr: u32) -> BusRead64 {
        let hi = self.read32(addr).data as u64;
        let lo = self.read32(addr.wrapping_add(4)).data as u64;
        BusRead64::ok((hi << 32) | lo)
    }

    fn write64(&self, addr: u32, val: u64) -> u32 {
        self.write32(addr, (val >> 32) as u32);
        self.write32(addr.wrapping_add(4), val as u32)
    }
}

/// The PCI memory window with nothing behind it. Reads all-ones, absorbs
/// writes, and counts both so we can tell whether the PROM keeps poking.
pub struct PciNativeView {
    reads: Mutex<u64>,
    writes: Mutex<u64>,
}

impl Default for PciNativeView {
    fn default() -> Self { Self::new() }
}

impl PciNativeView {
    pub fn new() -> Self { Self { reads: Mutex::new(0), writes: Mutex::new(0) } }
    pub fn counts(&self) -> (u64, u64) {
        (*self.reads.lock().unwrap(), *self.writes.lock().unwrap())
    }
}

impl BusDevice for PciNativeView {
    fn read8(&self, _a: u32) -> BusRead8 { *self.reads.lock().unwrap() += 1; BusRead8::ok(0xff) }
    fn read16(&self, _a: u32) -> BusRead16 { *self.reads.lock().unwrap() += 1; BusRead16::ok(0xffff) }
    fn read32(&self, _a: u32) -> BusRead32 { *self.reads.lock().unwrap() += 1; BusRead32::ok(0xffff_ffff) }
    fn read64(&self, _a: u32) -> BusRead64 { *self.reads.lock().unwrap() += 1; BusRead64::ok(u64::MAX) }
    fn write8(&self, _a: u32, _v: u8) -> u32 { *self.writes.lock().unwrap() += 1; BUS_OK }
    fn write16(&self, _a: u32, _v: u16) -> u32 { *self.writes.lock().unwrap() += 1; BUS_OK }
    fn write32(&self, _a: u32, _v: u32) -> u32 { *self.writes.lock().unwrap() += 1; BUS_OK }
    fn write64(&self, _a: u32, _v: u64) -> u32 { *self.writes.lock().unwrap() += 1; BUS_OK }
}

/// Records what the PROM does to the 1-Wire register and when, so the protocol
/// can be reconstructed from its own behaviour rather than guessed at.
pub struct NicTrace {
    events: Mutex<Vec<(u64, bool, u8)>>,
}

impl Default for NicTrace {
    fn default() -> Self { Self::new() }
}

impl NicTrace {
    pub fn new() -> Self { Self { events: Mutex::new(Vec::new()) } }
    pub fn record(&self, t: u64, write: bool, val: u8) {
        let mut e = self.events.lock().unwrap();
        if e.len() < 4096 {
            e.push((t, write, val));
        }
    }
    pub fn events(&self) -> Vec<(u64, bool, u8)> {
        self.events.lock().unwrap().clone()
    }
}

/// MACE's UST/MSC counter: unadjusted system time, free-running.
///
/// Every read advances it by [`UST_STRIDE`]. Real hardware ticks off a clock
/// independently of who is looking; the PROM's delay loops
/// (`while (ust() < start + n)`) therefore spin at whatever rate we choose.
/// Advancing one per read makes an `n`-tick delay cost `n` loop iterations,
/// which for a millisecond-scale delay is millions of instructions. A coarse
/// stride keeps those loops honest — they still terminate in order — without
/// making bring-up runs take minutes.
pub struct MaceUst {
    ticks: Mutex<u64>,
}

/// Ticks added per read. Nothing derives a wall-clock figure from this yet; if
/// something ever does, this has to become time-based instead.
pub const UST_STRIDE: u64 = 1024;

impl Default for MaceUst {
    fn default() -> Self { Self::new() }
}

impl MaceUst {
    pub fn new() -> Self { Self { ticks: Mutex::new(0) } }
    fn tick(&self) -> u64 {
        let mut t = self.ticks.lock().unwrap();
        *t = t.wrapping_add(UST_STRIDE);
        *t
    }
    pub fn now(&self) -> u64 { *self.ticks.lock().unwrap() }
}

impl BusDevice for MaceUst {
    fn read32(&self, addr: u32) -> BusRead32 {
        let t = self.tick();
        // 64-bit counter presented as two 32-bit halves.
        BusRead32::ok(if addr & 4 == 0 { (t >> 32) as u32 } else { t as u32 })
    }
    fn read64(&self, _addr: u32) -> BusRead64 { BusRead64::ok(self.tick()) }
    fn write32(&self, _a: u32, _v: u32) -> u32 { BUS_OK }
    fn write64(&self, _a: u32, _v: u64) -> u32 { BUS_OK }
}

/// A register file that remembers what it is told, and counts accesses per
/// offset. Stands in for MACE until there is a reason to build it properly.
///
/// Storing matters more than it looks: the PROM does read-modify-write on
/// several of these (the LED register among them), and a register that always
/// reads back zero silently discards every bit the firmware thought it had
/// set.
pub struct Stub {
    pub name: &'static str,
    reads: Mutex<u64>,
    writes: Mutex<u64>,
    /// Per-offset access counts. A stub that is being polled hard is the guest
    /// waiting on a bit we are not setting, and the offset says which.
    hot: Mutex<std::collections::BTreeMap<u32, (u64, u64)>>,
    /// Per-offset stored value.
    cells: Mutex<std::collections::BTreeMap<u32, u64>>,
}

impl Stub {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            reads: Mutex::new(0),
            writes: Mutex::new(0),
            hot: Mutex::new(std::collections::BTreeMap::new()),
            cells: Mutex::new(std::collections::BTreeMap::new()),
        }
    }
    pub fn counts(&self) -> (u64, u64) {
        (*self.reads.lock().unwrap(), *self.writes.lock().unwrap())
    }
    /// Offsets by access count, busiest first: `(offset, reads, writes)`.
    pub fn hottest(&self) -> Vec<(u32, u64, u64)> {
        let mut v: Vec<(u32, u64, u64)> =
            self.hot.lock().unwrap().iter().map(|(&o, &(r, w))| (o, r, w)).collect();
        v.sort_by_key(|&(_, r, w)| std::cmp::Reverse(r + w));
        v
    }
    fn r(&self) {
        *self.reads.lock().unwrap() += 1;
    }
    fn w(&self) {
        *self.writes.lock().unwrap() += 1;
    }
    fn cell(&self, addr: u32) -> u64 {
        *self.cells.lock().unwrap().get(&(addr & 0x007f_ffff & !7)).unwrap_or(&0)
    }
    fn set_cell(&self, addr: u32, val: u64) {
        self.cells.lock().unwrap().insert(addr & 0x007f_ffff & !7, val);
    }
    fn hit(&self, addr: u32, write: bool) {
        let mut h = self.hot.lock().unwrap();
        let e = h.entry(addr & 0x007f_ffff & !3).or_insert((0, 0));
        if write { e.1 += 1 } else { e.0 += 1 }
    }
}

impl BusDevice for Stub {
    fn read8(&self, a: u32) -> BusRead8 { self.r(); self.hit(a, false); BusRead8::ok(self.cell(a) as u8) }
    fn read16(&self, a: u32) -> BusRead16 { self.r(); self.hit(a, false); BusRead16::ok(self.cell(a) as u16) }
    fn read32(&self, a: u32) -> BusRead32 { self.r(); self.hit(a, false); BusRead32::ok(self.cell(a) as u32) }
    fn read64(&self, a: u32) -> BusRead64 { self.r(); self.hit(a, false); BusRead64::ok(self.cell(a)) }
    fn write8(&self, a: u32, v: u8) -> u32 { self.w(); self.hit(a, true); self.set_cell(a, v as u64); BUS_OK }
    fn write16(&self, a: u32, v: u16) -> u32 { self.w(); self.hit(a, true); self.set_cell(a, v as u64); BUS_OK }
    fn write32(&self, a: u32, v: u32) -> u32 { self.w(); self.hit(a, true); self.set_cell(a, v as u64); BUS_OK }
    fn write64(&self, a: u32, v: u64) -> u32 { self.w(); self.hit(a, true); self.set_cell(a, v); BUS_OK }
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
    pub macepci: MacePci,
    pub ust: MaceUst,
    pub nic_trace: NicTrace,
    pub pci_view: PciNativeView,
    /// Accesses that hit nothing, first 64 kept, as `(addr, is_write)`.
    unmapped: Mutex<Vec<(u32, bool)>>,
    /// 64-bit RAM accesses at the addresses POST's memory sizing uses, as
    /// `(addr, is_write, value)`. The test writes `(!a << 32) | a` to each and
    /// reads it back, so seeing both halves is what tells us whether the store
    /// or the load is the one going wrong.
    sizemem: Mutex<Vec<(u32, bool, u64)>>,
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
            macepci: MacePci::new(),
            ust: MaceUst::new(),
            nic_trace: NicTrace::new(),
            pci_view: PciNativeView::new(),
            unmapped: Mutex::new(Vec::new()),
            sizemem: Mutex::new(Vec::new()),
        }
    }

    /// The addresses POST's sizing test probes.
    pub const SIZEMEM_PROBES: [u32; 4] = [
        RAM_BASE,
        RAM_BASE + 0x01ff_fff8,
        RAM_BASE + 0x0200_0000,
        RAM_BASE + 0x07ff_fff8,
    ];

    pub fn sizemem_trace(&self) -> Vec<(u32, bool, u64)> {
        self.sizemem.lock().unwrap().clone()
    }

    /// True for POST's address/complement signature, `(!a << 32) | a`.
    fn is_probe_pattern(val: u64) -> bool {
        let lo = val as u32;
        let hi = (val >> 32) as u32;
        hi == !lo
    }

    fn note_sizemem(&self, addr: u32, write: bool, val: u64) {
        if Self::SIZEMEM_PROBES.contains(&addr) || Self::is_probe_pattern(val) {
            let mut t = self.sizemem.lock().unwrap();
            if t.len() < 64 {
                t.push((addr, write, val));
            }
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

    /// Offset of `addr` within RAM, if it is in RAM.
    pub fn ram_offset(&self, addr: u32) -> Option<u32> {
        self.in_ram(addr).then(|| addr - RAM_BASE)
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

    fn is_nic_reg(&self, addr: u32) -> bool {
        (addr & !3) == MACE_ISA_FLASH_NIC_REG
    }

    fn in_ust(&self, addr: u32) -> bool {
        addr >= MACE_UST_MSC && addr < MACE_UST_MSC + MACE_UST_MSC_SIZE
    }

    fn in_macepci(&self, addr: u32) -> bool {
        addr >= MACEPCI_BASE && addr < MACEPCI_BASE + MACEPCI_SIZE
    }

    /// Always false for now: the address `macereg.h` gives for the PCI native
    /// view is where main memory actually lives on this machine, and RAM has
    /// the stronger claim — POST writes its sizing patterns there. Kept as a
    /// hook so the window can be restored somewhere else once its real
    /// placement is known.
    fn in_pci_view(&self, _addr: u32) -> bool {
        false
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
        if self.in_macepci(addr) {
            return BusRead8::ok(self.macepci.read32(addr & !3).data as u8);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.read8(addr);
        }
        if self.is_nic_reg(addr) {
            let v = self.mace.read8(addr).data;
            self.nic_trace.record(self.ust.now(), false, v);
            return BusRead8::ok(v);
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
        if self.in_macepci(addr) {
            return BusRead16::ok(self.macepci.read32(addr & !3).data as u16);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.read16(addr);
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
        if self.in_ust(addr) {
            return self.ust.read32(addr);
        }
        if self.in_macepci(addr) {
            return self.macepci.read32(addr);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.read32(addr);
        }
        if self.is_nic_reg(addr) {
            let v = self.mace.read32(addr).data;
            self.nic_trace.record(self.ust.now(), false, v as u8);
            return BusRead32::ok(v);
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
        if self.in_ust(addr) {
            return self.ust.read64(addr);
        }
        if self.in_macepci(addr) {
            return self.macepci.read64(addr);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.read64(addr);
        }
        if self.is_nic_reg(addr) {
            let v = self.mace.read64(addr).data;
            self.nic_trace.record(self.ust.now(), false, v as u8);
            return BusRead64::ok(v);
        }
        if self.in_mace(addr) {
            return self.mace.read64(addr);
        }
        let r = bus_read!(self, addr, 8, BusRead64::ok, |v: u64| v);
        self.note_sizemem(addr, false, r.data);
        r
    }

    fn write8(&self, addr: u32, val: u8) -> u32 {
        if self.in_crime(addr) {
            return self.crime.write32(addr & !3, val as u32);
        }
        if self.in_macepci(addr) {
            return self.macepci.write32(addr & !3, val as u32);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.write8(addr, val);
        }
        if self.is_nic_reg(addr) {
            self.nic_trace.record(self.ust.now(), true, val);
            return self.mace.write8(addr, val);
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
        if self.in_macepci(addr) {
            return self.macepci.write32(addr & !3, val as u32);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.write16(addr, val);
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
        if self.in_ust(addr) {
            return self.ust.write32(addr, val);
        }
        if self.in_macepci(addr) {
            return self.macepci.write32(addr, val);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.write32(addr, val);
        }
        if self.is_nic_reg(addr) {
            self.nic_trace.record(self.ust.now(), true, val as u8);
            return self.mace.write32(addr, val);
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
        if self.in_ust(addr) {
            return self.ust.write64(addr, val);
        }
        if self.in_macepci(addr) {
            return self.macepci.write64(addr, val);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.write64(addr, val);
        }
        if self.is_nic_reg(addr) {
            self.nic_trace.record(self.ust.now(), true, val as u8);
            return self.mace.write64(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write64(addr, val);
        }
        self.note_sizemem(addr, true, val);
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
    fn ram_is_reachable_at_its_non_zero_base() {
        let bus = Ip32Bus::new(1 << 20, vec![0u8; PROM_SIZE as usize]);
        assert_eq!(bus.write32(RAM_BASE, 0xdead_beef), BUS_OK);
        assert_eq!(bus.read32(RAM_BASE).data, 0xdead_beef);
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
        let mut max_prom_pc = 0u32;
        const LIMIT: u64 = 50_000_000;

        while steps < LIMIT {
            let pc = exec.core.pc as u32;
            if pc == POST1_ENTRY {
                reached_post1 = true;
                break;
            }
            if (0xbfc0_0000..0xbfc8_0000).contains(&pc) && pc > max_prom_pc {
                max_prom_pc = pc;
            }
            exec.step_int();
            steps += 1;
        }

        let pc = exec.core.pc as u32;
        eprintln!("ip32: stopped after {steps} steps at PC 0x{pc:08x}{}",
                  if reached_post1 { "  <-- post1 entry" } else { "" });
        eprintln!("ip32: furthest PC seen inside the PROM: 0x{max_prom_pc:08x}");

        let touched = bus.crime.touched();
        eprintln!("ip32: CRIME registers touched ({}):", touched.len());
        for (off, write) in &touched {
            eprintln!("   0x{:04x} {}", off, if *write { "W" } else { "R" });
        }

        let sm = bus.sizemem_trace();
        eprintln!("ip32: SizeMEM probe traffic ({} events):", sm.len());
        for (a, w, v) in sm.iter().take(24) {
            // An access inside the PROM window is the firmware reading its own
            // expected-value table, not a probe of RAM. Only RAM traffic here
            // is evidence about memory.
            let where_ = if (PROM_BASE..PROM_BASE + PROM_SIZE).contains(a) {
                "prom table"
            } else {
                "RAM"
            };
            eprintln!("   0x{:08x} {} 0x{:016x}  ({})", a, if *w { "W" } else { "R" }, v, where_);
        }

        eprintln!("ip32: CRIME writes in order:");
        for (off, val) in bus.crime.written().iter().take(20) {
            eprintln!("   0x{:04x} <- 0x{:016x}", off, val);
        }

        let (mr, mw) = bus.mace.counts();
        eprintln!("ip32: MACE accesses: {mr} reads, {mw} writes");

        let ev = bus.nic_trace.events();
        eprintln!("ip32: 1-Wire register activity, first 40 of {} events:", ev.len());
        let mut prev = 0u64;
        for (t, w, v) in ev.iter().take(40) {
            let dt = t.saturating_sub(prev);
            prev = *t;
            eprintln!("   +{:>8}  {}  0x{:02x}  [we={} deassert={} data={}]",
                      dt, if *w { "W" } else { "R" }, v,
                      v & nic_bit::FLASH_WE != 0,
                      v & nic_bit::DEASSERT != 0,
                      v & nic_bit::DATA != 0);
        }

        eprintln!("ip32: busiest MACE offsets:");
        for (off, r, w) in bus.mace.hottest().into_iter().take(8) {
            eprintln!("   +0x{:06x}  {:>8} R  {:>4} W", off, r, w);
        }

        let probed = bus.macepci.probed();
        eprintln!("ip32: PCI config addresses selected ({}):", probed.len());
        for a in probed.iter().take(16) {
            // MACE tags are bus/dev/func/reg packed the usual way.
            eprintln!("   0x{:08x}  bus {} dev {:2} fn {} reg 0x{:02x}",
                      a, (a >> 16) & 0xff, (a >> 11) & 0x1f, (a >> 8) & 7, a & 0xfc);
        }
        let (pr, pw) = bus.pci_view.counts();
        eprintln!("ip32: PCI native-view accesses: {pr} reads, {pw} writes");

        let un = bus.unmapped();
        eprintln!("ip32: unmapped accesses ({}):", un.len());
        for (a, w) in un.iter().take(24) {
            eprintln!("   0x{:08x} {}", a, if *w { "W" } else { "R" });
        }
    }
}

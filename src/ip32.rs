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

use std::sync::atomic::{AtomicU32, Ordering};
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

/// `com0`, the PROM's console UART. NetBSD attaches it at `mace0 offset
/// 0x390000`; `com1` follows at 0x398000.
pub const MACE_COM0: u32 = MACE_BASE + 0x0039_0000;
pub const MACE_COM1: u32 = MACE_BASE + 0x0039_8000;
pub const MACE_COM_SIZE: u32 = 0x0000_0800;

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

/// CRIME's rendering-engine block. NetBSD's `crmfb` maps exactly
/// `0x15000000, 0x6000` for this. Graphics are out of scope, but POST touches
/// it regardless, so it is present and inert rather than absent.
pub const CRIME_RE_BASE: u32 = 0x1500_0000;
pub const CRIME_RE_SIZE: u32 = 0x0000_6000;

/// Low memory aliases the base of RAM.
///
/// POST writes a walking pattern to physical 0x00..0x20 from 0xbfc051b4, long
/// after it has sized memory at [`RAM_BASE`]. Without an alias those are
/// unmapped, the store takes a data bus error, and the PROM spins in its
/// exception vector. The Indy models the same idea (`ALIAS_BASE` in
/// `physical.rs`), so the shape is familiar even though the base differs.
///
/// The alias covers as much as there is RAM. POST walks well past the first
/// page, and since the lowest device sits at 0x14000000 an alias of up to
/// 128 MB cannot shadow anything.

/// The memory window CRIME decodes: eight banks of 128 MB from [`RAM_BASE`].
///
/// Whether a bank is *populated* is a separate question from whether it is
/// decoded. POST probes every bank in turn; an absent one must read back
/// something that fails its pattern check, **not** raise a bus error. Erroring
/// sends the PROM into its bus-error handler, which on an unpopulated machine
/// immediately faults again — a two-address exception loop between `jr s8` and
/// whatever `s8` happens to hold.
pub const RAM_WINDOW_SIZE: u32 = 8 * 128 * 1024 * 1024;

/// Boot PROM. Same address as the Indy's, which is the one thing that carries
/// over: the MIPS reset vector is 0xbfc00000 on both.
pub const PROM_BASE: u32 = 0x1fc0_0000;
pub const PROM_SIZE: u32 = 0x0008_0000; // 512 KiB

/// sloader's serial prompt: "SL", 9600 baud, 8 data bits, even parity. Seeing
/// this means POST has completed and the PROM is waiting on console input.
pub const SLOADER_PROMPT: &str = "SL-9600-8E>";

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

/// A 16550 with just enough behaviour to be written to.
///
/// Register `n` lives at `base + (n << 8) + 7`: MACE spaces its ISA registers
/// 256 bytes apart and puts the byte in the last lane of the 64-bit slot
/// (`sgimips/bus.c`: `h + (o << 8) + 7`). Getting this wrong is silent — every
/// register reads back zero and the firmware simply never transmits.
///
/// Transmit only. The line status register always reports the holding register
/// empty, so the PROM never waits on us, and bytes are appended to a buffer
/// rather than going anywhere.
pub struct Com16550 {
    out: Mutex<Vec<u8>>,
    /// Characters waiting to be read by the guest. The PROM polls LSR for
    /// data-ready and then reads the receive register, so an empty queue simply
    /// means "nobody has typed anything".
    input: Mutex<std::collections::VecDeque<u8>>,
    regs: Mutex<[u8; 8]>,
}

pub mod com_reg {
    /// Transmit holding / receive buffer (and divisor low when DLAB is set).
    pub const THR: u32 = 0;
    pub const IER: u32 = 1;
    pub const IIR_FCR: u32 = 2;
    pub const LCR: u32 = 3;
    pub const MCR: u32 = 4;
    pub const LSR: u32 = 5;
    pub const MSR: u32 = 6;
    pub const SCR: u32 = 7;

    /// LCR bit 7: the next accesses to 0 and 1 are the baud divisor.
    pub const LCR_DLAB: u8 = 0x80;
    /// LSR: a received character is waiting.
    pub const LSR_DR: u8 = 0x01;
    /// LSR: transmit holding register empty.
    pub const LSR_THRE: u8 = 0x20;
    /// LSR: transmitter completely empty.
    pub const LSR_TEMT: u8 = 0x40;
}

impl Default for Com16550 {
    fn default() -> Self { Self::new() }
}

impl Com16550 {
    pub fn new() -> Self {
        Self {
            out: Mutex::new(Vec::new()),
            input: Mutex::new(Default::default()),
            regs: Mutex::new([0u8; 8]),
        }
    }

    /// Queue characters for the guest to read.
    pub fn feed(&self, bytes: &[u8]) {
        self.input.lock().unwrap().extend(bytes.iter().copied());
    }

    /// Characters still unread by the guest.
    pub fn pending_input(&self) -> usize {
        self.input.lock().unwrap().len()
    }

    /// Everything written to the transmit register so far.
    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).to_string()
    }

    pub fn bytes_out(&self) -> usize {
        self.out.lock().unwrap().len()
    }

    /// Decode a MACE ISA address into a 16550 register number, if it names one.
    fn reg_of(addr: u32) -> Option<u32> {
        // Only the +7 lane carries the byte.
        if addr & 7 != 7 {
            return None;
        }
        let n = (addr >> 8) & 7;
        Some(n)
    }

    fn dlab(&self) -> bool {
        self.regs.lock().unwrap()[com_reg::LCR as usize] & com_reg::LCR_DLAB != 0
    }

    pub fn read_reg(&self, addr: u32) -> u8 {
        let Some(n) = Self::reg_of(addr) else { return 0 };
        match n {
            // Always ready to take another byte, and data-ready whenever
            // something has been typed.
            com_reg::LSR => {
                let mut v = com_reg::LSR_THRE | com_reg::LSR_TEMT;
                if !self.input.lock().unwrap().is_empty() {
                    v |= com_reg::LSR_DR;
                }
                v
            }
            // Register 0 reads the receive buffer (unless DLAB selects the
            // divisor, which nothing reads back).
            com_reg::THR if !self.dlab() => {
                self.input.lock().unwrap().pop_front().unwrap_or(0)
            }
            _ => self.regs.lock().unwrap()[n as usize],
        }
    }

    pub fn write_reg(&self, addr: u32, val: u8) {
        let Some(n) = Self::reg_of(addr) else { return };
        if n == com_reg::THR && !self.dlab() {
            self.out.lock().unwrap().push(val);
            return;
        }
        self.regs.lock().unwrap()[n as usize] = val;
    }
}

impl BusDevice for Com16550 {
    // Every width must be answered. An unimplemented one falls through to the
    // trait default, which reports a bus error — and a DBE inside POST looks
    // like a missing device rather than a missing method.
    fn read8(&self, addr: u32) -> BusRead8 { BusRead8::ok(self.read_reg(addr)) }
    fn read16(&self, addr: u32) -> BusRead16 { BusRead16::ok(self.read_reg(addr | 7) as u16) }
    fn write16(&self, addr: u32, val: u16) -> u32 { self.write_reg(addr | 7, val as u8); BUS_OK }
    fn write8(&self, addr: u32, val: u8) -> u32 { self.write_reg(addr, val); BUS_OK }
    fn read32(&self, addr: u32) -> BusRead32 { BusRead32::ok(self.read_reg(addr | 7) as u32) }
    fn write32(&self, addr: u32, val: u32) -> u32 { self.write_reg(addr | 7, val as u8); BUS_OK }
    fn read64(&self, addr: u32) -> BusRead64 { BusRead64::ok(self.read_reg(addr | 7) as u64) }
    fn write64(&self, addr: u32, val: u64) -> u32 { self.write_reg(addr | 7, val as u8); BUS_OK }
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
    pub crime_re: Stub,
    pub macepci: MacePci,
    pub ust: MaceUst,
    pub nic_trace: NicTrace,
    pub com0: Com16550,
    pub com1: Com16550,
    pub pci_view: PciNativeView,
    /// Accesses that hit nothing, first 64 kept, as `(addr, is_write, pc)`.
    unmapped: Mutex<Vec<(u32, bool, u32)>>,
    /// The PC of the instruction currently executing.
    pub pc: PcTap,
    /// 64-bit RAM accesses at the addresses POST's memory sizing uses, as
    /// `(addr, is_write, value, pc)`. The test writes `(!a << 32) | a` to each and
    /// reads it back, so seeing both halves is what tells us whether the store
    /// or the load is the one going wrong.
    sizemem: Mutex<Vec<(u32, bool, u64, u32)>>,
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
            crime_re: Stub::new("crime-re"),
            macepci: MacePci::new(),
            ust: MaceUst::new(),
            nic_trace: NicTrace::new(),
            com0: Com16550::new(),
            com1: Com16550::new(),
            pci_view: PciNativeView::new(),
            unmapped: Mutex::new(Vec::new()),
            pc: PcTap::default(),
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

    pub fn sizemem_trace(&self) -> Vec<(u32, bool, u64, u32)> {
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
            let pc = self.pc.get();
            let mut t = self.sizemem.lock().unwrap();
            if t.len() < 64 {
                t.push((addr, write, val, pc));
            }
        }
    }

    pub fn unmapped(&self) -> Vec<(u32, bool, u32)> {
        self.unmapped.lock().unwrap().clone()
    }

    fn miss(&self, addr: u32, write: bool) {
        let pc = self.pc.get();
        let mut u = self.unmapped.lock().unwrap();
        if u.len() < 64 && !u.iter().any(|&(a, w, _)| a == addr && w == write) {
            u.push((addr, write, pc));
        }
    }

    fn ram_len(&self) -> u32 {
        self.ram.lock().unwrap().len() as u32
    }

    fn in_ram(&self, addr: u32) -> bool {
        addr >= RAM_BASE && addr < RAM_BASE.wrapping_add(self.ram_len())
    }

    /// Inside CRIME's memory window but behind a bank with no SIMM in it.
    fn in_unpopulated_ram(&self, addr: u32) -> bool {
        !self.in_ram(addr)
            && addr >= RAM_BASE
            && addr < RAM_BASE.wrapping_add(RAM_WINDOW_SIZE)
    }

    /// Offset of `addr` within RAM, if it is in RAM.
    pub fn ram_offset(&self, addr: u32) -> Option<u32> {
        self.in_ram(addr).then(|| addr - RAM_BASE)
    }

    fn in_prom(&self, addr: u32) -> bool {
        addr >= PROM_BASE && addr < PROM_BASE + PROM_SIZE
    }

    fn in_crime_re(&self, addr: u32) -> bool {
        addr >= CRIME_RE_BASE && addr < CRIME_RE_BASE + CRIME_RE_SIZE
    }

    fn in_crime(&self, addr: u32) -> bool {
        addr >= CRIME_BASE && addr < CRIME_BASE + CRIME_SIZE
    }

    fn in_mace(&self, addr: u32) -> bool {
        addr >= MACE_BASE && addr < MACE_BASE + MACE_SIZE
    }

    fn com_for(&self, addr: u32) -> Option<&Com16550> {
        if (MACE_COM0..MACE_COM0 + MACE_COM_SIZE).contains(&addr) {
            Some(&self.com0)
        } else if (MACE_COM1..MACE_COM1 + MACE_COM_SIZE).contains(&addr) {
            Some(&self.com1)
        } else {
            None
        }
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

    /// Map an address through the low-memory alias, if it falls in it.
    fn resolve(&self, addr: u32) -> u32 {
        if addr < self.ram_len() {
            RAM_BASE.wrapping_add(addr)
        } else {
            addr
        }
    }

    fn read_bytes(&self, addr: u32, n: usize) -> Option<u64> {
        let addr = self.resolve(addr);
        // Decoded but not populated: give back a value that cannot pass POST's
        // address/complement check, so sizing concludes "no SIMM" and moves on.
        if self.in_unpopulated_ram(addr) {
            return Some(0);
        }
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
        let addr = self.resolve(addr);
        // Writes into an empty bank are swallowed by the memory controller.
        if self.in_unpopulated_ram(addr) {
            return true;
        }
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
        if self.in_crime_re(addr) {
            return BusRead8::ok(self.crime_re.read32(addr & !3).data as u8);
        }
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
        if let Some(c) = self.com_for(addr) {
            return c.read8(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read8(addr);
        }
        bus_read!(self, addr, 1, BusRead8::ok, |v: u64| v as u8)
    }

    fn read16(&self, addr: u32) -> BusRead16 {
        if self.in_crime_re(addr) {
            return BusRead16::ok(self.crime_re.read32(addr & !3).data as u16);
        }
        if self.in_crime(addr) {
            return BusRead16::ok(self.crime.read32(addr & !3).data as u16);
        }
        if self.in_macepci(addr) {
            return BusRead16::ok(self.macepci.read32(addr & !3).data as u16);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.read16(addr);
        }
        if let Some(c) = self.com_for(addr) {
            return c.read16(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read16(addr);
        }
        bus_read!(self, addr, 2, BusRead16::ok, |v: u64| v as u16)
    }

    fn read32(&self, addr: u32) -> BusRead32 {
        if self.in_crime_re(addr) {
            return self.crime_re.read32(addr);
        }
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
        if let Some(c) = self.com_for(addr) {
            return c.read32(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read32(addr);
        }
        bus_read!(self, addr, 4, BusRead32::ok, |v: u64| v as u32)
    }

    fn read64(&self, addr: u32) -> BusRead64 {
        if self.in_crime_re(addr) {
            return self.crime_re.read64(addr);
        }
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
        if let Some(c) = self.com_for(addr) {
            return c.read64(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read64(addr);
        }
        let r = bus_read!(self, addr, 8, BusRead64::ok, |v: u64| v);
        self.note_sizemem(addr, false, r.data);
        r
    }

    fn write8(&self, addr: u32, val: u8) -> u32 {
        if self.in_crime_re(addr) {
            return self.crime_re.write32(addr & !3, val as u32);
        }
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
        if let Some(c) = self.com_for(addr) {
            return c.write8(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write8(addr, val);
        }
        if self.write_bytes(addr, 1, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write16(&self, addr: u32, val: u16) -> u32 {
        if self.in_crime_re(addr) {
            return self.crime_re.write32(addr & !3, val as u32);
        }
        if self.in_crime(addr) {
            return self.crime.write32(addr & !3, val as u32);
        }
        if self.in_macepci(addr) {
            return self.macepci.write32(addr & !3, val as u32);
        }
        if self.in_pci_view(addr) {
            return self.pci_view.write16(addr, val);
        }
        if let Some(c) = self.com_for(addr) {
            return c.write16(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write16(addr, val);
        }
        if self.write_bytes(addr, 2, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write32(&self, addr: u32, val: u32) -> u32 {
        if self.in_crime_re(addr) {
            return self.crime_re.write32(addr, val);
        }
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
        if let Some(c) = self.com_for(addr) {
            return c.write32(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write32(addr, val);
        }
        if self.write_bytes(addr, 4, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write64(&self, addr: u32, val: u64) -> u32 {
        if self.in_crime_re(addr) {
            return self.crime_re.write64(addr, val);
        }
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
        if let Some(c) = self.com_for(addr) {
            return c.write64(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write64(addr, val);
        }
        self.note_sizemem(addr, true, val);
        if self.write_bytes(addr, 8, val) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }
}

// ── Instrumentation ─────────────────────────────────────────────────────────
//
// Three things, each chosen because its absence cost real time during
// bring-up:
//
// 1. **Every recorded access carries the PC that issued it.** The very first
//    run bus-errored on a store to 0x40000000, and it was read as "the PROM is
//    enumerating PCI" because a header names a PCI constant there. Had the log
//    said the store came from inside the memory-sizing routine, the real answer
//    — that RAM is based at 0x40000000 — would have been immediate.
//
// 2. **A printf tap.** post1's print routine is a stub that emits nothing, so
//    a console cannot show POST's narrative. But the format strings are in the
//    image and the arguments are in registers at the call. Reading them at the
//    call site recovers the narrative the hardware never prints.
//
// 3. **Stall detection that says what the loop touched.** "Stopped at PC X" is
//    nearly useless; "spun 390k times between PC A and B, reading only
//    MACE+0x340000" names the missing device immediately.

/// The PC of the instruction currently executing, published by the harness so
/// devices can attribute accesses. A plain atomic: this is a bring-up harness,
/// and correlating a store with its code site is worth an atomic store per
/// instruction.
#[derive(Default)]
pub struct PcTap(AtomicU32);

impl PcTap {
    pub fn set(&self, pc: u32) {
        self.0.store(pc, Ordering::Relaxed);
    }
    pub fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Watches the PC for a loop that is going nowhere.
///
/// Keeps the distinct PCs seen in the current window. When the window fills
/// without the set growing beyond `spread`, the guest is spinning, and the set
/// is the loop body.
pub struct StallDetector {
    window: usize,
    spread: usize,
    seen: std::collections::BTreeSet<u32>,
    count: usize,
}

impl StallDetector {
    pub fn new(window: usize, spread: usize) -> Self {
        Self { window, spread, seen: Default::default(), count: 0 }
    }

    /// Feed one PC. Returns the loop body once a stall is recognised.
    pub fn step(&mut self, pc: u32) -> Option<Vec<u32>> {
        self.seen.insert(pc);
        self.count += 1;
        if self.count < self.window {
            return None;
        }
        let stalled = self.seen.len() <= self.spread;
        let body: Vec<u32> = self.seen.iter().copied().collect();
        self.seen.clear();
        self.count = 0;
        stalled.then_some(body)
    }
}

/// Recovers POST's intended messages by reading them at the call site.
///
/// The PROM's print routine takes a format string in `a0` and up to three
/// arguments in `a1`-`a3`, and — in post1 — throws them away. Catching the call
/// and resolving `a0` against the PROM image gives the message anyway.
pub struct PrintfTap {
    /// Address of the routine to watch.
    pub entry: u32,
    lines: Mutex<Vec<String>>,
}

impl PrintfTap {
    /// post1's print routine: saves its arguments and returns.
    pub const POST1_PRINTF: u32 = 0xbfc0_4d74;

    pub fn new(entry: u32) -> Self {
        Self { entry, lines: Mutex::new(Vec::new()) }
    }

    /// Call when the PC reaches `entry`. `args` are a0..a3.
    pub fn on_call(&self, prom: &[u8], args: [u64; 4]) {
        let fmt = read_prom_cstr(prom, args[0] as u32);
        let Some(fmt) = fmt else { return };
        self.lines.lock().unwrap().push(format_prom(&fmt, &args[1..]));
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

/// Read a NUL-terminated string out of the PROM image, given a guest address
/// anywhere in the PROM window. Returns `None` for addresses outside it.
pub fn read_prom_cstr(prom: &[u8], addr: u32) -> Option<String> {
    // Accept KSEG0/KSEG1 aliases as well as the bare physical address.
    let phys = addr & 0x1fff_ffff;
    if !(PROM_BASE & 0x1fff_ffff..(PROM_BASE & 0x1fff_ffff) + PROM_SIZE).contains(&phys) {
        return None;
    }
    let off = (phys - (PROM_BASE & 0x1fff_ffff)) as usize;
    let end = prom[off..].iter().position(|&b| b == 0)? + off;
    if end == off {
        return None;
    }
    Some(String::from_utf8_lossy(&prom[off..end]).to_string())
}

/// Substitute `%d`/`%x`/`%lx`/`%s` style conversions with the supplied
/// arguments. Deliberately approximate: the point is to read the message, not
/// to reimplement printf.
pub fn format_prom(fmt: &str, args: &[u64]) -> String {
    let mut out = String::new();
    let mut it = fmt.chars().peekable();
    let mut argi = 0usize;
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Skip flags, width and length modifiers.
        let mut spec = String::new();
        while let Some(&n) = it.peek() {
            it.next();
            if n.is_ascii_alphabetic() {
                spec.push(n);
                break;
            }
        }
        let a = args.get(argi).copied().unwrap_or(0);
        match spec.chars().last() {
            Some('d') | Some('i') => { out.push_str(&format!("{}", a as i64)); argi += 1 }
            Some('u') => { out.push_str(&format!("{a}")); argi += 1 }
            Some('x') | Some('X') | Some('p') => { out.push_str(&format!("{a:x}")); argi += 1 }
            Some('c') => { out.push(a as u8 as char); argi += 1 }
            Some('s') => { out.push_str("<str>"); argi += 1 }
            Some('%') => out.push('%'),
            _ => out.push_str(&format!("%{spec}")),
        }
    }
    out
}

/// MIPS CP0 Cause.ExcCode, named. An unhandled trap is usually diagnosed
/// entirely from this plus BadVAddr.
pub fn exc_name(code: u32) -> &'static str {
    match code {
        0 => "Int", 1 => "TLBMod", 2 => "TLBL", 3 => "TLBS", 4 => "AdEL",
        5 => "AdES", 6 => "IBE", 7 => "DBE", 8 => "Sys", 9 => "Bp",
        10 => "RI", 11 => "CpU", 12 => "Ov", 13 => "Tr", 15 => "FPE",
        23 => "WATCH", 31 => "VCED",
        _ => "?",
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
        assert_eq!(bus.unmapped(), vec![(PROM_BASE, true, 0)], "and record the attempt");
    }

    /// MACE spaces ISA registers 256 bytes apart with the byte in the last lane
    /// (`sgimips/bus.c`: `h + (o << 8) + 7`). Decode this wrong and the UART is
    /// silently inert — every register reads zero and nothing is ever sent.
    #[test]
    fn the_uart_decodes_maces_register_spacing() {
        let c = Com16550::new();
        // Transmit register is offset 0, so byte lane +7 of the first slot.
        c.write8(MACE_COM0 + 7, b'A');
        assert_eq!(c.output(), "A");
        // Anything not in the +7 lane is not a register.
        c.write8(MACE_COM0 + 3, b'X');
        assert_eq!(c.output(), "A", "only the +7 lane carries the byte");
        // Line status lives at register 5, i.e. +0x507.
        let lsr = c.read8(MACE_COM0 + 0x507).data;
        assert_eq!(lsr, com_reg::LSR_THRE | com_reg::LSR_TEMT,
                   "the holding register must always read empty or the PROM waits forever");
    }

    /// With DLAB set, writes to register 0 are the baud divisor, not characters.
    #[test]
    fn the_baud_divisor_is_not_mistaken_for_output() {
        let c = Com16550::new();
        c.write8(MACE_COM0 + 0x307, com_reg::LCR_DLAB);
        c.write8(MACE_COM0 + 7, 0x0c);
        assert_eq!(c.output(), "", "divisor writes are not characters");
        c.write8(MACE_COM0 + 0x307, 0x03);
        c.write8(MACE_COM0 + 7, b'B');
        assert_eq!(c.output(), "B");
    }

    /// The PROM polls the line status for data-ready and then reads the
    /// receive register. Both halves have to work or it waits forever at its
    /// prompt — which looks exactly like a hang.
    #[test]
    fn a_typed_character_becomes_readable() {
        let c = Com16550::new();
        let lsr = MACE_COM0 + 0x507;
        assert_eq!(c.read8(lsr).data & com_reg::LSR_DR, 0, "nothing typed yet");

        c.feed(b"hi");
        assert_ne!(c.read8(lsr).data & com_reg::LSR_DR, 0, "data-ready must be set");
        assert_eq!(c.read8(MACE_COM0 + 7).data, b'h');
        assert_eq!(c.read8(MACE_COM0 + 7).data, b'i');
        assert_eq!(c.read8(lsr).data & com_reg::LSR_DR, 0, "queue drained");
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

    /// Run the PROM from reset and require that it reaches post1.
    ///
    /// sloader sizes memory, copies post1 into RAM and jumps to it. Reaching
    /// [`POST1_ENTRY`] means every gate before that is satisfied: CRIME's bank
    /// controls, the UST timer, the low-memory alias, and a memory window that
    /// absorbs accesses to unpopulated banks instead of bus-erroring.
    ///
    /// The printed trace is still the point of the test — it is what makes the
    /// next gate findable — but the milestone itself is now checked.
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
        let bus_prom = prom.clone();
        let bus = Arc::new(Ip32Bus::new(128 << 20, prom));
        let sysad: Arc<dyn crate::traits::BusDevice> = bus.clone();
        let cfg = MipsCpuConfig::indy();
        let tlb = MipsTlb::new(cfg.tlb_entries);
        let mut exec: MipsExecutor<MipsTlb, R5000Cache> = MipsExecutor::new(sysad, tlb, &cfg);

        let mut reached_post1 = false;
        let mut steps = 0u64;
        let mut max_prom_pc = 0u32;
        let mut max_post1_pc = 0u32;
        let mut post1_at = 0u64;
        let mut fed = false;
        let mut fed_at = 0u64;
        let feed_bytes: Option<Vec<u8>> = std::env::var("IRIS_IP32_INPUT")
            .ok()
            .map(|v| v.replace("\\r", "\r").into_bytes());
        const LIMIT: u64 = 50_000_000;

        let printf = PrintfTap::new(PrintfTap::POST1_PRINTF);
        let mut stall = StallDetector::new(200_000, 24);
        // A stall inside the exception vectors is an unhandled trap, and the
        // only useful question then is which one. Capture CP0 at the moment it
        // is recognised rather than making someone reproduce it by hand.
        let mut stalls: Vec<(u64, Vec<u32>, u32, u64, u64)> = Vec::new();
        let prom_for_strings = bus_prom.clone();

        while steps < LIMIT {
            let pc = exec.core.pc as u32;
            if pc == POST1_ENTRY {
                if !reached_post1 {
                    reached_post1 = true;
                    post1_at = steps;
                }
            }
            if reached_post1 && (POST1_ENTRY..POST1_ENTRY + 0x8000).contains(&pc) && pc > max_post1_pc {
                max_post1_pc = pc;
            }
            // Publish the PC so devices can attribute the accesses this
            // instruction is about to make.
            bus.pc.set(pc);

            if pc == printf.entry {
                let a = &exec.core.gpr;
                printf.on_call(&prom_for_strings, [a[4], a[5], a[6], a[7]]);
            }

            if let Some(body) = stall.step(pc) {
                if stalls.len() < 8 {
                    let cause = exec.core.cp0_cause as u32;
                    stalls.push((steps, body, cause, exec.core.cp0_epc, exec.core.cp0_badvaddr));
                }
            }

            // Optionally answer sloader's prompt, for experimenting with what
            // it accepts. Off unless IRIS_IP32_INPUT is set, and `contains` is
            // only evaluated while it is: scanning the output buffer on every
            // instruction is not something to do by default.
            if let Some(feed) = feed_bytes.as_ref() {
                if !fed && bus.com0.output().contains(SLOADER_PROMPT) {
                    bus.com0.feed(feed);
                    fed = true;
                    fed_at = steps;
                }
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
        if reached_post1 {
            eprintln!("ip32: entered post1 after {post1_at} steps; furthest PC in post1: 0x{max_post1_pc:08x}");
        }

        let msgs = printf.lines();
        eprintln!("ip32: ===== POST messages, recovered at the call site ({}) =====", msgs.len());
        for m in msgs.iter().take(40) {
            eprint!("   | {m}");
            if !m.ends_with('\n') { eprintln!(); }
        }
        eprintln!("ip32: ===== end POST messages =====");

        assert!(
            reached_post1,
            "sloader did not reach post1 at 0x{POST1_ENTRY:08x}; it stopped at \
             0x{:08x} after {steps} steps. The trace above says what it wanted.",
            exec.core.pc as u32,
        );
        assert!(bus.unmapped().is_empty(), "unmapped accesses: {:x?}", bus.unmapped());

        eprintln!("ip32: stalls detected: {}", stalls.len());
        for (at, body, cause, epc, bad) in stalls.iter().take(3) {
            let lo = body.first().copied().unwrap_or(0);
            let hi = body.last().copied().unwrap_or(0);
            let exc = (cause >> 2) & 0x1f;
            eprintln!("   after {at} steps: {} distinct PCs in 0x{lo:08x}..0x{hi:08x}", body.len());
            eprintln!("      CP0 Cause=0x{cause:08x} ExcCode={exc} ({}) EPC=0x{epc:08x} BadVAddr=0x{bad:08x}",
                      exc_name(exc));
        }

        let console = bus.com0.output();
        eprintln!("ip32: ===== PROM console output ({} bytes) =====", console.len());
        for line in console.lines() {
            eprintln!("   | {line}");
        }
        eprintln!("ip32: ===== end console =====");
        if fed {
            eprintln!("ip32: answered the prompt at step {fed_at}; {} byte(s) still unread",
                      bus.com0.pending_input());
        }

        let touched = bus.crime.touched();
        eprintln!("ip32: CRIME registers touched ({}):", touched.len());
        for (off, write) in &touched {
            eprintln!("   0x{:04x} {}", off, if *write { "W" } else { "R" });
        }

        let sm = bus.sizemem_trace();
        eprintln!("ip32: SizeMEM probe traffic ({} events):", sm.len());
        for (a, w, v, pc) in sm.iter().take(24) {
            // An access inside the PROM window is the firmware reading its own
            // expected-value table, not a probe of RAM. Only RAM traffic here
            // is evidence about memory.
            let where_ = if (PROM_BASE..PROM_BASE + PROM_SIZE).contains(a) {
                "prom table"
            } else {
                "RAM"
            };
            eprintln!("   0x{:08x} {} 0x{:016x}  ({})  from PC 0x{:08x}",
                      a, if *w { "W" } else { "R" }, v, where_, pc);
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
        for (a, w, pc) in un.iter().take(24) {
            eprintln!("   0x{:08x} {}  from PC 0x{:08x}", a, if *w { "W" } else { "R" }, pc);
        }
    }
}

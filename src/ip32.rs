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
/// GBE, the graphics back end. We drive the machine on serial and have no
/// interest in its output, but the firmware probes it during startup, so it
/// has to answer. A storing register file is enough for that.
pub const GBE_BASE: u32 = 0x1600_0000;
pub const GBE_SIZE: u32 = 0x0010_0000;

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

/// Where the `firmware` section's SHDR says it loads: KSEG0 0x81000000, i.e.
/// physical 0x01000000. If sloader is meant to hand off to the ARCS monitor,
/// this is where it would put it and where the PC would end up.
pub const FIRMWARE_LOAD_VA: u32 = 0x8100_0000;
pub const FIRMWARE_LOAD_PA: u32 = 0x0100_0000;
pub const FIRMWARE_SPAN: u32 = 0x0006_0000;

/// The RTC and its battery-backed NVRAM, at MACE + 0x3a0000. Registers use
/// MACE's ISA spacing, `(reg << 8) + 7`, exactly like the UART.
pub const MACE_RTC: u32 = MACE_BASE + 0x003a_0000;
pub const MACE_RTC_SIZE: u32 = 0x0001_0000;

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

    /// MCR bit 4: loopback. The transmitter is wired back to the receiver and
    /// the modem control outputs to the modem status inputs. post1's serial
    /// self-test runs entirely in this mode, so a UART without it fails POST.
    pub const MCR_LOOP: u8 = 0x10;
    /// MCR outputs that loop back to MSR while `MCR_LOOP` is set.
    pub const MCR_DTR: u8 = 0x01;
    pub const MCR_RTS: u8 = 0x02;
    pub const MCR_OUT1: u8 = 0x04;
    pub const MCR_OUT2: u8 = 0x08;
    /// MSR inputs, in the order the loopback wires them.
    pub const MSR_CTS: u8 = 0x10;
    pub const MSR_DSR: u8 = 0x20;
    pub const MSR_RI: u8 = 0x40;
    pub const MSR_DCD: u8 = 0x80;
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

    /// True while MCR selects internal loopback.
    fn looped(&self) -> bool {
        self.regs.lock().unwrap()[com_reg::MCR as usize] & com_reg::MCR_LOOP != 0
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
            // In loopback the modem control outputs appear as the status
            // inputs. Nothing in POST depends on this, but a half-wired
            // loopback is the kind of thing that misleads the next reader.
            com_reg::MSR if self.looped() => {
                let mcr = self.regs.lock().unwrap()[com_reg::MCR as usize];
                let mut v = 0;
                for (out, inp) in [
                    (com_reg::MCR_RTS, com_reg::MSR_CTS),
                    (com_reg::MCR_DTR, com_reg::MSR_DSR),
                    (com_reg::MCR_OUT1, com_reg::MSR_RI),
                    (com_reg::MCR_OUT2, com_reg::MSR_DCD),
                ] {
                    if mcr & out != 0 {
                        v |= inp;
                    }
                }
                v
            }
            _ => self.regs.lock().unwrap()[n as usize],
        }
    }

    pub fn write_reg(&self, addr: u32, val: u8) {
        let Some(n) = Self::reg_of(addr) else { return };
        if n == com_reg::THR && !self.dlab() {
            // Loopback: the byte never leaves the chip, it arrives back in
            // the receive register. post1 writes 0..254 this way and insists
            // on reading the same sequence back.
            if self.looped() {
                self.input.lock().unwrap().push_back(val);
            } else {
                self.out.lock().unwrap().push(val);
            }
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

/// A Dallas-style RTC with battery-backed NVRAM.
///
/// Registers are reached with MACE's ISA spacing, `(reg << 8) + 7`. The low
/// registers are the clock; everything from [`NVRAM_FIRST`] up is storage that
/// survives power loss on a real machine — which is precisely why firmware
/// trusts it, and why handing back zeros is not neutral.
pub struct MaceRtc {
    cells: Mutex<Vec<u8>>,
}

impl MaceRtc {
    /// First register that is general-purpose NVRAM rather than clock state.
    pub const NVRAM_FIRST: usize = 0x0e;
    pub const REGS: usize = 0x100;

    pub fn new() -> Self {
        let mut c = vec![0u8; Self::REGS];
        // A plausible stopped-but-valid clock. Register D bit 7 is the
        // valid-RAM-and-time flag: firmware that sees it clear concludes the
        // battery died, which is its own diagnostic path.
        c[0x0d] = 0x80;
        Self { cells: Mutex::new(c) }
    }

    fn reg_of(addr: u32) -> Option<usize> {
        if addr & 7 != 7 {
            return None;
        }
        Some(((addr >> 8) & 0xff) as usize)
    }

    pub fn peek(&self, reg: usize) -> u8 {
        self.cells.lock().unwrap()[reg & 0xff]
    }
    pub fn poke(&self, reg: usize, val: u8) {
        self.cells.lock().unwrap()[reg & 0xff] = val;
    }
}

impl Default for MaceRtc {
    fn default() -> Self { Self::new() }
}

impl BusDevice for MaceRtc {
    fn read8(&self, addr: u32) -> BusRead8 {
        BusRead8::ok(Self::reg_of(addr).map(|r| self.peek(r)).unwrap_or(0))
    }
    fn write8(&self, addr: u32, val: u8) -> u32 {
        if let Some(r) = Self::reg_of(addr) {
            self.poke(r, val);
        }
        BUS_OK
    }
    fn read16(&self, a: u32) -> BusRead16 { BusRead16::ok(self.read8(a | 7).data as u16) }
    fn read32(&self, a: u32) -> BusRead32 { BusRead32::ok(self.read8(a | 7).data as u32) }
    fn read64(&self, a: u32) -> BusRead64 { BusRead64::ok(self.read8(a | 7).data as u64) }
    fn write16(&self, a: u32, v: u16) -> u32 { self.write8(a | 7, v as u8) }
    fn write32(&self, a: u32, v: u32) -> u32 { self.write8(a | 7, v as u8) }
    fn write64(&self, a: u32, v: u64) -> u32 { self.write8(a | 7, v as u8) }
}

/// The Dallas 1-Wire identity chip: 64 bits of ROM holding a family code, a
/// 48-bit serial number and a CRC. SGI machines keep the Ethernet address here.
///
/// The bus is one wire, so everything is encoded in how long the master holds
/// it low. Durations are read from the UST clock, which is why that had to
/// become time-based first.
pub struct OneWireId {
    rom: [u8; 8],
    /// The EPROM data area. Erased cells read as 0xff on a real part, so an
    /// unprogrammed chip is all-ones rather than all-zeros — a distinction
    /// firmware notices.
    data: Vec<u8>,
    /// The status/redirection bytes, likewise erased.
    status: Vec<u8>,
    st: Mutex<OneWireState>,
}

/// What the device is doing between slots. It decides how the next slot is
/// read: while the master still has things to say a slot is a write and is
/// classified by width, and once the device is answering every slot is a read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum OwPhase {
    /// Waiting for a command byte.
    #[default]
    Idle,
    /// Collecting the address bytes a memory command needs.
    WantAddr,
    /// Streaming a reply.
    Sending,
}

#[derive(Default)]
struct OneWireState {
    /// Time the master last pulled the line low.
    low_since: Option<u64>,
    /// Bits received from the master in the current command byte.
    in_bits: u8,
    in_count: u8,
    /// Command being executed, once a whole byte has arrived.
    command: Option<u8>,
    /// Bit position within [`OneWireState::out`].
    out_pos: usize,
    phase: OwPhase,
    /// The memory command waiting for its address bytes.
    pending: u8,
    addr: [u8; 2],
    addr_count: usize,
    /// The bytes the device is sending, LSB first within each.
    out: Vec<u8>,
    /// True when the reply has a definite end after which the device listens
    /// for another command — READ ROM does, a memory read does not.
    out_then_idle: bool,
    /// Decoded protocol events, for the bring-up trace.
    log: Vec<String>,
    /// True once the master has opened a read slot, so a bit is on the line.
    presenting: bool,
    /// The presence pulse, as a window in UST time. Equal bounds mean none.
    presence_from: u64,
    presence_to: u64,
}

pub mod onewire_cmd {
    pub const READ_ROM: u8 = 0x33;
    pub const SKIP_ROM: u8 = 0xcc;
    /// DS2502 memory read: two address bytes follow, then the device sends a
    /// CRC of what it was told and streams data from that address.
    pub const READ_MEMORY: u8 = 0xf0;
    /// As above, but the device also emits a CRC after each page.
    pub const READ_DATA_CRC: u8 = 0xc3;
    /// DS2502 status (the redirection/write-protect bytes), same shape.
    pub const READ_STATUS: u8 = 0xaa;
}

/// Microsecond thresholds. A reset is held for at least 480 µs; within a time
/// slot, a short low is a 1 and a long low is a 0.
/// After the master releases a reset, the device waits this long and then
/// pulls the line low for [`OW_PRESENCE_LEN_US`]. The master samples somewhere
/// in that window; holding the pulse for a fixed time rather than "until
/// somebody looks" is what makes it independent of how often it polls.
/// The part SGI fits, and the one the firmware insists on: a DS2502, family
/// code 0x09. Getting this wrong is not subtle but it is quiet — the firmware
/// reads the first eight bits of the ROM, sees a family it does not recognise,
/// abandons the read and reports `ds2502_read_rom failed`, with the other
/// fifty-six bits never requested. A DS1990A's 0x01 is the obvious wrong
/// answer because it is the one every 1-Wire example uses.
pub const OW_FAMILY_DS2502: u8 = 0x09;

pub const OW_PRESENCE_DELAY_US: u64 = 20;
pub const OW_PRESENCE_LEN_US: u64 = 150;

/// Thresholds, set from the widths this PROM actually produces rather than
/// from the datasheet. Measured on the firmware's own ROM read:
///
/// ```text
/// 31x4 32x20 | 329x1 356x4 357x4 | 1980x2 | 7220 15954 21570
///  write-1        write-0           reset     idle low
/// ```
///
/// Three populations with a 10x and a 5.5x gap between them, so the
/// thresholds sit in the middle of each gap with better than 3x margin on
/// both sides. The absolute numbers run long against the 1-Wire spec (which
/// wants ~6/60/480 us) because the PROM bit-bangs with instruction-count
/// delays, and our instructions-per-microsecond is a free parameter —
/// [`UST_TICKS_DEN`]. What matters is that the populations stay separated,
/// and they are separated by an order of magnitude.
pub const OW_RESET_US: u64 = 800;
pub const OW_WRITE0_US: u64 = 120;

impl OneWireId {
    /// Build a ROM from a MAC address: the family code, the six address bytes
    /// as the serial number, and a CRC-8 over the first seven.
    pub fn from_mac(mac: [u8; 6]) -> Self {
        let mut rom = [0u8; 8];
        rom[0] = OW_FAMILY_DS2502;
        rom[1..7].copy_from_slice(&mac);
        rom[7] = crc8_dallas(&rom[..7]);
        // The firmware reads six bytes from offset 0 and reverses them to
        // form the address, so store it backwards.
        let mut data = vec![0xff; Self::DATA_LEN];
        for (i, b) in mac.iter().rev().enumerate() {
            data[i] = *b;
        }
        Self {
            rom,
            data,
            status: vec![0xff; Self::STATUS_LEN],
            st: Mutex::new(Default::default()),
        }
    }

    /// 1 kbit of EPROM, in four 32-byte pages.
    pub const DATA_LEN: usize = 128;
    pub const STATUS_LEN: usize = 8;

    pub fn rom(&self) -> [u8; 8] {
        self.rom
    }

    /// Program the EPROM data area.
    pub fn set_data(&mut self, at: usize, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            if at + i < self.data.len() {
                self.data[at + i] = *b;
            }
        }
    }

    /// The bit the device is currently presenting, LSB first within each byte.
    /// Past the end of the reply the line simply floats high, which is what a
    /// real part does.
    fn reply_bit(&self, st: &OneWireState) -> bool {
        let i = st.out_pos;
        match st.out.get(i / 8) {
            Some(b) => (b >> (i % 8)) & 1 != 0,
            None => true,
        }
    }

    /// A whole byte arrived from the master.
    fn take_byte(&self, st: &mut OneWireState, b: u8) {
        match st.phase {
            OwPhase::Idle => {
                st.command = Some(b);
                self.note(st, format!("cmd 0x{b:02x}"));
                match b {
                    onewire_cmd::READ_ROM => {
                        st.out = self.rom.to_vec();
                        st.out_pos = 0;
                        // Exactly 64 bits, and then the master may issue a
                        // memory command without an intervening reset.
                        st.out_then_idle = true;
                        st.phase = OwPhase::Sending;
                    }
                    onewire_cmd::READ_MEMORY
                    | onewire_cmd::READ_DATA_CRC
                    | onewire_cmd::READ_STATUS => {
                        st.pending = b;
                        st.addr_count = 0;
                        st.phase = OwPhase::WantAddr;
                    }
                    // SKIP_ROM and anything else: nothing to say.
                    _ => st.phase = OwPhase::Idle,
                }
            }
            OwPhase::WantAddr => {
                if st.addr_count < 2 {
                    st.addr[st.addr_count] = b;
                    st.addr_count += 1;
                }
                if st.addr_count == 2 {
                    let a = u16::from_le_bytes(st.addr) as usize;
                    // The device answers with a CRC over everything it was
                    // told, then streams memory from that address.
                    // The firmware reads exactly 1 + (len - addr) + 1 bytes:
                    // a CRC over what it just sent, the rest of the memory
                    // from that address, and a CRC over that data. Omitting
                    // the trailing CRC costs nothing visible — the master
                    // simply reads one byte past the end, gets the floating
                    // 0xff, and rejects the whole transfer.
                    let mut out = vec![crc8_dallas(&[st.pending, st.addr[0], st.addr[1]])];
                    let src: &[u8] = if st.pending == onewire_cmd::READ_STATUS {
                        &self.status
                    } else {
                        &self.data
                    };
                    let body: Vec<u8> = src.iter().skip(a).copied().collect();
                    let body_crc = crc8_dallas(&body);
                    out.extend(body);
                    out.push(body_crc);
                    self.note(st, format!("{} from 0x{a:04x}",
                        if st.pending == onewire_cmd::READ_STATUS { "status" } else { "memory" }));
                    st.out = out;
                    st.out_pos = 0;
                    // A memory read streams until the master resets.
                    st.out_then_idle = false;
                    st.phase = OwPhase::Sending;
                }
            }
            // Once the device is talking, the master listens.
            OwPhase::Sending => {}
        }
    }

    fn note(&self, st: &mut OneWireState, what: String) {
        if st.log.len() < 64 {
            st.log.push(what);
        }
    }

    /// The protocol the device saw, decoded — resets, commands, addresses.
    pub fn protocol_log(&self) -> Vec<String> {
        self.st.lock().unwrap().log.clone()
    }

    /// The master changed the line. `low` is true when it is pulling low.
    ///
    /// Everything is decided by how long the master held the line down, which
    /// is why the UST has to be a real clock: a reset, a write-0, a write-1 and
    /// a read slot differ only in duration.
    pub fn master_drive(&self, now: u64, low: bool) {
        let mut st = self.st.lock().unwrap();
        if low {
            if st.low_since.is_none() {
                // Falling edge. It opens the next slot, so the bit the last
                // slot presented is finished with now — advance past it here
                // rather than on release, where the master has not yet had a
                // chance to sample it.
                if st.phase == OwPhase::Sending && st.presenting {
                    st.out_pos += 1;
                    st.presenting = false;
                    // A finite reply ends, and the device goes back to
                    // listening. Without this the master's next command is
                    // read as more read slots and silently lost.
                    if st.out_then_idle && st.out_pos >= st.out.len() * 8 {
                        st.phase = OwPhase::Idle;
                        st.in_bits = 0;
                        st.in_count = 0;
                    }
                }
                st.low_since = Some(now);
            }
            return;
        }
        // Rising edge: classify by how long it was held.
        let Some(since) = st.low_since.take() else { return };
        let held = now.saturating_sub(since);
        if held >= OW_RESET_US {
            // How much of the reply the master actually took before giving up
            // is the difference between "it hated the CRC" and "it hated the
            // contents".
            if st.phase == OwPhase::Sending {
                let (got, total) = (st.out_pos, st.out.len() * 8);
                self.note(&mut st, format!("master took {got} of {total} bits"));
            }
            let log = std::mem::take(&mut st.log);
            *st = OneWireState {
                presence_from: now + OW_PRESENCE_DELAY_US,
                presence_to: now + OW_PRESENCE_DELAY_US + OW_PRESENCE_LEN_US,
                log,
                ..Default::default()
            };
            self.note(&mut st, "reset".into());
            return;
        }
        if st.phase == OwPhase::Sending {
            // A read slot: the master pulsed briefly, and for the rest of the
            // slot the device owns the line.
            st.presenting = true;
            return;
        }
        // A write slot, LSB first.
        let bit = held < OW_WRITE0_US;
        st.in_bits |= (bit as u8) << st.in_count;
        st.in_count += 1;
        if st.in_count == 8 {
            let b = st.in_bits;
            st.in_bits = 0;
            st.in_count = 0;
            self.take_byte(&mut st, b);
        }
    }

    /// What the master sees on the line: true is high (idle), false is low.
    pub fn line_level(&self, now: u64) -> bool {
        let st = self.st.lock().unwrap();
        if now >= st.presence_from && now < st.presence_to {
            return false;
        }
        if st.low_since.is_some() {
            // The master is holding it down itself.
            return false;
        }
        if st.phase == OwPhase::Sending && st.presenting {
            return self.reply_bit(&st);
        }
        true
    }
}

/// Dallas/Maxim CRC-8, polynomial x^8 + x^5 + x^4 + 1 reflected (0x8c).
pub fn crc8_dallas(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        let mut byte = b;
        for _ in 0..8 {
            let mix = (crc ^ byte) & 1;
            crc >>= 1;
            if mix != 0 {
                crc ^= 0x8c;
            }
            byte >>= 1;
        }
    }
    crc
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
/// Driven by executed instructions, not by reads.
///
/// Advancing on read was enough to stop delay loops hanging, but it makes
/// duration meaningless: a loop that reads twice as often "takes" twice as
/// long. Anything that decodes a *pulse width* — 1-Wire above all — needs
/// elapsed time to mean something, so the harness advances this from its step
/// count and the rate below fixes the relationship.
pub struct MaceUst {
    ticks: Mutex<u64>,
}

/// UST ticks per emulated instruction, as a fraction: `TICKS_NUM / TICKS_DEN`.
///
/// The PROM's delay loops are written in microseconds, and its 1-Wire slots
/// are tens of microseconds, so a tick is taken to be 1 µs. At roughly 100
/// emulated instructions per microsecond a bring-up run stays fast while pulse
/// widths keep their proportions.
pub const UST_TICKS_NUM: u64 = 1;
pub const UST_TICKS_DEN: u64 = 100;

impl Default for MaceUst {
    fn default() -> Self { Self::new() }
}

impl MaceUst {
    pub fn new() -> Self { Self { ticks: Mutex::new(0) } }

    /// Advance the clock to match `instructions` executed so far.
    pub fn advance_to(&self, instructions: u64) {
        *self.ticks.lock().unwrap() = instructions * UST_TICKS_NUM / UST_TICKS_DEN;
    }

    fn tick(&self) -> u64 { *self.ticks.lock().unwrap() }
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
    pub gbe: Stub,
    pub macepci: MacePci,
    pub ust: MaceUst,
    pub nic_trace: NicTrace,
    pub rtc: MaceRtc,
    pub onewire: OneWireId,
    pub com0: Com16550,
    pub com1: Com16550,
    pub pci_view: PciNativeView,
    /// Accesses that hit nothing, first 64 kept, as `(addr, is_write, pc)`.
    unmapped: Mutex<Vec<(u32, bool, u32)>>,
    /// The PC of the instruction currently executing.
    pub pc: PcTap,
    /// Count of writes into the firmware load region.
    fw_writes: Mutex<u64>,
    /// 64-bit RAM accesses at the addresses POST's memory sizing uses, as
    /// `(addr, is_write, value, pc)`. The test writes `(!a << 32) | a` to each and
    /// reads it back, so seeing both halves is what tells us whether the store
    /// or the load is the one going wrong.
    sizemem: Mutex<Vec<(u32, bool, u64, u32)>>,
    /// A physical-address watch range, from `IRIS_IP32_WATCHMEM=lo[:hi]`.
    /// Every access within it is logged as `(addr, is_write, value, pc)`.
    /// Cheap to leave in: one range compare on the RAM path.
    watch: Option<(u32, u32)>,
    watch_log: Mutex<Vec<(u32, bool, u64, u32, u64)>>,
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
            gbe: Stub::new("gbe"),
            macepci: MacePci::new(),
            ust: MaceUst::new(),
            nic_trace: NicTrace::new(),
            rtc: MaceRtc::new(),
            // A locally-administered address; nothing depends on the value yet.
            onewire: OneWireId::from_mac([0x08, 0x00, 0x69, 0x12, 0x34, 0x56]),
            com0: Com16550::new(),
            com1: Com16550::new(),
            pci_view: PciNativeView::new(),
            unmapped: Mutex::new(Vec::new()),
            pc: PcTap::default(),
            fw_writes: Mutex::new(0),
            sizemem: Mutex::new(Vec::new()),
            watch: std::env::var("IRIS_IP32_WATCHMEM").ok().and_then(|v| {
                let (a, b) = v.split_once(':').unwrap_or((&v, &v));
                let p = |x: &str| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok();
                Some((p(a)?, p(b)? + 8))
            }),
            watch_log: Mutex::new(Vec::new()),
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

    /// Record an access if it falls in the watch range.
    fn watch_hit(&self, addr: u32, write: bool, val: u64) {
        if let Some((lo, hi)) = self.watch {
            if addr >= lo && addr < hi {
                let mut w = self.watch_log.lock().unwrap();
                if w.len() < 256 {
                    w.push((addr, write, val, self.pc.get(), self.ust.now()));
                }
            }
        }
    }

    /// The watch log, in order.
    pub fn watched(&self) -> Vec<(u32, bool, u64, u32, u64)> {
        self.watch_log.lock().unwrap().clone()
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

    /// Writes landing where the `firmware` section would be loaded.
    pub fn firmware_writes(&self) -> u64 {
        *self.fw_writes.lock().unwrap()
    }

    /// Offset of `addr` within RAM, if it is in RAM.
    pub fn ram_offset(&self, addr: u32) -> Option<u32> {
        self.in_ram(addr).then(|| addr - RAM_BASE)
    }

    fn in_prom(&self, addr: u32) -> bool {
        addr >= PROM_BASE && addr < PROM_BASE + PROM_SIZE
    }

    fn in_gbe(&self, addr: u32) -> bool {
        addr >= GBE_BASE && addr < GBE_BASE + GBE_SIZE
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

    fn in_rtc(&self, addr: u32) -> bool {
        addr >= MACE_RTC && addr < MACE_RTC + MACE_RTC_SIZE
    }

    /// Feed a write of the NIC register through to the 1-Wire device, and
    /// report what the master should read back on the data line.
    fn nic_write(&self, val: u8) {
        // DEASSERT alone decides it: clear means the master is pulling the
        // line down, set means it has let go. The firmware writes DATA=1 in
        // both cases, so DATA is an input here, not a level to drive — which
        // is exactly what the bus trace shows (only 0x08 and 0x0c are ever
        // written).
        let low = val & nic_bit::DEASSERT == 0;
        self.onewire.master_drive(self.ust.now(), low);
    }

    fn nic_read(&self) -> u8 {
        let mut v = self.mace.cell(MACE_ISA_FLASH_NIC_REG) as u8;
        if self.onewire.line_level(self.ust.now()) {
            v |= nic_bit::DATA;
        } else {
            v &= !nic_bit::DATA;
        }
        v
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
            self.watch_hit(addr, false, v);
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
        if (FIRMWARE_LOAD_PA..FIRMWARE_LOAD_PA + FIRMWARE_SPAN).contains(&addr) {
            *self.fw_writes.lock().unwrap() += 1;
        }
        let addr = self.resolve(addr);
        self.watch_hit(addr, true, val);
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
        if self.in_gbe(addr) {
            return BusRead8::ok(self.gbe.read32(addr & !3).data as u8);
        }
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
            let v = self.nic_read();
            self.nic_trace.record(self.ust.now(), false, v);
            return BusRead8::ok(v);
        }
        if let Some(c) = self.com_for(addr) {
            return c.read8(addr);
        }
        if self.in_rtc(addr) {
            return self.rtc.read8(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read8(addr);
        }
        bus_read!(self, addr, 1, BusRead8::ok, |v: u64| v as u8)
    }

    fn read16(&self, addr: u32) -> BusRead16 {
        if self.in_gbe(addr) {
            return BusRead16::ok(self.gbe.read32(addr & !3).data as u16);
        }
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
        if self.in_rtc(addr) {
            return self.rtc.read16(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read16(addr);
        }
        bus_read!(self, addr, 2, BusRead16::ok, |v: u64| v as u16)
    }

    fn read32(&self, addr: u32) -> BusRead32 {
        if self.in_gbe(addr) {
            return self.gbe.read32(addr);
        }
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
            let v = self.nic_read();
            self.nic_trace.record(self.ust.now(), false, v);
            return BusRead32::ok(v as u32);
        }
        if let Some(c) = self.com_for(addr) {
            return c.read32(addr);
        }
        if self.in_rtc(addr) {
            return self.rtc.read32(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read32(addr);
        }
        bus_read!(self, addr, 4, BusRead32::ok, |v: u64| v as u32)
    }

    fn read64(&self, addr: u32) -> BusRead64 {
        if self.in_gbe(addr) {
            return self.gbe.read64(addr);
        }
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
            let v = self.nic_read();
            self.nic_trace.record(self.ust.now(), false, v);
            return BusRead64::ok(v as u64);
        }
        if let Some(c) = self.com_for(addr) {
            return c.read64(addr);
        }
        if self.in_rtc(addr) {
            return self.rtc.read64(addr);
        }
        if self.in_mace(addr) {
            return self.mace.read64(addr);
        }
        let r = bus_read!(self, addr, 8, BusRead64::ok, |v: u64| v);
        self.note_sizemem(addr, false, r.data);
        r
    }

    fn write8(&self, addr: u32, val: u8) -> u32 {
        if self.in_gbe(addr) {
            return self.gbe.write32(addr & !3, val as u32);
        }
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
            self.nic_write(val);
            return self.mace.write8(addr, val);
        }
        if let Some(c) = self.com_for(addr) {
            return c.write8(addr, val);
        }
        if self.in_rtc(addr) {
            return self.rtc.write8(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write8(addr, val);
        }
        if self.write_bytes(addr, 1, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write16(&self, addr: u32, val: u16) -> u32 {
        if self.in_gbe(addr) {
            return self.gbe.write32(addr & !3, val as u32);
        }
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
        if self.in_rtc(addr) {
            return self.rtc.write16(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write16(addr, val);
        }
        if self.write_bytes(addr, 2, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write32(&self, addr: u32, val: u32) -> u32 {
        if self.in_gbe(addr) {
            return self.gbe.write32(addr, val);
        }
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
            self.nic_write(val as u8);
            return self.mace.write32(addr, val);
        }
        if let Some(c) = self.com_for(addr) {
            return c.write32(addr, val);
        }
        if self.in_rtc(addr) {
            return self.rtc.write32(addr, val);
        }
        if self.in_mace(addr) {
            return self.mace.write32(addr, val);
        }
        if self.write_bytes(addr, 4, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write64(&self, addr: u32, val: u64) -> u32 {
        if self.in_gbe(addr) {
            return self.gbe.write64(addr, val);
        }
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
            self.nic_write(val as u8);
            return self.mace.write64(addr, val);
        }
        if let Some(c) = self.com_for(addr) {
            return c.write64(addr, val);
        }
        if self.in_rtc(addr) {
            return self.rtc.write64(addr, val);
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

    /// The CRC is how firmware decides the chip is real. Maxim's published
    /// example ROM is the standard check.
    #[test]
    fn the_dallas_crc_matches_the_published_example() {
        assert_eq!(crc8_dallas(&[0x02, 0x1c, 0xb8, 0x01, 0x00, 0x00, 0x00]), 0xa2);
    }

    #[test]
    fn a_rom_built_from_a_mac_is_self_consistent() {
        let ow = OneWireId::from_mac([0x08, 0x00, 0x69, 0x12, 0x34, 0x56]);
        let rom = ow.rom();
        assert_eq!(rom[0], OW_FAMILY_DS2502, "family code must be the DS2502's");
        assert_eq!(&rom[1..7], &[0x08, 0x00, 0x69, 0x12, 0x34, 0x56]);
        assert_eq!(rom[7], crc8_dallas(&rom[..7]), "CRC must cover the first seven bytes");
    }

    /// A reset long enough to qualify must be answered with a presence pulse —
    /// the line pulled low — or the master concludes nothing is attached.
    /// The pulse is a window in time, not a one-shot cleared by the first
    /// read: the firmware polls it around 150 times, and a pulse that
    /// vanished after the first look would be missed by every later one.
    #[test]
    fn a_reset_is_answered_with_a_presence_pulse_of_real_duration() {
        let ow = OneWireId::from_mac([1, 2, 3, 4, 5, 6]);
        ow.master_drive(0, true);
        let released = OW_RESET_US + 100;
        ow.master_drive(released, false);

        assert!(ow.line_level(released + 1),
                "the device must not answer instantly; the master is still letting go");
        let inside = released + OW_PRESENCE_DELAY_US + OW_PRESENCE_LEN_US / 2;
        assert!(!ow.line_level(inside), "presence pulse must pull the line low");
        // Every sample inside the window sees it, however many there are.
        for k in 0..20 {
            let t = released + OW_PRESENCE_DELAY_US + k * (OW_PRESENCE_LEN_US / 25);
            assert!(!ow.line_level(t), "presence pulse vanished at +{k}");
        }
        let after = released + OW_PRESENCE_DELAY_US + OW_PRESENCE_LEN_US + 1;
        assert!(ow.line_level(after), "and the device must let go afterwards");
    }

    /// A low too short to be a reset is a write slot, and must not be
    /// mistaken for one — otherwise every command byte restarts the device.
    #[test]
    fn a_short_low_is_not_a_reset() {
        let ow = OneWireId::from_mac([1, 2, 3, 4, 5, 6]);
        ow.master_drive(0, true);
        let released = OW_RESET_US - 1;
        ow.master_drive(released, false);
        assert!(ow.line_level(released + OW_PRESENCE_DELAY_US + 1),
                "a sub-reset low must not produce a presence pulse");
    }

    /// READ ROM, clocked in a bit at a time, must stream the ROM back LSB
    /// first — that is what carries the MAC address to the firmware.
    /// A tiny 1-Wire master, so the protocol tests read like the transaction
    /// they are rather than a list of pulse widths.
    struct OwMaster<'a> {
        ow: &'a OneWireId,
        t: u64,
    }

    impl<'a> OwMaster<'a> {
        fn new(ow: &'a OneWireId) -> Self {
            Self { ow, t: 0 }
        }

        fn reset(&mut self) {
            self.ow.master_drive(self.t, true);
            self.t += OW_RESET_US + 100;
            self.ow.master_drive(self.t, false);
            let pulse = self.t + OW_PRESENCE_DELAY_US + OW_PRESENCE_LEN_US / 2;
            assert!(!self.ow.line_level(pulse), "no presence pulse after a reset");
            self.t += OW_PRESENCE_DELAY_US + OW_PRESENCE_LEN_US + 10;
        }

        fn write_bit(&mut self, bit: bool) {
            self.ow.master_drive(self.t, true);
            self.t += if bit { 20 } else { OW_WRITE0_US + 200 };
            self.ow.master_drive(self.t, false);
            self.t += 50;
        }

        fn write_byte(&mut self, b: u8) {
            for i in 0..8 {
                self.write_bit((b >> i) & 1 != 0);
            }
        }

        fn read_bit(&mut self) -> bool {
            self.ow.master_drive(self.t, true);
            self.t += 20;
            self.ow.master_drive(self.t, false);
            self.t += 20;
            let v = self.ow.line_level(self.t);
            self.t += 50;
            v
        }

        fn read_byte(&mut self) -> u8 {
            let mut b = 0u8;
            for i in 0..8 {
                if self.read_bit() {
                    b |= 1 << i;
                }
            }
            b
        }
    }

    /// The whole transaction the firmware performs: identify the part, then
    /// read its memory. Every byte of it is checked, because each one was a
    /// separate gate — the family code, the address CRC, the data, and the
    /// trailing CRC that nothing in the datasheet summary mentions.
    #[test]
    fn the_firmwares_whole_ds2502_transaction_is_answered() {
        let mac = [0x08u8, 0x00, 0x69, 0x12, 0x34, 0x56];
        let ow = OneWireId::from_mac(mac);
        let mut m = OwMaster::new(&ow);

        m.reset();
        m.write_byte(onewire_cmd::READ_ROM);
        let mut rom = [0u8; 8];
        for b in rom.iter_mut() {
            *b = m.read_byte();
        }
        assert_eq!(rom, ow.rom(), "READ ROM must stream the identity back");

        // No reset in between: the firmware goes straight on to the memory
        // read, and a device still stuck in "sending" would swallow it.
        m.write_byte(onewire_cmd::READ_MEMORY);
        m.write_byte(0);
        m.write_byte(0);
        assert_eq!(
            m.read_byte(),
            crc8_dallas(&[onewire_cmd::READ_MEMORY, 0, 0]),
            "the device must answer with a CRC over the command and address"
        );

        let data: Vec<u8> = (0..OneWireId::DATA_LEN).map(|_| m.read_byte()).collect();
        assert_eq!(
            m.read_byte(),
            crc8_dallas(&data),
            "and a CRC over the data it just sent"
        );

        let eaddr: Vec<u8> = data[..6].iter().rev().copied().collect();
        assert_eq!(eaddr, mac, "the firmware reverses the first six bytes to get the MAC");
    }

    #[test]
    fn read_rom_streams_the_identity_back() {
        let mac = [0x08u8, 0x00, 0x69, 0x12, 0x34, 0x56];
        let ow = OneWireId::from_mac(mac);
        let mut t = 0u64;
        // Reset, then wait out the presence pulse.
        ow.master_drive(t, true);
        t += OW_RESET_US + 100;
        ow.master_drive(t, false);
        t += OW_PRESENCE_DELAY_US + OW_PRESENCE_LEN_US + 10;
        // Clock in READ ROM, LSB first: a short low is a 1, a long low a 0.
        for i in 0..8 {
            let bit = (onewire_cmd::READ_ROM >> i) & 1 != 0;
            ow.master_drive(t, true);
            t += if bit { 5 } else { OW_WRITE0_US + 20 };
            ow.master_drive(t, false);
            t += 10;
        }
        // Read 64 bits back. Each slot is the master pulsing the line low and
        // then sampling what the device leaves on it.
        let mut got = [0u8; 8];
        for i in 0..64 {
            ow.master_drive(t, true);
            t += 5;
            ow.master_drive(t, false);
            t += 5;
            if ow.line_level(t) {
                got[i / 8] |= 1 << (i % 8);
            }
            t += 10;
        }
        assert_eq!(got, ow.rom(), "the master must read back exactly the ROM");
        assert_eq!(&got[1..7], &mac, "and the MAC must survive the round trip");
    }

    #[test]
    fn nvram_survives_and_decodes_maces_spacing() {
        let r = MaceRtc::new();
        // Register 0x3f, byte lane +7.
        r.write8(MACE_RTC + (0x3f << 8) + 7, 0xa5);
        assert_eq!(r.read8(MACE_RTC + (0x3f << 8) + 7).data, 0xa5);
        assert_eq!(r.peek(0x3f), 0xa5);
        // The valid-RAM-and-time flag must not read as a dead battery.
        assert_ne!(r.peek(0x0d) & 0x80, 0);
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

    /// The distinct PCs in a ring, oldest first, most recent last. Repeats
    /// inside a loop collapse, so a long spin does not push the interesting
    /// prologue out of view.
    fn ring_path(ring: &[u32], at: usize) -> Vec<u32> {
        let n = ring.len();
        let mut seen = std::collections::BTreeSet::new();
        let mut v: Vec<u32> = (0..n.min(at))
            .map(|i| ring[(at - 1 - i) % n])
            .filter(|p| seen.insert(*p))
            .collect();
        v.reverse();
        v
    }

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
        let mut entered_firmware = false;
        // A short ring of recent PCs, snapshotted the moment the PROM first
        // writes to the console. Whatever branch chose serial-loader mode is
        // in here.
        // A fixed ring, maintained for the whole run: two stores per
        // instruction, and it means any interesting moment can be explained
        // after the fact rather than only the first one we thought to catch.
        const RING: usize = 512;
        let mut ring = [0u32; RING];
        let mut ring_at = 0usize;

        // A call tracer. A call is recognised exactly: an instruction that
        // leaves ra == its own address + 8, which is what jal/jalr/bal do and
        // what an `lw ra, n(sp)` epilogue restore does not. Returns are the
        // matching pc. This is what turns "it ends up in the serial loader"
        // into "this function was called, and returned this".
        let mut call_stack: Vec<(u32, u32, u64)> = Vec::new();
        // (target, last return value, total steps, depth, times in a row)
        let mut calls_done: std::collections::VecDeque<(u32, u64, u64, usize, u64)> =
            std::collections::VecDeque::new();
        let mut arm_call: Option<(u32, u64, u8)> = None;
        let mut call_snapshot: std::collections::HashMap<u32, Vec<String>> =
            std::collections::HashMap::new();
        let mut path_to_console: Vec<u32> = Vec::new();
        let mut fed = false;
        let mut fed_at = 0u64;
        let feed_bytes: Option<Vec<u8>> = std::env::var("IRIS_IP32_INPUT")
            .ok()
            .map(|v| v.replace("\\r", "\r").into_bytes());
        // post1 contains real timed delays — one of them waits a full second
        // of UST, which at UST_TICKS_DEN instructions per tick is 100M steps
        // on its own. The default budget clears that with room to spare;
        // IRIS_IP32_STEPS raises it for longer explorations.
        let limit: u64 = std::env::var("IRIS_IP32_STEPS").ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300_000_000);
        #[allow(non_snake_case)]
        let LIMIT = limit;

        let printf = PrintfTap::new(PrintfTap::POST1_PRINTF);
        let mut stall = StallDetector::new(200_000, 24);
        // A stall inside the exception vectors is an unhandled trap, and the
        // only useful question then is which one. Capture CP0 at the moment it
        // is recognised rather than making someone reproduce it by hand.
        let mut stalls: Vec<(u64, Vec<u32>, u32, u64, u64, [u64; 32])> = Vec::new();
        // PC -> the loop it belongs to, and total steps burned per loop.
        let mut loop_of: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        let mut loop_cost: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
        // loop -> (entries, first step, first GPRs, last step, last GPRs)
        #[allow(clippy::type_complexity)]
        let mut loop_entry: std::collections::HashMap<
            u32, (u64, u64, [u64; 32], u64, [u64; 32], Vec<u32>, Vec<u32>)>
            = std::collections::HashMap::new();
        let mut in_loop = false;
        let prom_for_strings = bus_prom.clone();

        while steps < LIMIT {
            let pc = exec.core.pc as u32;
            if pc == POST1_ENTRY {
                if !reached_post1 {
                    reached_post1 = true;
                    post1_at = steps;
                }
            }
            if (FIRMWARE_LOAD_VA..FIRMWARE_LOAD_VA + FIRMWARE_SPAN).contains(&pc) {
                entered_firmware = true;
            }
            if reached_post1 && (POST1_ENTRY..POST1_ENTRY + 0x8000).contains(&pc) && pc > max_post1_pc {
                max_post1_pc = pc;
            }
            // Publish the PC so devices can attribute the accesses this
            // instruction is about to make.
            bus.pc.set(pc);
            bus.ust.advance_to(steps);

            ring[ring_at % RING] = pc;
            ring_at += 1;

            // Two instructions after the call instruction, pc is the target.
            // The instruction after the call is its delay slot; the one
            // after that is the target. Wait for it, so the trace names the
            // function called rather than the branch delay slot.
            match arm_call {
                Some((ret, at, 0)) => {
                    call_stack.push((pc, ret, at));
                    arm_call = None;
                }
                Some((ret, at, n)) => arm_call = Some((ret, at, n - 1)),
                None => {}
            }
            // A return: pc is the address some frame is waiting to come back
            // to. Unwind through at most a few frames so a tail call or a
            // restore does not desynchronise the whole trace.
            if let Some(d) = call_stack.iter().rev().take(8)
                .position(|f| f.1 == pc)
            {
                for _ in 0..d {
                    call_stack.pop();
                }
                if let Some((target, _, at)) = call_stack.pop() {
                    // Leaf helpers called thousands of times would otherwise
                    // flush everything structural out of the ring. Keep the
                    // shallow calls, which carry the shape of the boot, and
                    // any call that took real time, which is where it went.
                    let took = steps - at;
                    let depth = call_stack.len();
                    if depth <= 5 || took >= 10_000 {
                        // Run-length encode: a helper called ten thousand
                        // times is one line, and the structure above it
                        // survives in the ring.
                        match calls_done.back_mut() {
                            Some(b) if b.0 == target && b.3 == depth => {
                                b.1 = exec.core.gpr[2];
                                b.2 += took;
                                b.4 += 1;
                            }
                            _ => {
                                if calls_done.len() == 64 {
                                    calls_done.pop_front();
                                }
                                calls_done.push_back(
                                    (target, exec.core.gpr[2], took, depth, 1));
                            }
                        }
                    }
                }
            }
            if path_to_console.is_empty() && bus.com0.bytes_out() > 0 {
                path_to_console = ring_path(&ring, ring_at);
            }

            if pc == printf.entry {
                let a = &exec.core.gpr;
                printf.on_call(&prom_for_strings, [a[4], a[5], a[6], a[7]]);
            }

            // Once a spin loop has been recognised, keep counting the time
            // spent in it. A loop that costs half the run is the thing to
            // fix, whether or not it ever exits.
            if let Some(&key) = loop_of.get(&pc) {
                *loop_cost.entry(key).or_insert(0u64) += 1;
                // Count entries, not just time: a delay loop re-entered a
                // thousand times is progress, the same loop entered once is
                // a wedge. Keep the register state of the first and latest
                // entry so the difference between them is visible.
                if !in_loop {
                    let path = ring_path(&ring, ring_at);
                    let e = loop_entry.entry(key).or_insert_with(
                        || (0u64, steps, exec.core.gpr, steps, exec.core.gpr,
                            Vec::new(), Vec::new()));
                    e.0 += 1;
                    e.3 = steps;
                    e.4 = exec.core.gpr;
                    e.6 = path.clone();
                    if e.0 == 1 {
                        e.1 = steps;
                        e.2 = exec.core.gpr;
                        e.5 = path;
                        {
                            let snap = call_snapshot.entry(key).or_default();
                            for (t, v0, took, depth, n) in calls_done.iter() {
                                let times = if *n > 1 { format!(" x{n}") } else { String::new() };
                                snap.push(format!(
                                    "{:indent$}0x{t:08x} -> 0x{v0:x}  ({took} steps){times}",
                                    "", indent = (*depth).min(12) * 2));
                            }
                            for (t, r, at) in call_stack.iter() {
                                snap.push(format!(
                                    "   in flight: 0x{t:08x} (returns to 0x{r:08x}, entered @{at})"));
                            }
                        }
                    }
                }
                in_loop = true;
            } else {
                in_loop = false;
            }
            if let Some(body) = stall.step(pc) {
                for &p in &body {
                    loop_of.entry(p).or_insert(body[0]);
                }
                if stalls.len() < 8 {
                    let cause = exec.core.cp0_cause as u32;
                    let g = exec.core.gpr;
                    stalls.push((steps, body, cause, exec.core.cp0_epc, exec.core.cp0_badvaddr,
                                 g));
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
            if arm_call.is_none() && exec.core.gpr[31] as u32 == pc.wrapping_add(8) {
                arm_call = Some((pc.wrapping_add(8), steps, 1));
            }
        }

        let pc = exec.core.pc as u32;
        eprintln!("ip32: stopped after {steps} steps at PC 0x{pc:08x}{}",
                  if reached_post1 { "  <-- post1 entry" } else { "" });
        eprintln!("ip32: furthest PC seen inside the PROM: 0x{max_prom_pc:08x}");
        {
            let tail = ring_path(&ring, ring_at);
            eprintln!("ip32: last {} distinct PCs before stopping (most recent last):",
                      tail.len());
            for chunk in tail.chunks(8) {
                let line: Vec<String> = chunk.iter().map(|p| format!("{p:08x}")).collect();
                eprintln!("   {}", line.join(" "));
            }
        }

        if reached_post1 {
            eprintln!("ip32: entered post1 after {post1_at} steps; furthest PC in post1: 0x{max_post1_pc:08x}");
        }

        let watched = bus.watched();
        if !watched.is_empty() {
            eprintln!("ip32: watched memory accesses ({}):", watched.len());
            for (a, w, v, p, t) in watched.iter().take(40) {
                eprintln!("   @{t:<9} 0x{a:08x} {} 0x{v:016x}  from PC 0x{p:08x}",
                          if *w { "W" } else { "R" });
            }
        }

        let msgs = printf.lines();
        eprintln!("ip32: ===== POST messages, recovered at the call site ({}) =====", msgs.len());
        for m in msgs.iter().take(40) {
            eprint!("   | {m}");
            if !m.ends_with('\n') { eprintln!(); }
        }
        eprintln!("ip32: ===== end POST messages =====");

        {
            let mut by_cost: Vec<(u32, u64)> = loop_cost.iter().map(|(&k, &v)| (k, v)).collect();
            by_cost.sort_by_key(|&(_, v)| std::cmp::Reverse(v));
            let spun: u64 = by_cost.iter().map(|&(_, v)| v).sum();
            eprintln!("ip32: {spun} of {steps} steps ({}%) spent in {} recognised spin loops:",
                      spun * 100 / steps.max(1), by_cost.len());
            for (pc, cost) in by_cost.iter().take(6) {
                eprintln!("   0x{pc:08x}  {cost} steps ({}%)", cost * 100 / steps.max(1));
                if let Some((n, fs, fg, ls, lg, fp, lp)) = loop_entry.get(pc) {
                    eprintln!("      entered {n} time(s)");
                    let body: Vec<u32> = loop_of.iter()
                        .filter(|(_, &v)| v == *pc).map(|(&k, _)| k).collect();
                    let mut named: Vec<usize> = Vec::new();
                    for &p in &body {
                        let w = bus.read32(p & 0x1fff_ffff).data;
                        for r in [(w >> 21) & 31, (w >> 16) & 31, (w >> 11) & 31] {
                            if r != 0 && !named.contains(&(r as usize)) { named.push(r as usize); }
                        }
                    }
                    named.push(31);
                    named.sort_unstable();
                    named.dedup();
                    if let Some(snap) = call_snapshot.get(pc) {
                        eprintln!("      calls before it (indent = depth, -> = return value):");
                        for l in snap {
                            eprintln!("        {l}");
                        }
                    }
                    for (label, at, g, path) in
                        [("first", fs, fg, fp), ("last", ls, lg, lp)]
                    {
                        let line: Vec<String> = named.iter()
                            .map(|&r| format!("{}=0x{:x}", crate::mips_dis::reg_name(r as u32), g[r]))
                            .collect();
                        eprintln!("      {label} entry @{at}: {}", line.join("  "));
                        eprint!("      {label} path:");
                        for p in path.iter().rev().take(28).collect::<Vec<_>>().iter().rev() {
                            eprint!(" {p:08x}");
                        }
                        eprintln!();
                    }
                }
            }
        }
        // IRIS_IP32_DIS=start:end disassembles a range once the run is over,
        // reading through the bus so RAM-resident code (post1) is visible.
        if let Ok(spec) = std::env::var("IRIS_IP32_DIS") {
            if let Some((a, b)) = spec.split_once(':') {
                let a = u32::from_str_radix(a.trim_start_matches("0x"), 16).unwrap_or(0);
                let b = u32::from_str_radix(b.trim_start_matches("0x"), 16).unwrap_or(0);
                eprintln!("ip32: disassembly 0x{a:08x}..0x{b:08x}");
                let mut p = a;
                while p < b {
                    let w = bus.read32(p & 0x1fff_ffff).data;
                    eprintln!("   0x{p:08x}  {w:08x}  {}",
                              crate::mips_dis::disassemble(w, p as u64, None));
                    p += 4;
                }
            }
        }
        eprintln!("ip32: stalls detected: {}", stalls.len());
        for (at, body, cause, epc, bad, gpr) in stalls.iter() {
            let lo = body.first().copied().unwrap_or(0);
            let hi = body.last().copied().unwrap_or(0);
            let exc = (cause >> 2) & 0x1f;
            eprintln!("   after {at} steps: {} distinct PCs in 0x{lo:08x}..0x{hi:08x}", body.len());
            eprintln!("      CP0 Cause=0x{cause:08x} ExcCode={exc} ({}) EPC=0x{epc:08x} BadVAddr=0x{bad:08x}",
                      exc_name(exc));
            // A short loop body is worth reading. Anything longer is a
            // legitimate busy routine, not something to disassemble here.
            if body.len() <= 24 {
                for &p in body {
                    let w = bus.read32(p & 0x1fff_ffff).data;
                    eprintln!("      0x{p:08x}  {:08x}  {}", w,
                              crate::mips_dis::disassemble(w, p as u64, None));
                }
                // The registers are what say *which* counter a poll is
                // waiting on, and for what value. Only the ones the loop
                // body actually names, so the output stays readable.
                let mut named: Vec<usize> = Vec::new();
                for &p in body {
                    let w = bus.read32(p & 0x1fff_ffff).data;
                    for r in [(w >> 21) & 31, (w >> 16) & 31, (w >> 11) & 31] {
                        if r != 0 && !named.contains(&(r as usize)) { named.push(r as usize); }
                    }
                }
                named.sort_unstable();
                let line: Vec<String> = named.iter()
                    .map(|&r| format!("{}=0x{:016x}", crate::mips_dis::reg_name(r as u32), gpr[r]))
                    .collect();
                eprintln!("      {}", line.join("  "));
            }
        }

        let console = bus.com0.output();
        eprintln!("ip32: ===== PROM console output ({} bytes) =====", console.len());
        for line in console.lines() {
            eprintln!("   | {line}");
        }
        eprintln!("ip32: ===== end console =====");
        eprintln!("ip32: path to the first console byte ({} distinct PCs, most recent last):",
                  path_to_console.len());
        for chunk in path_to_console.chunks(8) {
            eprintln!("   {}", chunk.iter().map(|p| format!("{p:08x}")).collect::<Vec<_>>().join(" "));
        }
        eprintln!("ip32: firmware section: {} write(s) to its load region, PC entered it: {}",
                  bus.firmware_writes(), entered_firmware);
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
        // Writes are what drive the bus; a run of reads is the master
        // sampling and says nothing new. Show every write, and collapse the
        // polling between them into a count.
        eprintln!("ip32: 1-Wire bus, {} events (reads between writes collapsed):", ev.len());
        {
            let mut prev = 0u64;
            let mut reads = 0u32;
            let mut read_lo = 0u32;
            let mut last_level_low = false;
            let mut lows: Vec<u64> = Vec::new();
            for (t, w, v) in ev.iter() {
                if !*w {
                    reads += 1;
                    if v & nic_bit::DATA == 0 { read_lo += 1; }
                    continue;
                }
                if reads > 0 {
                    eprintln!("      ({reads} reads, {read_lo} of them saw the line low)");
                    reads = 0;
                    read_lo = 0;
                }
                let dt = t.saturating_sub(prev);
                prev = *t;
                // What matters is how long the *previous* level lasted: on
                // this bus every distinction — reset, write-0, write-1, read
                // slot — is a pulse width.
                let was_low = last_level_low;
                last_level_low = v & nic_bit::DEASSERT == 0;
                eprintln!("   W 0x{v:02x} -> {:5}   (was {} for {dt}us)",
                          if last_level_low { "LOW" } else { "high" },
                          if was_low { "LOW " } else { "high" });
                if was_low {
                    lows.push(dt);
                }
            }
            if reads > 0 {
                eprintln!("      ({reads} reads, {read_lo} of them saw the line low)");
            }
            // The populations are the whole argument for where the thresholds
            // go. If they are not well separated, the thresholds are guesses.
            lows.sort_unstable();
            let mut runs: Vec<(u64, u32)> = Vec::new();
            for d in &lows {
                match runs.last_mut() {
                    Some(r) if r.0 == *d => r.1 += 1,
                    _ => runs.push((*d, 1)),
                }
            }
            let plog = bus.onewire.protocol_log();
            eprintln!("   decoded protocol ({} events): {}", plog.len(), plog.join(" | "));
            eprintln!("   master low-pulse widths (us x count): {}",
                      runs.iter().map(|(d, n)| format!("{d}x{n}"))
                          .collect::<Vec<_>>().join(" "));
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

        // Checks last. Everything above is the evidence for whatever fails
        // here, and a panic partway through would throw it away.
        assert!(
            reached_post1,
            "sloader did not reach post1 at 0x{POST1_ENTRY:08x}; it stopped at \
             0x{:08x} after {steps} steps. The trace above says what it wanted.",
            exec.core.pc as u32,
        );
        assert!(un.is_empty(), "unmapped accesses: {un:x?}");
    }
}

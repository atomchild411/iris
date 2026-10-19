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
/// VICE, the video and image compression engine.
pub const VICE_BASE: u32 = 0x1700_0000;
pub const VICE_SIZE: u32 = 0x0100_0000;

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
/// The Ethernet, MAC-110 (`crate::dev::mec`), and its CRIME interrupt.
pub const MACE_MEC: u32 = MACE_BASE + 0x0028_0000;
pub const MACE_MEC_SIZE: u32 = 0x0000_0200;
pub const IP32_MEC_CRIME_INT: u32 = 3;
pub const MACE_COM0: u32 = MACE_BASE + 0x0039_0000;
pub const MACE_COM1: u32 = MACE_BASE + 0x0039_8000;
pub const MACE_COM_SIZE: u32 = 0x0000_0800;

/// MACE's free-running timer (`MACE_UST_MSC`, i.e. `MACE_PERIF + 0x40000`).
/// The PROM busy-waits on this; a constant hangs it forever.
/// The two PS/2 ports, keyboard at +0x00 and mouse at +0x20, each
/// `tx / rx / control / status` at eight-byte spacing.
pub const MACE_PS2: u32 = MACE_BASE + 0x0032_0000;
pub const MACE_PS2_SIZE: u32 = 0x40;

pub mod ps2_bit {
    pub const CLOCK_SIGNAL: u64 = 1 << 0;
    pub const CLOCK_INHIBIT: u64 = 1 << 1;
    pub const TX_INPROGRESS: u64 = 1 << 2;
    /// The transmit holding register has drained. The PROM spins on this for
    /// up to 20834 tries after every byte it sends, so a port that never sets
    /// it is not merely idle — it is expensive.
    pub const TX_EMPTY: u64 = 1 << 3;
    /// A byte has arrived. Never set here: nothing is plugged in.
    pub const RX_FULL: u64 = 1 << 4;
    pub const RX_INPROGRESS: u64 = 1 << 5;
}

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
/// The alias is the first 256 MB of the window ([`LOW_ALIAS_SIZE`]); memory
/// above that is only at its own address. The lowest device sits at
/// 0x14000000, so the alias cannot shadow one.

/// The memory window CRIME decodes: 1 GB from [`RAM_BASE`], which its eight
/// bank controls place banks of 32 or 128 MB in. How, and what an absent bank
/// answers, is in [`crate::dev::crime_mem`].
use crate::dev::crime_mem::{Decode, O2Memory};

pub const RAM_WINDOW_SIZE: u32 = 8 * 128 * 1024 * 1024;
/// How much of memory also answers from address 0: the first 256 MB.
pub const LOW_ALIAS_SIZE: u32 = 0x1000_0000;

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
/// How often the harness looks at the CP0 timer. Every instruction would be
/// honest and far too slow; this is fine enough that a kernel's clock ticks
/// smoothly and coarse enough to cost nothing.
pub const TIMER_POLL_STEPS: u64 = 256;

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
    /// Pending interrupts, and which of them are allowed through. NetBSD
    /// attaches CRIME as `platform.intr0`, so anything getting past the mask
    /// arrives at the CPU as IP2.
    pub const INTSTAT: u32 = 0x0010;
    pub const INTMASK: u32 = 0x0018;
    /// Software interrupts: bits 31:28 and 24:0 can be set from here.
    pub const SOFTINT: u32 = 0x0020;
    /// The hardware sources; writing 0 to a latched bit clears it.
    pub const HARDINT: u32 = 0x0028;
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
/// CRIME interrupt bits that latch (CRIME 1.5 spec): 30, 24:22, 19:16.
pub const CRIME_LATCHED_BITS: u64 = 0x41cf_0000;

/// CRIME 1.1 as the PROM wants to see it before it offers a graphics console.
pub const CRIME_REV_GBE: u64 = 0x0000_0000_0000_00a1;

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
    /// Edge-triggered sources CRIME holds until software writes a 0 to the
    /// bit in HARDINT: GBE's four timing lines (16..19), the rendering
    /// engine's empty/full/idle edges (22..24) and bit 30.
    latched: Mutex<u64>,
    revision: u64,
    /// The clock CRIME's free-running counter is derived from, so the PROM's
    /// delays take the time they ask for rather than a number of reads.
    ust: std::sync::Arc<MaceUst>,
    /// Subtracted from the counter, so a write sets what the next read sees.
    time_base: Mutex<u64>,
}

/// CRIME's counter runs at 66.67 MHz. The PROM's own delay routine computes
/// `66 * microseconds` before waiting on it, which is where this number comes
/// from — it is the firmware's arithmetic, not a datasheet's.
pub const CRIME_TICKS_PER_US: u64 = 66;

impl Crime {
    pub fn new(ust: std::sync::Arc<MaceUst>) -> Self {
        Self {
            regs: Mutex::new(vec![0u64; (CRIME_SIZE / 8) as usize]),
            trace: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            latched: Mutex::new(0),
            revision: CRIME_REV_DEFAULT,
            ust,
            time_base: Mutex::new(0),
        }
    }

    pub fn with_revision(ust: std::sync::Arc<MaceUst>, revision: u64) -> Self {
        Self { revision, ..Self::new(ust) }
    }

    /// Raise or lower one of CRIME's interrupt inputs.
    pub fn set_int(&self, bit: u32, on: bool) {
        let mut r = self.regs.lock().unwrap();
        let i = (crime_reg::INTSTAT / 8) as usize;
        if on {
            r[i] |= 1u64 << bit;
        } else {
            r[i] &= !(1u64 << bit);
        }
    }

    /// Latch an edge-triggered source until software clears it in HARDINT.
    pub fn latch_int(&self, bit: u32) {
        *self.latched.lock().unwrap() |= (1u64 << bit) & CRIME_LATCHED_BITS;
    }

    /// INTSTAT: the level sources, the latched ones and the software ones.
    fn intstat(&self) -> u64 {
        let r = self.regs.lock().unwrap();
        r[(crime_reg::INTSTAT / 8) as usize] | r[(crime_reg::SOFTINT / 8) as usize] | *self.latched.lock().unwrap()
    }

    /// True when something is pending that the mask lets through.
    pub fn interrupting(&self) -> bool {
        let mask = self.regs.lock().unwrap()[(crime_reg::INTMASK / 8) as usize];
        self.intstat() & mask != 0
    }

    /// The raw counter, before the offset a write installs.
    fn raw_time(&self) -> u64 {
        self.ust.now().wrapping_mul(CRIME_TICKS_PER_US)
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
            crime_reg::INTSTAT => self.intstat(),
            crime_reg::HARDINT => self.regs.lock().unwrap()[(crime_reg::INTSTAT / 8) as usize] | *self.latched.lock().unwrap(),
            // A free-running microsecond-ish counter. POST loops on this to time
            // out, so a constant would hang it forever.
            crime_reg::TIME => {
                let base = *self.time_base.lock().unwrap();
                self.raw_time().wrapping_sub(base) & 0x0000_ffff_ffff_ffff
            }
            _ => self.regs.lock().unwrap()[(off / 8) as usize],
        }
    }

    fn store(&self, off: u32, val: u64) {
        match off {
            // Read-only: the sources set it, not the CPU.
            crime_reg::INTSTAT => return,
            crime_reg::HARDINT => {
                let mut l = self.latched.lock().unwrap();
                *l &= val | !CRIME_LATCHED_BITS;
                return;
            }
            crime_reg::SOFTINT => {
                self.regs.lock().unwrap()[(off / 8) as usize] = val & 0xf1ff_ffff;
                return;
            }
            _ => {}
        }
        if off == crime_reg::TIME {
            // The PROM zeroes the counter and then waits for it to reach a
            // deadline, so a write has to set where it counts from.
            *self.time_base.lock().unwrap() = self.raw_time().wrapping_sub(val);
        }
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
/// One PCI function's configuration space.
///
/// Enough of it to be discovered and configured: identity, class, command and
/// status, and base address registers that answer a size probe. Writing all
/// ones to a BAR and reading it back is how firmware learns how much space a
/// device wants, so a BAR that stores whatever it is given reports a size of
/// four gigabytes and the bus layout falls apart.
pub struct PciFunction {
    pub name: &'static str,
    config: Mutex<[u32; 64]>,
    /// Size of each BAR in bytes; zero means the BAR is not implemented.
    bar_size: [u32; 6],
    /// Every config register touched, in first-touch order, with direction.
    touched: Mutex<Vec<(u8, bool)>>,
    /// The device's own registers, behind its memory BAR, and a log of what
    /// the guest did to them. At this stage the log is the point: it says
    /// which parts of the chip the firmware insists on.
    mem: Mutex<std::collections::BTreeMap<u32, u32>>,
    mem_log: Mutex<Vec<(u32, bool, u32, u32, u32)>>,
    /// The device's behaviour, if it has any beyond storing what it is told.
    ops: Option<std::sync::Arc<dyn PciDeviceOps>>,
}

/// What a PCI device does when the guest touches its memory BAR.
pub trait PciDeviceOps: Send + Sync {
    fn read(&self, off: u32, width: usize) -> u32;
    fn write(&self, off: u32, val: u32, width: usize);
}

impl PciFunction {
    /// `class` is the 24-bit class/subclass/interface, as it sits in the top
    /// three bytes of register 0x08.
    pub fn new(
        name: &'static str,
        vendor: u16,
        device: u16,
        class: u32,
        revision: u8,
        bar_size: [u32; 6],
    ) -> Self {
        let mut c = [0u32; 64];
        c[0] = ((device as u32) << 16) | vendor as u32;
        c[2] = (class << 8) | revision as u32;
        // Single-function, ordinary device header.
        c[3] = 0;
        // Interrupt pin A; the line is left for firmware to assign.
        c[15] = 0x0000_0100;
        Self {
            name,
            config: Mutex::new(c),
            bar_size,
            touched: Mutex::new(Vec::new()),
            mem: Mutex::new(std::collections::BTreeMap::new()),
            mem_log: Mutex::new(Vec::new()),
            ops: None,
        }
    }

    fn note(&self, reg: u8, write: bool) {
        let mut t = self.touched.lock().unwrap();
        if !t.iter().any(|&(r, w)| r == reg && w == write) {
            t.push((reg, write));
        }
    }

    /// Config registers touched so far: `(byte offset, was_write)`.
    pub fn touched(&self) -> Vec<(u8, bool)> {
        self.touched.lock().unwrap().clone()
    }

    /// The base address firmware programmed into a BAR, with the type bits
    /// masked off.
    pub fn bar(&self, i: usize) -> u32 {
        let v = self.config.lock().unwrap()[4 + i];
        if v & 1 != 0 { v & !3 } else { v & !15 }
    }

    /// Accesses to the device's memory BAR: `(offset, was_write, value, pc)`.
    pub fn mem_log(&self) -> Vec<(u32, bool, u32, u32, u32)> {
        self.mem_log.lock().unwrap().clone()
    }

    fn log_mem(&self, off: u32, write: bool, val: u32, pc: (u32, u32)) {
        let mut l = self.mem_log.lock().unwrap();
        if l.len() < 20000 {
            l.push((off, write, val, pc.0, pc.1));
        }
    }

    /// Give the function real behaviour instead of a register file.
    pub fn with_ops(mut self, ops: std::sync::Arc<dyn PciDeviceOps>) -> Self {
        self.ops = Some(ops);
        self
    }

    pub fn mem_read(&self, off: u32, width: usize, pc: (u32, u32)) -> u32 {
        if let Some(ops) = &self.ops {
            let v = ops.read(off, width);
            self.log_mem(off, false, v, pc);
            return v;
        }
        let m = self.mem.lock().unwrap();
        let mut v = 0u32;
        for i in 0..width as u32 {
            v = (v << 8) | m.get(&(off + i)).copied().unwrap_or(0);
        }
        drop(m);
        self.log_mem(off, false, v, pc);
        v
    }

    pub fn mem_write(&self, off: u32, val: u32, width: usize, pc: (u32, u32)) {
        self.log_mem(off, true, val, pc);
        if let Some(ops) = &self.ops {
            ops.write(off, val, width);
            return;
        }
        let mut m = self.mem.lock().unwrap();
        for i in 0..width as u32 {
            m.insert(off + i, (val >> (8 * (width as u32 - 1 - i))) as u8 as u32);
        }
    }

    /// True when the device has been told it may decode memory accesses.
    pub fn memory_enabled(&self) -> bool {
        self.config.lock().unwrap()[1] & 0x2 != 0
    }

    pub fn read(&self, reg: usize) -> u32 {
        self.note((reg * 4) as u8, false);
        match reg {
            // Status: report that we are a fast device with no errors.
            1 => (self.config.lock().unwrap()[1] & 0xffff) | 0x0200_0000,
            _ => self.config.lock().unwrap()[reg],
        }
    }

    pub fn write(&self, reg: usize, val: u32) {
        self.note((reg * 4) as u8, true);
        let mut c = self.config.lock().unwrap();
        match reg {
            // Identity, class and header type are read-only.
            0 | 2 | 3 => {}
            // Command register; status bits are write-one-to-clear and we
            // never set any.
            1 => c[1] = val & 0xffff,
            4..=9 => {
                let i = reg - 4;
                let size = self.bar_size[i];
                if size == 0 {
                    c[reg] = 0;
                } else {
                    // Bit 0 selects I/O space. Keep the type bits we
                    // advertise and let the address bits above the size be
                    // written; everything below reads back as zero, which is
                    // what tells firmware the size.
                    let io = i == 0;
                    let mask = !(size - 1);
                    let type_bits = if io { 1 } else { 0 };
                    c[reg] = (val & mask) | type_bits;
                }
            }
            _ => c[reg] = val,
        }
    }
}

pub struct MacePci {
    config_addr: Mutex<u32>,
    regs: Mutex<[u32; (MACEPCI_SIZE / 4) as usize]>,
    /// Distinct config addresses the guest selected, in order.
    probed: Mutex<Vec<u32>>,
    /// Devices by slot number on bus 0.
    slots: Mutex<std::collections::BTreeMap<u8, std::sync::Arc<PciFunction>>>,
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

/// The SCSI controller an O2 has on its PCI bus: an Adaptec AIC-7880.
pub const PCI_VENDOR_ADAPTEC: u16 = 0x9004;
pub const PCI_DEVICE_AIC7880: u16 = 0x8078;
/// Mass storage controller, SCSI, no specific programming interface.
pub const PCI_CLASS_SCSI: u32 = 0x0100_00;
/// The two SCSI controllers an O2 has on its motherboard. CRIME carries a
/// separate interrupt line for each -- `CRIME_INT_PCI_SCSI0` and
/// `CRIME_INT_PCI_SCSI1` -- which is the clearest evidence that both are
/// expected to be there.
pub const IP32_SCSI_SLOT: u8 = 1;
pub const IP32_SCSI_SLOT_B: u8 = 2;
/// MACE's own interrupt status, which CRIME input 4 cascades from. NetBSD's
/// handler reads this register and matches each device's mask against it, so
/// raising CRIME alone is not enough -- the demultiplexer has to find
/// something.
pub const MACE_ISA_INT_STATUS: u32 = MACE_BASE + 0x0031_0010;
/// CRIME input 4 is the ISA cascade; `com0` sits behind it.
pub const IP32_ISA_CRIME_INT: u32 = 4;
/// CRIME input for MACE ISA status bits 8..15: RTC, the PS/2 ports, UST timers.
pub const IP32_MISC_CRIME_INT: u32 = 5;
/// MACE ISA interrupt mask, next to [`MACE_ISA_INT_STATUS`].
pub const MACE_ISA_INT_MASK: u32 = MACE_BASE + 0x0031_0018;
/// The PS/2 bits of the ISA status: 9 and 11 the ports' requests, 10 and 12
/// their poll flags.
pub const MACE_ISA_PS2_MASK: u64 = 0x1e00;
/// ISA ring base and reset register; bit 4 reads as the MACE revision.
pub const MACE_ISA_RING_BASE: u32 = MACE_BASE + 0x0031_0000;
pub const MACE_REV2_BIT: u64 = 0x10;
/// The bits NetBSD's `com0` attaches with, as it prints them:
/// "com0 at mace0 offset 0x390000 intr 4 intrmask 0x3f00000".
pub const MACE_ISA_SERIAL0_MASK: u64 = 0x03f0_0000;
/// The serial DMA channels' registers: port 0 at the start, port 1 16 KB on.
pub const MACE_SDMA: u32 = MACE_BASE + 0x0031_8000;
pub const MACE_SDMA_SIZE: u32 = 0x0000_8000;

/// The serial DMA control registers, rings and status bits, as MACE's
/// datasheet lays them out.
pub mod sdma {
    /// Control bit 10: the channel is held in reset, pointers at zero.
    pub const CTRL_RESET: u64 = 0x400;
    /// Control bit 9: the engine runs. Clear, it is frozen with its state.
    pub const CTRL_ENABLE: u64 = 0x200;
    /// Control bits 7:5: which depth condition raises the threshold bit.
    pub const CTRL_COND: u64 = 0xe0;
    /// The bits a control register keeps; the rest read as zero.
    pub const CTRL_MASK: u64 = 0x6e0;
    /// Pointers and depths count 32-byte blocks in bits 11:5.
    pub const PTR_MASK: u32 = 0xfe0;
    /// One block short of the ring: the engine never fills the last slot.
    pub const RING_FULL: u32 = 0xfe0;
    pub const BLOCK: u32 = 32;
    /// Where the eight 4 KB rings start: bits 31:15 of the ring base register.
    pub const RING_BASE_MASK: u64 = 0xffff_8000;
    /// Transmit pair commands, in the top two bits of a control byte.
    pub const TX_CMD_THR: u8 = 1;
    pub const TX_CMD_MCR: u8 = 2;
    /// Transmit control bit 5: tell software once this pair is done.
    pub const TX_PREQ: u8 = 0x20;
    /// Receive control bit 7: this slot holds a real character.
    pub const RX_VALID: u8 = 0x80;

    /// Port `p`'s six ISA status bits, from bit 20 for port 0.
    pub const fn group(p: usize) -> u64 { 0x3f << (20 + 6 * p) }
    /// The 16550's own interrupt request.
    pub const fn dir(p: usize) -> u64 { 1 << (20 + 6 * p) }
    pub const fn tx_thir(p: usize) -> u64 { 1 << (21 + 6 * p) }
    pub const fn tx_preq(p: usize) -> u64 { 1 << (22 + 6 * p) }
    pub const fn tx_memerr(p: usize) -> u64 { 1 << (23 + 6 * p) }
    pub const fn rx_thir(p: usize) -> u64 { 1 << (24 + 6 * p) }
    pub const fn rx_overrun(p: usize) -> u64 { 1 << (25 + 6 * p) }
}

/// Which of CRIME's interrupt inputs the controller in that slot drives.
/// NetBSD works this out for itself and prints it: "interrupting at crime
/// interrupt 8".
pub const IP32_SCSI_CRIME_INT: u32 = 8;
pub const IP32_SCSI_CRIME_INT_B: u32 = 9;

/// The CPU-side window onto PCI memory, and the PCI address it starts at.
///
/// These are not the same number, which is the whole point. The PROM programs
/// a BAR with a *PCI* address — 0x80001000 — and then reaches it through a
/// window low in the CPU's physical map. The bus-error register dump named
/// `0xba001084`, physical 0x1a001084, which is this window plus 0x1084, which
/// is that BAR plus 0x84. Mapping the window one-to-one instead would put the
/// device nowhere the firmware ever looks.
pub const PCI_MEM_BASE: u32 = 0x1a00_0000;
pub const PCI_MEM_SIZE: u32 = 0x0200_0000;
pub const PCI_MEM_PCI_BASE: u32 = 0x8000_0000;

/// PCI memory's second, one-to-one window, from physical 0x2_8000_0000. A
/// 64-bit kernel (OpenBSD's) reaches devices there; the PROM and NetBSD use
/// the low window above. Physical addresses reach this bus as 32 bits, so it
/// arrives as 0x8000_0000 and up, where an O2 has nothing else below 4 GB.
pub const PCI_HI_MEM_BASE: u32 = 0x8000_0000;

impl Default for MacePci {
    fn default() -> Self { Self::new() }
}

impl MacePci {
    pub fn new() -> Self {
        Self {
            config_addr: Mutex::new(0),
            regs: Mutex::new([0u32; (MACEPCI_SIZE / 4) as usize]),
            probed: Mutex::new(Vec::new()),
            slots: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Put a device in a slot on bus 0.
    pub fn attach(&self, slot: u8, f: std::sync::Arc<PciFunction>) {
        self.slots.lock().unwrap().insert(slot, f);
    }

    pub fn slot(&self, slot: u8) -> Option<std::sync::Arc<PciFunction>> {
        self.slots.lock().unwrap().get(&slot).cloned()
    }

    /// The device decoding a PCI memory address, and the offset within its
    /// BAR, if any device claims it.
    pub fn mem_target(&self, pci_addr: u32) -> Option<(std::sync::Arc<PciFunction>, u32)> {
        for f in self.slots.lock().unwrap().values() {
            if !f.memory_enabled() {
                continue;
            }
            for i in 0..6 {
                let size = f.bar_size[i];
                let base = f.bar(i);
                if size != 0 && base != 0 && pci_addr >= base && pci_addr < base + size {
                    return Some((f.clone(), pci_addr - base));
                }
            }
        }
        None
    }

    /// Split a CONFIG_ADDR value into the function it selects and the
    /// register within it. Only bus 0 exists here.
    fn selected(&self) -> Option<(std::sync::Arc<PciFunction>, usize)> {
        // Bit 31 is the enable bit of the standard PCI mechanism, and MACE's
        // bridge does not insist on it: the PROM sets it (0x80000800) and
        // NetBSD's driver does not (0x00000800). Requiring it makes the whole
        // bus invisible to one of them, which is exactly how the kernel came
        // to probe and find nothing.
        let a = *self.config_addr.lock().unwrap() & 0x7fff_ffff;
        if (a >> 16) & 0xff != 0 {
            return None;
        }
        let dev = ((a >> 11) & 0x1f) as u8;
        let func = (a >> 8) & 7;
        if func != 0 {
            return None;
        }
        let f = self.slots.lock().unwrap().get(&dev).cloned()?;
        Some((f, ((a >> 2) & 0x3f) as usize))
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
            macepci_reg::CONFIG_DATA => BusRead32::ok(match self.selected() {
                Some((f, reg)) => f.read(reg),
                // An absent device: the bus floats high.
                None => 0xffff_ffff,
            }),
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
            macepci_reg::CONFIG_DATA => {
                if let Some((f, reg)) = self.selected() {
                    f.write(reg, val);
                }
            }
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
    /// A live copy of everything transmitted, flushed as it arrives.
    /// A guest that takes half an hour to boot cannot be watched through a
    /// dump printed at the end of the run.
    log: Mutex<Option<std::fs::File>>,
    /// Characters waiting to be read by the guest. The PROM polls LSR for
    /// data-ready and then reads the receive register, so an empty queue simply
    /// means "nobody has typed anything".
    input: std::sync::Arc<Mutex<std::collections::VecDeque<u8>>>,
    regs: Mutex<[u8; 8]>,
    /// The client of a TCP console (`serve_tcp`), which gets every byte
    /// transmitted and whose keystrokes are queued as input.
    /// Every connected console client: each sees all output and may type.
    tcp: std::sync::Arc<Mutex<Vec<std::net::TcpStream>>>,
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

    /// IER: interrupt on received data, and on the transmitter draining.
    pub const IER_RX: u8 = 0x01;
    pub const IER_TX: u8 = 0x02;
    /// IIR: bit 0 clear means "something to service"; the cause is above it.
    pub const IIR_NONE: u8 = 0x01;
    pub const IIR_TX_EMPTY: u8 = 0x02;
    pub const IIR_RX_DATA: u8 = 0x04;
    /// The two high bits a 16550 sets once its FIFOs are enabled.
    pub const IIR_FIFO: u8 = 0xc0;

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
            log: Mutex::new(None),
            input: Default::default(),
            regs: Mutex::new([0u8; 8]),
            tcp: Default::default(),
        }
    }

    /// A live console on `127.0.0.1:port`, as IRIS's other machines have on
    /// their serial ports: one client at a time (a new one replaces it), in
    /// telnet character mode -- the same handshake and filter the Z85C30's
    /// TCP backend uses, so `telnet` and `nc` both work. Everything the guest
    /// transmits from now on goes to the client too; what the client types is
    /// queued for the guest, as `feed` queues a script's input.
    /// Port 0 takes any free port; the one bound is returned.
    pub fn serve_tcp(&self, port: u16) -> std::io::Result<u16> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
        let bound = listener.local_addr()?.port();
        let input = self.input.clone();
        let client = self.tcp.clone();
        std::thread::Builder::new().name(format!("ip32-com0-tcp-{port}")).spawn(move || {
            // One thread per client, so a person and a script can share the
            // console: both see everything, and whatever either types goes in.
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_nodelay(true);
                if stream.write_all(&crate::net::telnet::TelnetFilter::initial_handshake()).is_err() {
                    continue;
                }
                let Ok(writer) = stream.try_clone() else { continue };
                client.lock().unwrap().push(writer);
                let input = input.clone();
                let _ = std::thread::Builder::new().name(format!("ip32-com0-client-{port}")).spawn(move || {
                    let mut telnet = crate::net::telnet::TelnetFilter::new();
                    let mut buf = [0u8; 256];
                    loop {
                        let n = match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        let mut reply = Vec::new();
                        let mut typed = Vec::new();
                        for &b in &buf[..n] {
                            if let Some(d) = telnet.feed(b, &mut reply) {
                                typed.push(d);
                            }
                        }
                        if !reply.is_empty() && stream.write_all(&reply).is_err() {
                            break;
                        }
                        input.lock().unwrap().extend(typed);
                    }
                    // A client that has gone is dropped on the next write
                    // that fails.
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                });
            }
        })?;
        Ok(bound)
    }

    /// Mirror everything transmitted to a file as it happens.
    pub fn log_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        *self.log.lock().unwrap() = Some(std::fs::File::create(path)?);
        Ok(())
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

    /// The last `n` bytes written, without copying the whole history.
    pub fn output_tail(&self, n: usize) -> String {
        let o = self.out.lock().unwrap();
        String::from_utf8_lossy(&o[o.len().saturating_sub(n)..]).to_string()
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

    /// What the chip wants serviced, in the form the driver reads it.
    ///
    /// A kernel does not poll: it fills the transmit FIFO and waits to be
    /// told it has drained. Reporting "nothing pending" here leaves it
    /// waiting forever, and the visible symptom is output that stops in the
    /// middle of a word.
    fn iir(&self) -> u8 {
        let ier = self.regs.lock().unwrap()[com_reg::IER as usize];
        if ier & com_reg::IER_RX != 0 && !self.input.lock().unwrap().is_empty() {
            return com_reg::IIR_RX_DATA;
        }
        if ier & com_reg::IER_TX != 0 {
            // The transmitter is always drained here; there is no wire.
            return com_reg::IIR_TX_EMPTY;
        }
        com_reg::IIR_NONE
    }

    /// True while the chip is asking for attention.
    pub fn interrupting(&self) -> bool {
        self.iir() & com_reg::IIR_NONE == 0
    }

    pub fn read_reg(&self, addr: u32) -> u8 {
        let Some(n) = Self::reg_of(addr) else { return 0 };
        match n {
            // Reading IIR is how the driver asks what to do next, and on a
            // 16550 it also acknowledges a transmit interrupt.
            com_reg::IIR_FCR => {
                let fifo = if self.regs.lock().unwrap()[com_reg::IIR_FCR as usize] & 1 != 0 {
                    com_reg::IIR_FIFO
                } else {
                    0
                };
                self.iir() | fifo
            }
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
            self.transmit(val);
            return;
        }
        self.regs.lock().unwrap()[n as usize] = val;
    }

    /// Send one character, as a write of the transmit register does. MACE's
    /// serial DMA engine comes here too, so a byte sent from the ring reaches
    /// the log and the TCP console by exactly the same road as one the
    /// firmware writes by hand.
    pub fn transmit(&self, val: u8) {
        // Loopback: the byte never leaves the chip, it arrives back in
        // the receive register. post1 writes 0..254 this way and insists
        // on reading the same sequence back.
        if self.looped() {
            self.input.lock().unwrap().push_back(val);
            return;
        }
        self.out.lock().unwrap().push(val);
        if let Some(f) = self.log.lock().unwrap().as_mut() {
            use std::io::Write;
            let _ = f.write_all(&[val]);
            let _ = f.flush();
        }
        let mut tcp = self.tcp.lock().unwrap();
        if !tcp.is_empty() {
            use std::io::Write;
            let mut buf = Vec::with_capacity(2);
            crate::net::telnet::escape_byte(val, &mut buf);
            tcp.retain_mut(|t| t.write_all(&buf).is_ok());
        }
    }

    /// Set the modem control register, for a DMA pair that names it rather
    /// than the transmitter.
    pub fn write_mcr(&self, val: u8) {
        self.regs.lock().unwrap()[com_reg::MCR as usize] = val;
    }

    /// Hand up to `buf.len()` received characters to the DMA engine, oldest
    /// first, and say how many. They leave the queue, so the PIO path never
    /// sees them as well.
    pub fn take_input(&self, buf: &mut [u8]) -> usize {
        let mut q = self.input.lock().unwrap();
        let n = buf.len().min(q.len());
        for (b, c) in buf.iter_mut().zip(q.drain(..n)) {
            *b = c;
        }
        n
    }

    /// True when nothing has been received and not yet taken.
    fn input_empty(&self) -> bool {
        self.input.lock().unwrap().is_empty()
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

/// One direction of a serial port's DMA: its control register and the two
/// pointers into its ring, as byte offsets with only bits 11:5 kept.
#[derive(Clone, Copy)]
struct SdmaChan {
    ctrl: u64,
    rptr: u32,
    wptr: u32,
    /// Blocks and characters moved, so a quiet console can be told apart
    /// from an engine that never ran.
    blocks: u64,
    bytes: u64,
}

impl SdmaChan {
    /// Out of reset a channel is held in reset, which is what software that
    /// reads the control register before ever writing it gets back.
    const RESET: Self = Self { ctrl: sdma::CTRL_RESET, rptr: 0, wptr: 0, blocks: 0, bytes: 0 };

    /// Bytes of ring in use, always a whole number of blocks.
    fn depth(&self) -> u32 {
        self.wptr.wrapping_sub(self.rptr) & sdma::PTR_MASK
    }

    fn in_reset(&self) -> bool {
        self.ctrl & sdma::CTRL_RESET != 0
    }

    fn running(&self) -> bool {
        self.ctrl & (sdma::CTRL_RESET | sdma::CTRL_ENABLE) == sdma::CTRL_ENABLE
    }

    /// The threshold bit, as a level: true exactly while the depth meets the
    /// condition in the control register. A pure level is what lets the
    /// driver's own control writes take it down again; anything latched here
    /// would be an interrupt nobody can clear. It does not wait for ENABLE:
    /// a disabled channel is frozen, not switched off.
    fn threshold(&self, output: bool) -> bool {
        if self.in_reset() {
            return false;
        }
        let n = self.depth() >> 5;
        // The fill-level codes read the same comparator both ways round: an
        // input channel wants attention once it is that full, an output
        // channel once it has drained below it.
        let level = |at: u32| if output { n < at } else { n >= at };
        match (self.ctrl & sdma::CTRL_COND) >> 5 {
            1 => level(32),
            2 => level(64),
            3 => level(96),
            4 => n == 0,
            5 => n != 0,
            6 => n == 127,
            7 => n != 127,
            _ => false,
        }
    }
}

/// Both directions of one serial port, and whether the guest is using them.
#[derive(Clone, Copy)]
struct SdmaPort {
    tx: SdmaChan,
    rx: SdmaChan,
    /// The guest has taken a channel out of reset, so the port's interrupt
    /// bits follow the DMA model rather than the 16550 alone. See
    /// [`MaceSerialDma`].
    owned: bool,
    /// Latches. Only `preq` can ever be set: the model has no memory errors
    /// to report on receive, and applies back-pressure rather than overrun.
    preq: bool,
    tx_memerr: bool,
    rx_overrun: bool,
}

impl SdmaPort {
    const RESET: Self =
        Self { tx: SdmaChan::RESET, rx: SdmaChan::RESET, owned: false, preq: false, tx_memerr: false, rx_overrun: false };
}

/// What the engines reach outside themselves: memory, the two UARTs, and
/// where the rings are.
pub struct SdmaIo<'a> {
    pub ram: &'a Mutex<O2Memory>,
    pub com: [&'a Com16550; 2],
    /// The ring block's physical address, from the ring base register.
    pub ring_base: u32,
}

impl SdmaIo<'_> {
    /// Where block `ptr` of ring `ring` is in memory, as an offset into it.
    /// The rings are a DMA master's view of memory, as the Ethernet's are.
    fn block(&self, ram: &O2Memory, ring: u32, ptr: u32) -> Option<usize> {
        let i = ram.dma_offset(self.ring_base | (ring << 12) | ptr)?;
        (i + sdma::BLOCK as usize <= ram.len()).then_some(i)
    }
}

/// MACE's serial DMA: the rings that carry IRIX's console once its tty
/// driver is attached.
///
/// Each port has a transmit and a receive ring of 32-byte blocks in a 32 KB
/// area the ring base register names. A block is four 64-bit pairs, each four
/// control bytes then the four data bytes they describe. Software fills
/// transmit blocks and moves the write pointer; the engine sends them and
/// moves the read pointer. On receive the engine packs characters into
/// blocks and software follows behind.
///
/// Transmit is drained at once, inside the write of the write pointer: the
/// depth is back to zero by the time the driver reads it, and a polled
/// console write between two batches cannot overtake output still queued.
/// Receive fills from the UART's input queue on the service tick and
/// whenever the read pointer moves, never past one block short of full;
/// characters that do not fit wait in the queue.
///
/// Only a port whose guest has taken a channel out of reset is "owned" by
/// the model. The PROM, NetBSD and OpenBSD never touch these registers, and
/// for them the 16550's interrupt keeps driving all six of the port's status
/// bits exactly as before. An owned port's bits are the datasheet's: the two
/// thresholds and the latches, read through the mask, with the UART's own
/// request held low because in DMA mode the engine consumes it.
pub struct MaceSerialDma {
    ports: Mutex<[SdmaPort; 2]>,
    /// Either port owned, readable without the lock, so the per-access hooks
    /// cost nothing on a machine that never uses DMA.
    any_owned: std::sync::atomic::AtomicBool,
}

impl Default for MaceSerialDma {
    fn default() -> Self { Self::new() }
}

impl MaceSerialDma {
    pub fn new() -> Self {
        Self { ports: Mutex::new([SdmaPort::RESET; 2]), any_owned: Default::default() }
    }

    /// Every channel back in reset and neither port owned, as at power-on.
    /// A warm restart that left them running would leave the next kernel's
    /// pointers out of step with the engine's, and steal the PROM's input.
    pub fn reset(&self) {
        *self.ports.lock().unwrap() = [SdmaPort::RESET; 2];
        self.any_owned.store(false, Ordering::Release);
    }

    pub fn any_owned(&self) -> bool {
        self.any_owned.load(Ordering::Acquire)
    }

    /// Port and register number. Gaps within a port's 16 KB page alias to
    /// its eight registers.
    fn decode(addr: u32) -> (usize, u32) {
        (((addr >> 14) & 1) as usize, (addr >> 3) & 7)
    }

    pub fn read(&self, addr: u32) -> u64 {
        let (p, reg) = Self::decode(addr);
        let st = self.ports.lock().unwrap();
        let port = &st[p];
        (match reg {
            0 => return port.tx.ctrl,
            1 => port.tx.rptr,
            2 => port.tx.wptr,
            3 => port.tx.depth(),
            4 => return port.rx.ctrl,
            5 => port.rx.rptr,
            6 => port.rx.wptr,
            _ => port.rx.depth(),
        }) as u64
    }

    pub fn write(&self, addr: u32, val: u64, io: &SdmaIo) {
        let (p, reg) = Self::decode(addr);
        let mut st = self.ports.lock().unwrap();
        let port = &mut st[p];
        let ptr = val as u32 & sdma::PTR_MASK;
        match reg {
            0 | 4 => {
                let tx = reg == 0;
                let ch = if tx { &mut port.tx } else { &mut port.rx };
                ch.ctrl = val & sdma::CTRL_MASK;
                if ch.in_reset() {
                    ch.rptr = 0;
                    ch.wptr = 0;
                    if tx {
                        port.preq = false;
                        port.tx_memerr = false;
                    } else {
                        port.rx_overrun = false;
                    }
                }
                port.owned = !(port.tx.in_reset() && port.rx.in_reset());
                if tx {
                    Self::tx_run(p, port, io);
                } else {
                    Self::rx_run(p, port, io);
                }
                let owned = st.iter().any(|q| q.owned);
                self.any_owned.store(owned, Ordering::Release);
            }
            1 => port.tx.rptr = ptr,
            2 => {
                port.tx.wptr = ptr;
                Self::tx_run(p, port, io);
                // In loopback what was just sent is already waiting to come
                // back in.
                Self::rx_run(p, port, io);
            }
            5 => {
                port.rx.rptr = ptr;
                Self::rx_run(p, port, io);
            }
            // The receive write pointer is the engine's, and depths are
            // computed.
            _ => {}
        }
    }

    /// Move whatever can be moved: characters that arrived between register
    /// accesses (the TCP console, a script's `feed`), and any transmit work
    /// left over.
    pub fn poll(&self, io: &SdmaIo) {
        let mut st = self.ports.lock().unwrap();
        for (p, port) in st.iter_mut().enumerate() {
            if port.owned {
                Self::tx_run(p, port, io);
                Self::rx_run(p, port, io);
            }
        }
    }

    /// Send every block between the read and write pointers.
    fn tx_run(p: usize, port: &mut SdmaPort, io: &SdmaIo) {
        let ring = 4 + 2 * p as u32;
        while port.tx.running() && port.tx.rptr != port.tx.wptr {
            let mut b = [0u8; sdma::BLOCK as usize];
            {
                let ram = io.ram.lock().unwrap();
                let Some(i) = io.block(&ram, ring, port.tx.rptr) else {
                    // Nowhere to read from: the hardware stops the channel and
                    // says why.
                    port.tx_memerr = true;
                    port.tx.ctrl &= !sdma::CTRL_ENABLE;
                    return;
                };
                b.copy_from_slice(&ram.bytes(i, sdma::BLOCK as usize));
            }
            for pair in b.chunks_exact(8) {
                for slot in 0..4 {
                    let (c, d) = (pair[slot], pair[4 + slot]);
                    match c >> 6 {
                        sdma::TX_CMD_THR => {
                            io.com[p].transmit(d);
                            port.tx.bytes += 1;
                        }
                        sdma::TX_CMD_MCR => io.com[p].write_mcr(d),
                        // Skip, or a delay: there is no wire to be idle on.
                        _ => {}
                    }
                    if c & sdma::TX_PREQ != 0 {
                        port.preq = true;
                    }
                }
            }
            port.tx.blocks += 1;
            port.tx.rptr = (port.tx.rptr + sdma::BLOCK) & sdma::PTR_MASK;
        }
    }

    /// Pack received characters into blocks while there is room.
    ///
    /// Each block is written whole, characters from the first slot on with
    /// the valid bit and every unused slot as padding (control 0, data 0xff),
    /// because IRIX's reader stops at the first all-zero control word and
    /// walks off the start of its buffer if a block it is handed has none in
    /// its first pair. The block is in memory before the write pointer says
    /// so: the service thread fills while the CPU reads.
    fn rx_run(p: usize, port: &mut SdmaPort, io: &SdmaIo) {
        let ring = 5 + 2 * p as u32;
        while port.rx.running() && port.rx.depth() < sdma::RING_FULL && !io.com[p].input_empty() {
            let mut ram = io.ram.lock().unwrap();
            let Some(i) = io.block(&ram, ring, port.rx.wptr) else { return };
            let mut chars = [0u8; 16];
            let n = io.com[p].take_input(&mut chars);
            if n == 0 {
                return;
            }
            let mut b = [0u8; sdma::BLOCK as usize];
            for (j, &c) in chars.iter().enumerate() {
                let (ctrl, data) = if j < n { (sdma::RX_VALID, c) } else { (0, 0xff) };
                b[8 * (j / 4) + j % 4] = ctrl;
                b[8 * (j / 4) + 4 + j % 4] = data;
            }
            ram.write(i, &b);
            drop(ram);
            port.rx.blocks += 1;
            port.rx.bytes += n as u64;
            port.rx.wptr = (port.rx.wptr + sdma::BLOCK) & sdma::PTR_MASK;
        }
    }

    /// Which ports are owned, and their status bits before the mask.
    pub fn status(&self) -> ([bool; 2], u64) {
        let st = self.ports.lock().unwrap();
        let mut bits = 0;
        for (p, port) in st.iter().enumerate() {
            for (on, bit) in [
                (port.tx.threshold(true), sdma::tx_thir(p)),
                (port.preq, sdma::tx_preq(p)),
                (port.tx_memerr, sdma::tx_memerr(p)),
                (port.rx.threshold(false), sdma::rx_thir(p)),
                (port.rx_overrun, sdma::rx_overrun(p)),
            ] {
                if on {
                    bits |= bit;
                }
            }
        }
        ([st[0].owned, st[1].owned], bits)
    }

    /// Software wrote the ISA status register: a pair-request latch whose
    /// bit it wrote as zero is cleared. Every other serial bit is a level or
    /// fatal, and is not the guest's to change.
    pub fn status_written(&self, val: u64) {
        let mut st = self.ports.lock().unwrap();
        for (p, port) in st.iter_mut().enumerate() {
            if val & sdma::tx_preq(p) == 0 {
                port.preq = false;
            }
        }
    }

    /// One line per port: who owns it, its control registers, pointers and
    /// what has moved.
    pub fn summary(&self) -> String {
        let st = self.ports.lock().unwrap();
        st.iter()
            .enumerate()
            .map(|(p, s)| {
                format!(
                    "port {p}{}: tx ctrl {:03x} r {:03x} w {:03x} ({} blocks, {} bytes); \
                     rx ctrl {:03x} r {:03x} w {:03x} ({} blocks, {} bytes)",
                    if s.owned { " (DMA)" } else { "" },
                    s.tx.ctrl, s.tx.rptr, s.tx.wptr, s.tx.blocks, s.tx.bytes,
                    s.rx.ctrl, s.rx.rptr, s.rx.wptr, s.rx.blocks, s.rx.bytes,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The pair of PS/2 ports, with nothing plugged into either.
///
/// We drive this machine on the serial console, so an empty port is the
/// honest model — but "empty" is not the same as "dead". A real controller
/// still drains its transmit register whether or not a keyboard is listening,
/// and the PROM waits for exactly that. Reporting zero makes every probe take
/// its full 20834-iteration timeout instead of returning at once.
pub struct MacePs2 {
    ports: [Mutex<Ps2Port>; 2],
    /// The keyboard (port 0) and, when `mouse` is set, the mouse (port 1),
    /// as one PS/2 device model. None: nothing plugged in, which is how the
    /// serial-console machines run.
    dev: Option<std::sync::Arc<crate::dev::ps2::Ps2Controller>>,
    mouse: bool,
    /// Keyboard STATUS reads with nothing to read since a byte last arrived:
    /// a guest sitting at a prompt runs this up fast.
    idle_polls: AtomicU32,
    /// The last bytes through either port, `(port, to the device?, byte)`,
    /// for working out a conversation that stalled.
    traffic: Mutex<std::collections::VecDeque<(u8, bool, u8, std::time::Instant)>>,
}

struct Ps2Port {
    control: u64,
    /// The receive latch: it keeps the last byte, which a read returns again
    /// when nothing new has arrived (IRIX reads the ID twice that way).
    rx: u8,
    rx_full: bool,
    /// A byte written to TX that has not gone out yet (TxEN clear).
    tx: Option<u8>,
    /// Bytes the guest has sent, kept so a test can see the transmit path ran.
    sent: Vec<u8>,
}

impl Default for Ps2Port {
    fn default() -> Self {
        Self { control: ps2_ctl::CLKEN, rx: 0, rx_full: false, tx: None, sent: Vec::new() }
    }
}

/// CONTROL bits of a MACE PS/2 port.
pub mod ps2_ctl {
    pub const TX_ENABLE: u64 = 1 << 1;
    pub const TX_INT: u64 = 1 << 2;
    pub const RX_INT: u64 = 1 << 3;
    /// Clock enabled: while clear the device may not send.
    pub const CLKEN: u64 = 1 << 4;
    pub const RESET: u64 = 1 << 5;
}

impl MacePs2 {
    pub fn new() -> Self {
        Self {
            ports: [Mutex::new(Ps2Port::default()), Mutex::new(Ps2Port::default())],
            dev: None,
            mouse: false,
            idle_polls: AtomicU32::new(0),
            traffic: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// With a keyboard on port 0 (and a mouse on port 1 if `mouse`).
    pub fn with_devices(mouse: bool) -> Self {
        let dev = std::sync::Arc::new(crate::dev::ps2::Ps2Controller::new(None));
        crate::traits::Device::start(&*dev);
        Self { dev: Some(dev), mouse, ..Self::new() }
    }

    /// The device model, for the host side to push keys into.
    pub fn devices(&self) -> Option<&std::sync::Arc<crate::dev::ps2::Ps2Controller>> {
        self.dev.as_ref()
    }

    /// The device on port `p`, if one is plugged in.
    fn attached(&self, p: usize) -> Option<&std::sync::Arc<crate::dev::ps2::Ps2Controller>> {
        if p == 1 && !self.mouse { None } else { self.dev.as_ref() }
    }

    /// `(port, register)` for an address in the block, if it names one.
    fn decode(addr: u32) -> (usize, u32) {
        let off = addr - MACE_PS2;
        (((off >> 5) & 1) as usize, (off >> 3) & 3)
    }

    pub fn sent(&self, port: usize) -> Vec<u8> {
        self.ports[port].lock().unwrap().sent.clone()
    }

    /// Move the device's next byte into an empty latch, if the clock lets it.
    fn latch(&self, p: usize, port: &mut Ps2Port) {
        if port.rx_full || port.control & ps2_ctl::CLKEN == 0 || port.control & ps2_ctl::RESET != 0 {
            return;
        }
        if let Some(b) = self.attached(p).and_then(|d| d.pop_from(p == 1)) {
            port.rx = b;
            self.note(p, false, b);
            port.rx_full = true;
        }
    }

    /// Send the pending byte if the transmitter is enabled.
    fn transmit(&self, p: usize, port: &mut Ps2Port) {
        if port.control & ps2_ctl::TX_ENABLE == 0 || port.control & ps2_ctl::RESET != 0 {
            return;
        }
        if let Some(b) = port.tx.take() {
            self.note(p, true, b);
            if let Some(d) = self.attached(p) {
                d.write_to(b, p == 1);
            }
        }
    }

    pub fn read(&self, addr: u32) -> u64 {
        let (p, reg) = Self::decode(addr);
        let mut port = self.ports[p].lock().unwrap();
        self.latch(p, &mut port);
        match reg {
            1 => {
                let v = port.rx as u64;
                port.rx_full = false;
                self.latch(p, &mut port);
                v
            }
            2 => port.control,
            3 => {
                if p == 0 {
                    if port.rx_full {
                        self.idle_polls.store(0, Ordering::Relaxed);
                    } else {
                        self.idle_polls.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let mut st = ps2_bit::CLOCK_SIGNAL;
                if port.control & ps2_ctl::CLKEN != 0 && port.control & ps2_ctl::RESET == 0 {
                    st |= ps2_bit::CLOCK_INHIBIT;
                }
                if port.tx.is_none() {
                    st |= ps2_bit::TX_EMPTY;
                }
                if port.rx_full {
                    st |= ps2_bit::RX_FULL;
                }
                st
            }
            _ => 0,
        }
    }

    pub fn write(&self, addr: u32, val: u64) {
        let (p, reg) = Self::decode(addr);
        let mut port = self.ports[p].lock().unwrap();
        match reg {
            0 => {
                if port.sent.len() < 64 {
                    port.sent.push(val as u8);
                }
                if port.control & ps2_ctl::RESET == 0 {
                    port.tx = Some(val as u8);
                    self.transmit(p, &mut port);
                }
            }
            2 => {
                port.control = val & 0x3f;
                if port.control & ps2_ctl::RESET != 0 {
                    port.tx = None;
                    port.rx_full = false;
                }
                self.transmit(p, &mut port);
                self.latch(p, &mut port);
            }
            _ => {}
        }
    }

    fn note(&self, p: usize, out: bool, b: u8) {
        let mut t = self.traffic.lock().unwrap();
        if t.len() >= 4000 {
            t.pop_front();
        }
        t.push_back((p as u8, out, b, std::time::Instant::now()));
    }

    /// The recent traffic as text: `>` to a device, `<` from it, per port.
    pub fn traffic(&self) -> String {
        self.traffic
            .lock()
            .unwrap()
            .iter()
            .map(|&(p, out, b, _)| format!("{p}{}{b:02x}", if out { '>' } else { '<' }))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The recent traffic one byte per line, with host milliseconds since
    /// the first byte kept: to tell a periodic exchange from a reaction.
    pub fn traffic_timed(&self) -> String {
        let t = self.traffic.lock().unwrap();
        let Some(&(_, _, _, t0)) = t.front() else { return String::new() };
        t.iter()
            .map(|&(p, out, b, at)| {
                format!("{:9.1} {p}{}{b:02x}\n", at.duration_since(t0).as_secs_f64() * 1000.0, if out { '>' } else { '<' })
            })
            .collect()
    }

    /// Keyboard status reads with nothing waiting, since the last byte.
    pub fn idle_polls(&self) -> u32 {
        self.idle_polls.load(Ordering::Relaxed)
    }

    pub fn reset_idle(&self) {
        self.idle_polls.store(0, Ordering::Relaxed);
    }

    /// The PS/2 bits of MACE's ISA interrupt status: 9 and 11 are the ports'
    /// interrupt requests (receive full with RxIEN, transmit empty with
    /// TxIEN), 10 and 12 their unmasked receive-full flags.
    pub fn isa_bits(&self) -> u64 {
        let mut bits = 0;
        for p in 0..2 {
            let mut port = self.ports[p].lock().unwrap();
            self.latch(p, &mut port);
            let irq = (port.control & ps2_ctl::RX_INT != 0 && port.rx_full)
                || (port.control & ps2_ctl::TX_INT != 0 && port.tx.is_none());
            if irq {
                bits |= 1 << (9 + 2 * p);
            }
            if port.rx_full {
                bits |= 1 << (10 + 2 * p);
            }
        }
        bits
    }
}

impl Default for MacePs2 {
    fn default() -> Self {
        Self::new()
    }
}

/// What is plugged into an O2 beyond its memory: the SCSI controllers and
/// their disks, and where the PROM's flash (its environment) is kept.
#[derive(Clone, Debug, Default)]
pub struct Ip32Options {
    /// The two on-board AIC-7880s. Without them the PROM finds nothing to
    /// boot from.
    pub scsi: bool,
    pub disks: Vec<Ip32Disk>,
    /// A file holding the flash, so `setenv` survives from run to run.
    pub flash_file: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Ip32Disk {
    /// SCSI id; None is the controller's default.
    pub id: Option<u8>,
    /// Which controller: 0 is the internal bus, 1 the external connector.
    /// IRIX looks only at ids 1 to 4 on the internal one, as the case has
    /// room for no more, so further drives go outside.
    pub bus: u8,
    pub path: std::path::PathBuf,
    /// A CD-ROM, which the PROM's install menu insists on seeing.
    pub cdrom: bool,
}

impl Ip32Options {
    /// IRIS_IP32_SCSI (controllers on), IRIS_IP32_DISK (one disk at the
    /// default id), IRIS_IP32_DISKS=id=path[:cd],... (more than one: an install
    /// needs the media on one id and the target on another), IRIS_IP32_FLASH.
    pub fn from_env() -> Self {
        let mut disks = Vec::new();
        if let Ok(path) = std::env::var("IRIS_IP32_DISK") {
            disks.push(Ip32Disk { id: None, bus: 0, path: path.into(), cdrom: false });
        }
        if let Ok(spec) = std::env::var("IRIS_IP32_DISKS") {
            for entry in spec.split(',') {
                let Some((id, path)) = entry.split_once('=') else { continue };
                let Ok(id) = id.trim().parse::<u8>() else { continue };
                let (path, cdrom) = match path.trim().strip_suffix(":cd") {
                    Some(p) => (p, true),
                    None => (path.trim(), false),
                };
                disks.push(Ip32Disk { id: Some(id), bus: 0, path: path.into(), cdrom });
            }
        }
        Self {
            scsi: std::env::var("IRIS_IP32_SCSI").is_ok(),
            disks,
            flash_file: std::env::var_os("IRIS_IP32_FLASH").map(Into::into),
        }
    }
}

/// A Dallas-style RTC with battery-backed NVRAM.
///
/// Registers are reached with MACE's ISA spacing, `(reg << 8) + 7`. The low
/// registers are the clock; everything from [`NVRAM_FIRST`] up is storage that
/// survives power loss on a real machine — which is precisely why firmware
/// trusts it, and why handing back zeros is not neutral.
pub struct MaceRtc {
    cells: Mutex<Vec<u8>>,
    /// The clock: guest seconds since 1970 at a host instant, so it runs.
    /// A kernel that calibrates its CPU clock against the RTC waits for the
    /// seconds to change, and a stopped clock leaves it waiting for ever
    /// (OpenBSD, straight after autoconfiguration).
    clock: Mutex<(i64, std::time::Instant)>,
    /// Set when the guest writes PAB in extended control register 4A: on an
    /// O2 that is software power-off (the RTC lets go of the supply). Both
    /// NetBSD and OpenBSD power down this way; without it OpenBSD retries
    /// for ever, printing "Power supply did not shut down".
    powered_off: std::sync::atomic::AtomicBool,
}

/// The DS1687's clock registers and the bits of register B that shape them.
mod rtc_reg {
    pub const SEC: usize = 0x00;
    pub const MIN: usize = 0x02;
    pub const HOUR: usize = 0x04;
    pub const WDAY: usize = 0x06;
    pub const MDAY: usize = 0x07;
    pub const MONTH: usize = 0x08;
    pub const YEAR: usize = 0x09;
    pub const A: usize = 0x0a;
    /// Register A's bank select: with it set, 0x40-0x7f are the extended
    /// registers, and 0x48 is the century.
    pub const A_DV0: u8 = 0x10;
    pub const CENTURY: usize = 0x48;
    /// Extended control register 4A (bank 1), and its power-active-bar bit.
    pub const XCTRL4A: usize = 0x4a;
    pub const XCTRL4A_PAB: u8 = 0x08;
    pub const B: usize = 0x0b;
    pub const B_SET: u8 = 0x80;
    pub const B_BINARY: u8 = 0x04;
    pub const B_24H: u8 = 0x02;
    pub const TIME: [usize; 7] = [SEC, MIN, HOUR, WDAY, MDAY, MONTH, YEAR];
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The date for a count of days since 1970-01-01: (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
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
        // BCD, 24-hour: what SGI firmware leaves the part in.
        c[rtc_reg::B] = rtc_reg::B_24H;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64).unwrap_or(0);
        Self {
            cells: Mutex::new(c),
            clock: Mutex::new((now, std::time::Instant::now())),
            powered_off: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether the guest has switched the machine off.
    pub fn powered_off(&self) -> bool {
        self.powered_off.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Guest time now, in seconds since 1970.
    fn now(&self) -> i64 {
        let (secs, at) = *self.clock.lock().unwrap();
        secs + at.elapsed().as_secs() as i64
    }

    /// The clock registers' value for `t`, in the mode register B selects.
    fn field(&self, reg: usize, t: i64, b: u8) -> u8 {
        let days = t.div_euclid(86400);
        let secs = t.rem_euclid(86400);
        let (y, m, d) = civil_from_days(days);
        let mut hour = (secs / 3600) as u8;
        let mut pm = 0u8;
        if reg == rtc_reg::HOUR && b & rtc_reg::B_24H == 0 {
            pm = if hour >= 12 { 0x80 } else { 0 };
            hour %= 12;
            if hour == 0 { hour = 12; }
        }
        let v = match reg {
            rtc_reg::SEC => (secs % 60) as u8,
            rtc_reg::MIN => (secs / 60 % 60) as u8,
            rtc_reg::HOUR => hour,
            // 1 = Sunday; 1970-01-01 was a Thursday.
            rtc_reg::WDAY => ((days + 4).rem_euclid(7) + 1) as u8,
            rtc_reg::MDAY => d as u8,
            rtc_reg::MONTH => m as u8,
            _ => y.rem_euclid(100) as u8,
        };
        let v = if b & rtc_reg::B_BINARY != 0 { v } else { (v / 10) << 4 | v % 10 };
        v | pm
    }

    /// Restart the clock from what the guest has written into the time
    /// registers. A two-digit year below 70 is taken as 20xx.
    fn set_from_cells(&self) {
        self.set_from_cells_in(None)
    }

    /// The same, with the century given (a write to the century register).
    fn set_from_cells_in(&self, century: Option<i64>) {
        let c = self.cells.lock().unwrap();
        let b = c[rtc_reg::B];
        let get = |r: usize| -> i64 {
            let raw = c[r] & if r == rtc_reg::HOUR && b & rtc_reg::B_24H == 0 { 0x7f } else { 0xff };
            (if b & rtc_reg::B_BINARY != 0 { raw } else { (raw >> 4) * 10 + (raw & 0x0f) }) as i64
        };
        let mut hour = get(rtc_reg::HOUR);
        if b & rtc_reg::B_24H == 0 {
            hour %= 12;
            if c[rtc_reg::HOUR] & 0x80 != 0 { hour += 12; }
        }
        let yy = get(rtc_reg::YEAR);
        let year = match century {
            Some(c) => c * 100 + yy,
            None if yy < 70 => 2000 + yy,
            None => 1900 + yy,
        };
        let (month, mday) = (get(rtc_reg::MONTH).clamp(1, 12), get(rtc_reg::MDAY).clamp(1, 31));
        let t = days_from_civil(year, month, mday) * 86400
            + hour * 3600 + get(rtc_reg::MIN) * 60 + get(rtc_reg::SEC);
        drop(c);
        *self.clock.lock().unwrap() = (t, std::time::Instant::now());
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
        let Some(r) = Self::reg_of(addr) else { return BusRead8::ok(0) };
        let b = self.peek(rtc_reg::B);
        if r == rtc_reg::CENTURY && self.peek(rtc_reg::A) & rtc_reg::A_DV0 != 0 {
            let days = self.now().div_euclid(86400);
            let c = (civil_from_days(days).0 / 100) as u8;
            return BusRead8::ok(if b & rtc_reg::B_BINARY != 0 { c } else { (c / 10) << 4 | c % 10 });
        }
        // While SET is up the clock is frozen at what is being written.
        if rtc_reg::TIME.contains(&r) && b & rtc_reg::B_SET == 0 {
            return BusRead8::ok(self.field(r, self.now(), b));
        }
        BusRead8::ok(self.peek(r))
    }
    fn write8(&self, addr: u32, val: u8) -> u32 {
        let Some(r) = Self::reg_of(addr) else { return BUS_OK };
        let b = self.peek(rtc_reg::B);
        if r == rtc_reg::XCTRL4A && self.peek(rtc_reg::A) & rtc_reg::A_DV0 != 0 {
            if val & rtc_reg::XCTRL4A_PAB != 0 {
                self.powered_off.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return BUS_OK;
        }
        if r == rtc_reg::CENTURY && self.peek(rtc_reg::A) & rtc_reg::A_DV0 != 0 {
            // The century, in the extended bank: keep the rest of the time.
            let t = self.now();
            for &f in &rtc_reg::TIME {
                let v = self.field(f, t, b);
                self.poke(f, v);
            }
            let c = if b & rtc_reg::B_BINARY != 0 { val } else { (val >> 4) * 10 + (val & 0x0f) };
            self.set_from_cells_in(Some(c as i64));
            return BUS_OK;
        }
        if r == rtc_reg::B && b & rtc_reg::B_SET == 0 && val & rtc_reg::B_SET != 0 {
            // SET going up: freeze the time registers at the current time,
            // so fields the guest does not rewrite keep their values.
            let t = self.now();
            for &f in &rtc_reg::TIME {
                let v = self.field(f, t, b);
                self.poke(f, v);
            }
        }
        self.poke(r, val);
        if r == rtc_reg::B && b & rtc_reg::B_SET != 0 && val & rtc_reg::B_SET == 0 {
            // SET coming down: the clock runs on from what was written.
            self.set_from_cells();
        } else if rtc_reg::TIME.contains(&r) && b & rtc_reg::B_SET == 0 {
            // A field written without SET: freeze the others at the current
            // time first, then take the whole lot as the new time.
            let t = self.now();
            for &f in &rtc_reg::TIME {
                if f != r {
                    let v = self.field(f, t, b);
                    self.poke(f, v);
                }
            }
            self.set_from_cells();
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

/// Thresholds, and a lesson about calibration.
///
/// These were first set from the widths the PROM produced, which were
/// `32 / 357 / 1980` microseconds — four to six times the 1-Wire spec's
/// `6 / 60 / 480`. That looked like the PROM simply bit-banging slowly, and
/// the thresholds were moved out to match. They were in fact compensating for
/// CRIME's counter, which advanced once per *read* instead of with time, so
/// every delay the firmware asked for came out stretched.
///
/// With [`CRIME_TICKS_PER_US`] driving that counter from the same clock as
/// the UST, the measured widths are `8 / 90 / 500` and the datasheet numbers
/// work unmodified. A device model that needs constants tuned away from the
/// specification is usually telling you something about the clock, not about
/// the device.
pub const OW_RESET_US: u64 = 300;
pub const OW_WRITE0_US: u64 = 30;

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
    /// When set, the clock is the host's: microseconds since this instant,
    /// read live. The 1-Wire ID chip's slots are tens of microseconds, so a
    /// clock stepped from another thread cannot time them.
    host: std::sync::OnceLock<std::time::Instant>,
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
    pub fn new() -> Self { Self { ticks: Mutex::new(0), host: std::sync::OnceLock::new() } }

    /// Run from the host clock from now on (a machine, not the harness).
    pub fn use_host_clock(&self) {
        let _ = self.host.set(std::time::Instant::now());
    }

    /// Set the clock to `us` microseconds, for a machine that keeps time by
    /// the host clock rather than by instructions run.
    pub fn set_us(&self, us: u64) {
        *self.ticks.lock().unwrap() = us;
    }

    /// Advance the clock to match `instructions` executed so far.
    pub fn advance_to(&self, instructions: u64) {
        *self.ticks.lock().unwrap() = instructions * UST_TICKS_NUM / UST_TICKS_DEN;
    }

    fn tick(&self) -> u64 { self.now() }
    pub fn now(&self) -> u64 {
        match self.host.get() {
            Some(t0) => t0.elapsed().as_micros() as u64,
            None => *self.ticks.lock().unwrap(),
        }
    }
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
    /// Which code touches each offset. "Polled 80,000 times" is a symptom;
    /// the PC is what lets you go and read the loop.
    who: Mutex<std::collections::BTreeMap<u32, Vec<(u32, u32)>>>,
    /// Set by the owner so the PC can be attributed.
    pc: Mutex<Option<std::sync::Arc<PcTap>>>,
}

impl Stub {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            reads: Mutex::new(0),
            writes: Mutex::new(0),
            hot: Mutex::new(std::collections::BTreeMap::new()),
            cells: Mutex::new(std::collections::BTreeMap::new()),
            who: Mutex::new(std::collections::BTreeMap::new()),
            pc: Mutex::new(None),
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
    pub fn cell_pub(&self, addr: u32) -> u64 {
        self.cell(addr)
    }
    pub fn set_cell_pub(&self, addr: u32, val: u64) {
        self.set_cell(addr, val)
    }
    fn set_cell(&self, addr: u32, val: u64) {
        self.cells.lock().unwrap().insert(addr & 0x007f_ffff & !7, val);
    }
    fn hit(&self, addr: u32, write: bool) {
        let off = addr & 0x007f_ffff & !3;
        let mut h = self.hot.lock().unwrap();
        let e = h.entry(off).or_insert((0, 0));
        if write { e.1 += 1 } else { e.0 += 1 }
        drop(h);
        if let Some(tap) = self.pc.lock().unwrap().as_ref() {
            let who = (tap.get(), tap.get_ra());
            let mut w = self.who.lock().unwrap();
            let v = w.entry(off).or_default();
            if v.len() < 4 && !v.contains(&who) {
                v.push(who);
            }
        }
    }

    /// Attribute this stub's accesses to the PC making them.
    pub fn watch_pc(&self, tap: std::sync::Arc<PcTap>) {
        *self.pc.lock().unwrap() = Some(tap);
    }

    /// The PCs seen touching an offset.
    pub fn who_touched(&self, off: u32) -> Vec<(u32, u32)> {
        self.who.lock().unwrap().get(&off).cloned().unwrap_or_default()
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
    pub(crate) ram: std::sync::Arc<Mutex<O2Memory>>,
    /// The flash. Writable, because an O2 keeps its environment variables in
    /// it -- there is no separate NVRAM chip -- and the installer saves
    /// settings there. The user's PROM file is never touched: this is a copy.
    prom: Mutex<Vec<u8>>,
    /// Writes into the flash, so a command protocol would be visible rather
    /// than silently corrupting the image.
    prom_writes: Mutex<Vec<(u32, u8)>>,
    /// Where to keep the flash between runs, if anywhere. Never the image the
    /// caller supplied: settings written by a guest belong in the user's
    /// working directory, not in their copy of the PROM.
    flash_file: Option<std::path::PathBuf>,
    pub crime: Crime,
    pub mace: Stub,
    /// CRIME's rendering engine: draws into RAM through its tile tables.
    pub crime_re: crate::dev::o2gfx::CrimeRe,
    /// The display back end: scans the frame buffer out of RAM.
    pub gbe: crate::dev::o2gfx::Gbe,
    /// VICE, the video/image compression engine: a register file for now.
    /// IRIX probes it at boot (OpenBSD and NetBSD never look), and a bus
    /// error there panics the kernel.
    pub vice: Stub,
    pub macepci: MacePci,
    pub ust: std::sync::Arc<MaceUst>,
    pub nic_trace: NicTrace,
    pub rtc: MaceRtc,
    pub ps2: MacePs2,
    /// See [`Ip32Bus::set_irq_sink`].
    irq_sink: std::sync::OnceLock<Box<dyn Fn(bool) + Send + Sync>>,
    irq_level: std::sync::atomic::AtomicBool,
    pub onewire: OneWireId,
    pub com0: Com16550,
    pub com1: Com16550,
    /// The serial ports' DMA rings, which IRIX's console runs on.
    pub sdma: MaceSerialDma,
    /// The Ethernet. Its frames go nowhere until `start_nat` (IRIS_IP32_NET).
    pub mec: std::sync::Arc<crate::dev::mec::Mec>,
    pub pci_view: PciNativeView,
    /// Accesses that hit nothing, first 64 kept, as `(addr, is_write, pc)`.
    unmapped: Mutex<Vec<(u32, bool, u32)>>,
    /// The PC of the instruction currently executing.
    pub pc: std::sync::Arc<PcTap>,
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
    watch: Mutex<Option<(u32, u32)>>,
    watch_log: Mutex<Vec<(u32, bool, u64, u32, u64, u8)>>,
    /// Entries kept (IRIS_IP32_WATCHMEM_CAP, default 256) and whether reads
    /// are left out (IRIS_IP32_WATCHMEM_WRITES).
    watch_cap: usize,
    watch_writes_only: bool,
    /// The SCSI controller, when one is attached, so the trace can report
    /// what the driver asked it for.
    pub scsi: Mutex<Vec<(std::sync::Arc<crate::dev::aic7880::Aic7880>, u32)>>,
}

impl Ip32Bus {
    /// `ram_bytes` of memory in the usual fitting for that size, or, for a
    /// size no O2 can have, one bank of it (power-of-two sizes only). `prom`
    /// must be the raw 512 KiB IP32 PROM image.
    pub fn new(ram_bytes: usize, prom: Vec<u8>) -> Self {
        let mem = O2Memory::with_total((ram_bytes >> 20) as u32)
            .ok()
            .filter(|m| m.total() as usize == ram_bytes)
            .unwrap_or_else(|| O2Memory::single(ram_bytes));
        Self::with_memory(mem, prom)
    }

    /// A machine with `mem` fitted, configured from the IRIS_IP32_* variables.
    pub fn with_memory(mem: O2Memory, prom: Vec<u8>) -> Self {
        Self::with_options(mem, prom, Ip32Options::from_env())
    }

    /// A machine with `mem` fitted, configured by `opts`.
    pub fn with_options(mem: O2Memory, prom: Vec<u8>, opts: Ip32Options) -> Self {
        let pc = std::sync::Arc::new(PcTap::default());
        let ram = std::sync::Arc::new(Mutex::new(mem));
        let ust = std::sync::Arc::new(MaceUst::new());
        let bus = Self {
            ram: ram.clone(),
            prom: Mutex::new(prom),
            prom_writes: Mutex::new(Vec::new()),
            flash_file: opts.flash_file.clone(),
            crime: Crime::new(ust.clone()),
            mace: Stub::new("mace"),
            crime_re: crate::dev::o2gfx::CrimeRe::new(ram.clone()),
            vice: Stub::new("vice"),
            gbe: {
                let ust = ust.clone();
                crate::dev::o2gfx::Gbe::new(std::sync::Arc::new(move || ust.now()))
            },
            macepci: MacePci::new(),
            ust: ust.clone(),
            nic_trace: NicTrace::new(),
            rtc: MaceRtc::new(),
            ps2: MacePs2::new(),
            irq_sink: std::sync::OnceLock::new(),
            irq_level: std::sync::atomic::AtomicBool::new(false),
            // A locally-administered address; nothing depends on the value yet.
            onewire: OneWireId::from_mac([0x08, 0x00, 0x69, 0x12, 0x34, 0x56]),
            com0: Com16550::new(),
            com1: Com16550::new(),
            sdma: MaceSerialDma::new(),
            mec: std::sync::Arc::new(crate::dev::mec::Mec::new(ram.clone())),
            pci_view: PciNativeView::new(),
            unmapped: Mutex::new(Vec::new()),
            pc: pc.clone(),
            fw_writes: Mutex::new(0),
            sizemem: Mutex::new(Vec::new()),
            watch: Mutex::new(std::env::var("IRIS_IP32_WATCHMEM").ok().and_then(|v| {
                let (a, b) = v.split_once(':').unwrap_or((&v, &v));
                let p = |x: &str| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok();
                Some((p(a)?, p(b)? + 8))
            })),
            watch_log: Mutex::new(Vec::new()),
            watch_cap: std::env::var("IRIS_IP32_WATCHMEM_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(256),
            watch_writes_only: std::env::var("IRIS_IP32_WATCHMEM_WRITES").is_ok(),
            scsi: Mutex::new(Vec::new()),
        };
        // An experiment for now: the PROM probes slots 1..3 for a vendor ID
        // and finds nothing, so it has no disk and cannot boot. Putting the
        // controller a real O2 has there is how we learn how much of it the
        // PROM insists on.
        if opts.scsi {
            // The controller fetches its work from host memory, so it
            // needs to reach RAM directly rather than through the bus that
            // owns it.
            let scsi = std::sync::Arc::new(crate::dev::aic7880::Aic7880::new(ram.clone()));
            // The second controller. Real hardware has it whether or not
            // anything is plugged into it, and a kernel that expects two
            // waits on the one that is missing.
            let scsi_b = std::sync::Arc::new(crate::dev::aic7880::Aic7880::new(ram.clone()));
            bus.macepci.attach(
                IP32_SCSI_SLOT_B,
                std::sync::Arc::new(
                    PciFunction::new(
                        "aic7880-b",
                        PCI_VENDOR_ADAPTEC,
                        PCI_DEVICE_AIC7880,
                        PCI_CLASS_SCSI,
                        0x01,
                        [0x100, 0x1000, 0, 0, 0, 0],
                    )
                    .with_ops(scsi_b.clone()),
                ),
            );
            bus.macepci.attach(
                IP32_SCSI_SLOT,
                std::sync::Arc::new(
                    PciFunction::new(
                        "aic7880",
                        PCI_VENDOR_ADAPTEC,
                        PCI_DEVICE_AIC7880,
                        PCI_CLASS_SCSI,
                        0x01,
                        // 256 bytes of I/O, 4 KiB of memory: what the part has.
                        [0x100, 0x1000, 0, 0, 0, 0],
                    )
                    .with_ops(scsi.clone()),
                ),
            );
            for d in &opts.disks {
                let ctl = if d.bus == 0 { &scsi } else { &scsi_b };
                let id = d.id.unwrap_or(ctl.default_target());
                let at = if d.bus == 0 { format!("id {id}") } else { format!("{}:{id}", d.bus) };
                match ctl.attach_at(id, &d.path, d.cdrom) {
                    Ok(b) => eprintln!("ip32: SCSI {at}: {} ({b} blocks)", d.path.display()),
                    Err(e) => eprintln!("ip32: SCSI {at}: {}: {e}", d.path.display()),
                }
            }
            let mut list = bus.scsi.lock().unwrap();
            list.push((scsi, IP32_SCSI_CRIME_INT));
            list.push((scsi_b, IP32_SCSI_CRIME_INT_B));
        }
        // A saved flash carries the guest's environment -- boot paths, console
        // choice, everything the PROM's `setenv` writes -- from one run to
        // the next, which is what makes an install worth doing once.
        if let Some(path) = bus.flash_file.clone() {
            match std::fs::read(&path) {
                Ok(saved) if saved.len() == PROM_SIZE as usize => {
                    *bus.prom.lock().unwrap() = saved;
                    eprintln!("ip32: flash restored from {}", path.display());
                }
                Ok(_) => eprintln!("ip32: flash {} is the wrong size; ignoring", path.display()),
                Err(_) => {}
            }
        }
        // So a hot offset can name the code polling it.
        bus.mace.watch_pc(pc.clone());
        let _ = pc;
        bus
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

    /// Memory fitted, in bytes.
    pub fn ram_len(&self) -> u32 {
        self.ram.lock().unwrap().total()
    }

    /// A CRIME write that may have moved a memory bank.
    /// GBE's picture out of RAM into `out` (R, G, B, A bytes), or None while
    /// the display is off. See [`crate::dev::o2gfx::Gbe::render`].
    pub fn render_screen(&self, out: &mut [u8]) -> Option<(u32, u32)> {
        self.gbe.render_from(&self.ram, out)
    }

    /// Where IP2 goes when CRIME's output changes, for a machine whose CPU
    /// runs on its own thread. Without one (the harness) nothing here runs.
    pub fn set_irq_sink(&self, sink: Box<dyn Fn(bool) + Send + Sync>) {
        let _ = self.irq_sink.set(sink);
    }

    fn is_ram_addr(addr: u32) -> bool {
        addr < LOW_ALIAS_SIZE || addr >= RAM_BASE
    }

    #[inline]
    fn before_read(&self, addr: u32) {
        if self.irq_sink.get().is_some() && self.in_crime(addr) {
            self.refresh_irq();
        }
    }

    #[inline]
    fn after_write(&self, addr: u32) {
        if self.irq_sink.get().is_some() && !Self::is_ram_addr(addr) {
            self.refresh_irq();
        }
    }

    /// Device lines into CRIME and, if CRIME's output changed, into the sink.
    pub fn refresh_irq(&self) -> bool {
        self.update_lines();
        let level = self.crime.interrupting();
        if let Some(sink) = self.irq_sink.get() {
            if self.irq_level.swap(level, Ordering::AcqRel) != level {
                sink(level);
            }
        }
        level
    }

    /// The SCSI controllers, the Ethernet, com0 and the PS/2 ports, into
    /// their CRIME inputs.
    fn update_lines(&self) {
        // GBE's vertical-timing interrupts: the beam crossed a VT_INTRn line.
        let crossed = self.gbe.vt_crossings();
        for n in 0..4 {
            if crossed & (1 << n) != 0 {
                self.crime.latch_int(16 + n);
            }
        }
        for (scsi, line) in self.scsi.lock().unwrap().iter() {
            self.crime.set_int(*line, scsi.interrupting());
        }
        self.crime.set_int(IP32_MEC_CRIME_INT, self.mec.interrupting());
        self.serial_touched(true);
        self.ps2_touched();
    }

    /// The serial ports' bits of MACE's ISA status, and CRIME line 4 from
    /// them. Only these bits are set and cleared; overwriting the whole
    /// register would erase the guest's own and other devices' bits.
    ///
    /// A port the guest drives by PIO keeps the behaviour the PROM and the
    /// BSDs boot with: all six bits follow the 16550's interrupt (`legacy`
    /// says whether to look at it again; port 1 has never contributed), and
    /// go to the line whatever the mask says. A port in DMA mode has the
    /// datasheet's bits, read through the mask, and the line follows them
    /// only while unmasked: IRIX acknowledges a level by masking it and
    /// leaving the work to a thread, and a line that ignored the mask would
    /// bring it straight back into the dispatcher, never letting that thread
    /// run.
    fn serial_touched(&self, legacy: bool) {
        let mut isa = self.mace.cell(MACE_ISA_INT_STATUS);
        let (owned, bits) = if self.sdma.any_owned() { self.sdma.status() } else { ([false; 2], 0) };
        let mask = if owned.contains(&true) { self.mace.cell(MACE_ISA_INT_MASK) } else { 0 };
        for p in 0..2 {
            let g = sdma::group(p);
            if owned[p] {
                isa = (isa & !g) | (bits & g & mask);
            } else if legacy && p == 0 {
                // The console's receive interrupt: a kernel does not poll
                // the UART.
                isa = if self.com0.interrupting() { isa | g } else { isa & !g };
            }
        }
        self.mace.set_cell(MACE_ISA_INT_STATUS, isa);
        // Line 4 is serial and parallel only (bits 16..31); bits 8..15 are
        // line 5's, from the PS/2 ports among others.
        self.crime.set_int(IP32_ISA_CRIME_INT, isa & 0xffff_0000 != 0);
    }

    /// The engines' view of the machine.
    fn sdma_io(&self) -> SdmaIo<'_> {
        SdmaIo {
            ram: &self.ram,
            com: [&self.com0, &self.com1],
            ring_base: (self.mace.cell(MACE_ISA_RING_BASE) & sdma::RING_BASE_MASK) as u32,
        }
    }

    fn in_sdma(&self, addr: u32) -> bool {
        (MACE_SDMA..MACE_SDMA + MACE_SDMA_SIZE).contains(&addr)
    }

    /// A serial DMA register, read. Counted with MACE's other offsets, so a
    /// guest polling one shows up where every other hot register does.
    fn sdma_read(&self, addr: u32) -> u64 {
        self.mace.r();
        self.mace.hit(addr, false);
        self.sdma.read(addr)
    }

    fn sdma_write(&self, addr: u32, val: u64) -> u32 {
        self.mace.w();
        self.mace.hit(addr, true);
        self.sdma.write(addr, val, &self.sdma_io());
        // A pointer or control write moves the levels, and may hand the port
        // back to (or take it from) the 16550's own interrupt.
        self.serial_touched(true);
        BUS_OK
    }

    /// Before a read of a register held as a plain MACE cell: the ISA status
    /// is brought up to date for a DMA port, so the dispatcher's one read of
    /// it is current even between service ticks.
    #[inline]
    fn before_mace_read(&self, addr: u32) {
        if addr & !7 == MACE_ISA_INT_STATUS && self.sdma.any_owned() {
            self.serial_touched(false);
        }
    }

    /// Bring every device interrupt up to date in CRIME: the SCSI
    /// controllers, the Ethernet (and its frames from the network), com0 and
    /// the PS/2 ports. Returns whether CRIME is interrupting the CPU (IP2).
    /// The harness calls it every 256 instructions, the machine from its
    /// service thread.
    pub fn service(&self) -> bool {
        self.mec.poll();
        if self.sdma.any_owned() {
            self.sdma.poll(&self.sdma_io());
        }
        self.refresh_irq()
    }

    /// After any PS/2 register access: the ports' bits of MACE's ISA
    /// interrupt status, and CRIME line 5 from them, follow at once. A level
    /// left high after the handler drained the port reads as a spurious
    /// interrupt to OpenBSD.
    pub fn ps2_touched(&self) {
        if self.ps2.devices().is_none() {
            return;
        }
        let bits = self.ps2.isa_bits();
        let isa = (self.mace.cell_pub(MACE_ISA_INT_STATUS) & !MACE_ISA_PS2_MASK) | bits;
        self.mace.set_cell_pub(MACE_ISA_INT_STATUS, isa);
        let mask = self.mace.cell_pub(MACE_ISA_INT_MASK);
        self.crime.set_int(IP32_MISC_CRIME_INT, isa & mask & 0xff00 != 0);
    }

    /// After a write to a MACE register held as a plain cell.
    fn mace_wrote(&self, addr: u32) {
        // MACE revision 2 or later: SGI software reads bit 4 of the ISA ring
        // base register as that, and with it clear shifts every PS/2 reply
        // right one bit to work around revision 1.
        if addr & !7 == MACE_ISA_RING_BASE && self.ps2.devices().is_some() {
            let v = self.mace.cell_pub(MACE_ISA_RING_BASE);
            self.mace.set_cell_pub(MACE_ISA_RING_BASE, v | MACE_REV2_BIT);
        }
        // A DMA port's status bits are not the guest's to set, and its mask
        // decides what it sees of them.
        let reg = addr & !7;
        if reg == MACE_ISA_INT_STATUS {
            self.sdma.status_written(self.mace.cell(MACE_ISA_INT_STATUS));
        }
        if (reg == MACE_ISA_INT_STATUS || reg == MACE_ISA_INT_MASK) && self.sdma.any_owned() {
            self.serial_touched(false);
        }
    }

    /// Plug in what a graphics console needs: a PS/2 keyboard on port 0, a
    /// CRIME that reads as revision 1.1 (ID 0xa1, the value the PROM requires
    /// before it adds the graphics component) and MACE revision 2.
    pub fn enable_graphics(&mut self) {
        self.enable_graphics_with_mouse(false);
    }

    /// The same, with a PS/2 mouse on port 1 as well.
    pub fn enable_graphics_with_mouse(&mut self, mouse: bool) {
        self.ps2 = MacePs2::with_devices(mouse);
        self.crime.revision = CRIME_REV_GBE;
        let v = self.mace.cell_pub(MACE_ISA_RING_BASE);
        self.mace.set_cell_pub(MACE_ISA_RING_BASE, v | MACE_REV2_BIT);
    }

    fn crime_wrote(&self, addr: u32) {
        let off = (addr - CRIME_BASE) & !7;
        if (crime_reg::MEM_BANK_CTRL0..crime_reg::MEM_BANK_CTRL0 + 8 * 8).contains(&off) {
            let v = self.crime.load(off);
            self.ram.lock().unwrap().set_ctrl(((off - crime_reg::MEM_BANK_CTRL0) / 8) as usize, v);
        }
    }

    /// Record an access if it falls in the watch range.
    fn watch_hit(&self, addr: u32, write: bool, val: u64, n: usize) {
        if let Some((lo, hi)) = *self.watch.lock().unwrap() {
            // (0, 0) watches whatever the display is scanning out.
            let hit = if (lo, hi) == (0, 0) {
                addr >= RAM_BASE && self.gbe.displayed.lock().unwrap().contains(&((addr - RAM_BASE) >> 16))
            } else {
                addr < hi && addr + n as u32 > lo
            };
            if hit && (write || !self.watch_writes_only) {
                let mut w = self.watch_log.lock().unwrap();
                if w.len() < self.watch_cap {
                    // The harness sets the PC every step; the machine only
                    // knows where its last batch ended.
                    let pc = match self.pc.get() {
                        0 => crate::ip32_machine::LAST_PC.load(Ordering::Relaxed),
                        pc => pc,
                    };
                    w.push((addr, write, val, pc, self.ust.now(), n as u8));
                }
            }
        }
    }

    /// Watch physical `lo..hi` (as the bus sees it, RAM at 0x4000_0000) from
    /// now on, with an empty log; None stops watching.
    pub fn set_watch(&self, range: Option<(u32, u32)>) {
        *self.watch.lock().unwrap() = range;
        self.watch_log.lock().unwrap().clear();
    }

    /// The watch log as text, one access per line.
    pub fn watch_text(&self) -> String {
        let w = self.watch_log.lock().unwrap();
        let mut s = String::new();
        for (a, wr, v, p, t, n) in w.iter() {
            s.push_str(&format!("{t} {} {n} 0x{a:08x} 0x{v:0w$x} pc 0x{p:08x}\n", if *wr { "W" } else { "R" }, w = 2 * *n as usize));
        }
        s
    }

    /// The watch log, in order.
    /// Write the flash out, if a file was named for it.
    pub fn save_flash(&self) {
        let Some(path) = &self.flash_file else { return };
        let p = self.prom.lock().unwrap();
        match std::fs::write(path, &p[..]) {
            Ok(()) => eprintln!("ip32: flash saved to {}", path.display()),
            Err(e) => eprintln!("ip32: flash {}: {e}", path.display()),
        }
    }

    /// How many bytes have been written into the flash so far.
    pub fn flash_writes(&self) -> usize {
        self.prom_writes.lock().unwrap().len()
    }

    /// Everything written into the flash, in order.
    pub fn prom_writes(&self) -> Vec<(u32, u8)> {
        self.prom_writes.lock().unwrap().clone()
    }

    pub fn watched(&self) -> Vec<(u32, bool, u64, u32, u64, u8)> {
        self.watch_log.lock().unwrap().clone()
    }

    /// Writes landing where the `firmware` section would be loaded.
    pub fn firmware_writes(&self) -> u64 {
        *self.fw_writes.lock().unwrap()
    }

    fn in_prom(&self, addr: u32) -> bool {
        addr >= PROM_BASE && addr < PROM_BASE + PROM_SIZE
    }

    /// Reads of PCI memory: the device that claims the address, or all ones
    /// from the floating bus.
    /// PCI is little-endian and this CPU is not, so the bridge swaps byte
    /// lanes: an 8-bit access at CPU offset `A` reaches device byte `A ^ 3`,
    /// and a 16-bit one `A ^ 2`.
    ///
    /// This is not cosmetic. Without it the AIC-7880's initialisation reads
    /// as writes to registers 0x84, 0x91, 0x93 and 0xbc, which are nothing in
    /// particular. With it they are HCNTRL (chip reset, then pause), CLRINT
    /// (clear every interrupt), SCBPTR walking 0,1,2,... and the last byte of
    /// each SCB being cleared — which is exactly how that chip is brought up.
    fn lane_swizzle(off: u32, width: usize) -> u32 {
        match width {
            1 => off ^ 3,
            2 => off ^ 2,
            _ => off,
        }
    }

    fn pci_addr_of(addr: u32) -> u32 {
        if addr >= PCI_HI_MEM_BASE {
            return addr;
        }
        PCI_MEM_PCI_BASE.wrapping_add(addr.wrapping_sub(PCI_MEM_BASE))
    }

    fn pci_mem_read_w(&self, addr: u32, width: usize) -> u32 {
        match self.macepci.mem_target(Self::pci_addr_of(addr)) {
            Some((f, off)) => f.mem_read(Self::lane_swizzle(off, width), width,
                                         (self.pc.get(), self.pc.get_ra())),
            None => 0xffff_ffff,
        }
    }

    fn pci_mem_read(&self, addr: u32) -> u32 {
        self.pci_mem_read_w(addr, 4)
    }

    fn pci_mem_write_w(&self, addr: u32, val: u32, width: usize) {
        if let Some((f, off)) = self.macepci.mem_target(Self::pci_addr_of(addr)) {
            f.mem_write(Self::lane_swizzle(off, width), val, width,
                        (self.pc.get(), self.pc.get_ra()));
        }
    }

    fn pci_mem_write(&self, addr: u32, val: u32) {
        self.pci_mem_write_w(addr, val, 4)
    }

    fn in_pci_mem(&self, addr: u32) -> bool {
        (addr >= PCI_MEM_BASE && addr < PCI_MEM_BASE.wrapping_add(PCI_MEM_SIZE))
            || addr >= PCI_HI_MEM_BASE
    }

    fn in_ps2(&self, addr: u32) -> bool {
        addr >= MACE_PS2 && addr < MACE_PS2 + MACE_PS2_SIZE
    }

    fn in_vice(&self, addr: u32) -> bool {
        addr >= VICE_BASE && addr < VICE_BASE + VICE_SIZE
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

    fn in_mec(&self, addr: u32) -> bool {
        (MACE_MEC..MACE_MEC + MACE_MEC_SIZE).contains(&addr)
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

    /// Map an address through the low-memory alias, if it falls in it. The
    /// alias covers the first 256 MB only, as on an O2: above that, CRIME and
    /// MACE live at 0x14000000 and 0x1f000000, and memory beyond 256 MB is
    /// reached at its own address above `RAM_BASE`.
    fn resolve(&self, addr: u32) -> u32 {
        if addr < LOW_ALIAS_SIZE {
            RAM_BASE.wrapping_add(addr)
        } else {
            addr
        }
    }

    fn read_bytes(&self, addr: u32, n: usize) -> Option<u64> {
        let addr = self.resolve(addr);
        {
            let r = self.ram.lock().unwrap();
            match r.decode(addr) {
                // A bank control matches but nothing is fitted: what is left
                // on the memory bus, which POST has just cleared to zero.
                Decode::Empty => return Some(0),
                Decode::Ram(i) => {
                    let mut v = 0u64;
                    for k in 0..n {
                        v = (v << 8) | r[i + k] as u64;
                    }
                    drop(r);
                    self.watch_hit(addr, false, v, n);
                    return Some(v);
                }
                Decode::None => {}
            }
        }
        if self.in_prom(addr) {
            let p = self.prom.lock().unwrap();
            let i = (addr - PROM_BASE) as usize;
            if i + n > p.len() {
                return None;
            }
            let mut v = 0u64;
            for k in 0..n {
                v = (v << 8) | p[i + k] as u64;
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
        self.watch_hit(addr, true, val, n);
        {
            let mut r = self.ram.lock().unwrap();
            match r.decode(addr) {
                // Writes into an empty bank are swallowed by the memory controller.
                Decode::Empty => return true,
                Decode::Ram(i) => {
                    r.write(i, &val.to_be_bytes()[8 - n..]);
                    return true;
                }
                Decode::None => {}
            }
        }
        if self.in_prom(addr) {
            let mut p = self.prom.lock().unwrap();
            let i = (addr - PROM_BASE) as usize;
            if i + n > p.len() {
                return false;
            }
            {
                let mut log = self.prom_writes.lock().unwrap();
                for k in 0..n {
                    if log.len() < 512 {
                        log.push((addr + k as u32, (val >> (8 * (n - 1 - k))) as u8));
                    }
                }
            }
            for k in 0..n {
                p[i + k] = (val >> (8 * (n - 1 - k))) as u8;
            }
            return true;
        }
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

impl Ip32Bus {
    fn read8_raw(&self, addr: u32) -> BusRead8 {
        if self.in_vice(addr) {
            return self.vice.read8(addr);
        }
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
        if self.in_pci_mem(addr) {
            return BusRead8::ok(self.pci_mem_read_w(addr, 1) as u8);
        }
        if self.in_ps2(addr) {
            let v = self.ps2.read(addr);
            self.ps2_touched();
            return BusRead8::ok(v as u8);
        }
        if self.in_rtc(addr) {
            return self.rtc.read8(addr);
        }
        if self.in_sdma(addr) {
            let v = self.sdma_read(addr);
            return BusRead8::ok(v as u8);
        }
        if self.in_mace(addr) {
            self.before_mace_read(addr);
            return self.mace.read8(addr);
        }
        bus_read!(self, addr, 1, BusRead8::ok, |v: u64| v as u8)
    }

    fn read16_raw(&self, addr: u32) -> BusRead16 {
        if self.in_vice(addr) {
            return self.vice.read16(addr);
        }
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
        if self.in_pci_mem(addr) {
            return BusRead16::ok(self.pci_mem_read_w(addr, 2) as u16);
        }
        if self.in_ps2(addr) {
            let v = self.ps2.read(addr);
            self.ps2_touched();
            return BusRead16::ok(v as u16);
        }
        if self.in_rtc(addr) {
            return self.rtc.read16(addr);
        }
        if self.in_sdma(addr) {
            let v = self.sdma_read(addr);
            return BusRead16::ok(v as u16);
        }
        if self.in_mace(addr) {
            self.before_mace_read(addr);
            return self.mace.read16(addr);
        }
        bus_read!(self, addr, 2, BusRead16::ok, |v: u64| v as u16)
    }

    fn read32_raw(&self, addr: u32) -> BusRead32 {
        if self.in_mec(addr) {
            // The registers are 64-bit; a 32-bit access is the half it names.
            let v = self.mec.read64((addr - MACE_MEC) & !7);
            return BusRead32::ok(if addr & 4 == 0 { (v >> 32) as u32 } else { v as u32 });
        }
        if self.in_vice(addr) {
            return self.vice.read32(addr);
        }
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
        if self.in_pci_mem(addr) {
            return BusRead32::ok(self.pci_mem_read(addr) as u32);
        }
        if self.in_ps2(addr) {
            let v = self.ps2.read(addr);
            self.ps2_touched();
            return BusRead32::ok(v as u32);
        }
        if self.in_rtc(addr) {
            return self.rtc.read32(addr);
        }
        if self.in_sdma(addr) {
            // The registers are 64-bit; a 32-bit access is the half it names.
            let v = self.sdma_read(addr);
            return BusRead32::ok(if addr & 4 == 0 { (v >> 32) as u32 } else { v as u32 });
        }
        if self.in_mace(addr) {
            self.before_mace_read(addr);
            return self.mace.read32(addr);
        }
        bus_read!(self, addr, 4, BusRead32::ok, |v: u64| v as u32)
    }

    fn read64_raw(&self, addr: u32) -> BusRead64 {
        if self.in_vice(addr) {
            return self.vice.read64(addr);
        }
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
        if self.in_mec(addr) {
            return BusRead64::ok(self.mec.read64(addr - MACE_MEC));
        }
        if let Some(c) = self.com_for(addr) {
            return c.read64(addr);
        }
        if self.in_pci_mem(addr) {
            // A 64-bit access to a 32-bit bus is two cycles.
            let hi = self.pci_mem_read(addr) as u64;
            let lo = self.pci_mem_read(addr.wrapping_add(4)) as u64;
            return BusRead64::ok((hi << 32) | lo);
        }
        if self.in_ps2(addr) {
            let v = self.ps2.read(addr);
            self.ps2_touched();
            return BusRead64::ok(v);
        }
        if self.in_rtc(addr) {
            return self.rtc.read64(addr);
        }
        if self.in_sdma(addr) {
            let v = self.sdma_read(addr);
            return BusRead64::ok(v);
        }
        if self.in_mace(addr) {
            self.before_mace_read(addr);
            return self.mace.read64(addr);
        }
        let r = bus_read!(self, addr, 8, BusRead64::ok, |v: u64| v);
        self.note_sizemem(addr, false, r.data);
        r
    }

    fn write8_raw(&self, addr: u32, val: u8) -> u32 {
        if self.in_vice(addr) {
            return self.vice.write8(addr, val);
        }
        if self.in_gbe(addr) {
            return self.gbe.write32(addr & !3, val as u32);
        }
        if self.in_crime_re(addr) {
            return self.crime_re.write32(addr & !3, val as u32);
        }
        if self.in_crime(addr) {
            let r = self.crime.write32(addr & !3, val as u32);
            self.crime_wrote(addr);
            return r;
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
            let r = self.mace.write8(addr, val);
            self.mace_wrote(addr);
            return r;
        }
        if let Some(c) = self.com_for(addr) {
            return c.write8(addr, val);
        }
        if self.in_pci_mem(addr) {
            self.pci_mem_write_w(addr, val as u32, 1);
            return BUS_OK;
        }
        if self.in_ps2(addr) {
            self.ps2.write(addr, val as u64);
            self.ps2_touched();
            return BUS_OK;
        }
        if self.in_rtc(addr) {
            return self.rtc.write8(addr, val);
        }
        if self.in_sdma(addr) {
            // A narrow store is the low end of the register.
            return self.sdma_write(addr, val as u64);
        }
        if self.in_mace(addr) {
            let r = self.mace.write8(addr, val);
            self.mace_wrote(addr);
            return r;
        }
        if self.write_bytes(addr, 1, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write16_raw(&self, addr: u32, val: u16) -> u32 {
        if self.in_vice(addr) {
            return self.vice.write16(addr, val);
        }
        if self.in_gbe(addr) {
            return self.gbe.write32(addr & !3, val as u32);
        }
        if self.in_crime_re(addr) {
            return self.crime_re.write32(addr & !3, val as u32);
        }
        if self.in_crime(addr) {
            let r = self.crime.write32(addr & !3, val as u32);
            self.crime_wrote(addr);
            return r;
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
        if self.in_pci_mem(addr) {
            self.pci_mem_write_w(addr, val as u32, 2);
            return BUS_OK;
        }
        if self.in_ps2(addr) {
            self.ps2.write(addr, val as u64);
            self.ps2_touched();
            return BUS_OK;
        }
        if self.in_rtc(addr) {
            return self.rtc.write16(addr, val);
        }
        if self.in_sdma(addr) {
            // A narrow store is the low end of the register.
            return self.sdma_write(addr, val as u64);
        }
        if self.in_mace(addr) {
            let r = self.mace.write16(addr, val);
            self.mace_wrote(addr);
            return r;
        }
        if self.write_bytes(addr, 2, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write32_raw(&self, addr: u32, val: u32) -> u32 {
        if self.in_mec(addr) {
            // MACE takes 64-bit stores; a 32-bit one is the low word.
            self.mec.write64((addr - MACE_MEC) & !7, val as u64);
            return BUS_OK;
        }
        if self.in_vice(addr) {
            return self.vice.write32(addr, val);
        }
        if self.in_gbe(addr) {
            return self.gbe.write32(addr, val);
        }
        if self.in_crime_re(addr) {
            return self.crime_re.write32(addr, val);
        }
        if self.in_crime(addr) {
            let r = self.crime.write32(addr, val);
            self.crime_wrote(addr);
            return r;
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
            let r = self.mace.write32(addr, val);
            self.mace_wrote(addr);
            return r;
        }
        if let Some(c) = self.com_for(addr) {
            return c.write32(addr, val);
        }
        if self.in_pci_mem(addr) {
            self.pci_mem_write_w(addr, val as u32, 4);
            return BUS_OK;
        }
        if self.in_ps2(addr) {
            self.ps2.write(addr, val as u64);
            self.ps2_touched();
            return BUS_OK;
        }
        if self.in_rtc(addr) {
            return self.rtc.write32(addr, val);
        }
        if self.in_sdma(addr) {
            // A narrow store is the low end of the register.
            return self.sdma_write(addr, val as u64);
        }
        if self.in_mace(addr) {
            let r = self.mace.write32(addr, val);
            self.mace_wrote(addr);
            return r;
        }
        if self.write_bytes(addr, 4, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write64_raw(&self, addr: u32, val: u64) -> u32 {
        if self.in_mec(addr) {
            self.mec.write64(addr - MACE_MEC, val);
            return BUS_OK;
        }
        if self.in_vice(addr) {
            return self.vice.write64(addr, val);
        }
        if self.in_gbe(addr) {
            return self.gbe.write64(addr, val);
        }
        if self.in_crime_re(addr) {
            return self.crime_re.write64(addr, val);
        }
        if self.in_crime(addr) {
            let r = self.crime.write64(addr, val);
            self.crime_wrote(addr);
            return r;
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
            let r = self.mace.write64(addr, val);
            self.mace_wrote(addr);
            return r;
        }
        if let Some(c) = self.com_for(addr) {
            return c.write64(addr, val);
        }
        if self.in_pci_mem(addr) {
            self.pci_mem_write_w(addr, val as u32, 4);
            return BUS_OK;
        }
        if self.in_ps2(addr) {
            self.ps2.write(addr, val as u64);
            self.ps2_touched();
            return BUS_OK;
        }
        if self.in_rtc(addr) {
            return self.rtc.write64(addr, val);
        }
        if self.in_sdma(addr) {
            return self.sdma_write(addr, val);
        }
        if self.in_mace(addr) {
            let r = self.mace.write64(addr, val);
            self.mace_wrote(addr);
            return r;
        }
        self.note_sizemem(addr, true, val);
        if self.write_bytes(addr, 8, val) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }
}

/// Every access goes to the `_raw` decode above. Around it, a machine with
/// an interrupt sink (`set_irq_sink`) gets its lines recomputed: before a
/// read of CRIME, so INTSTAT is live, and after any write that is not RAM,
/// so a handler's acknowledgement drops IP2 at once instead of leaving it up
/// until the next service tick (OpenBSD: "spurious crime interrupt").
impl BusDevice for Ip32Bus {
    /// Memory pages have generations jitv2 can compile against; the PROM and
    /// devices have none, so nothing there is compiled.
    #[cfg(feature = "jitv2")]
    fn gen_ptr(&self, addr: u32) -> *const std::sync::atomic::AtomicU64 {
        let addr = self.resolve(addr);
        let r = self.ram.lock().unwrap();
        match r.decode(addr) {
            Decode::Ram(i) => r.gen_ptr(i),
            _ => std::ptr::null(),
        }
    }

    fn read8(&self, addr: u32) -> BusRead8 {
        self.before_read(addr);
        self.read8_raw(addr)
    }
    fn read16(&self, addr: u32) -> BusRead16 {
        self.before_read(addr);
        self.read16_raw(addr)
    }
    fn read32(&self, addr: u32) -> BusRead32 {
        self.before_read(addr);
        self.read32_raw(addr)
    }
    fn read64(&self, addr: u32) -> BusRead64 {
        self.before_read(addr);
        self.read64_raw(addr)
    }
    fn write8(&self, addr: u32, val: u8) -> u32 {
        let r = self.write8_raw(addr, val);
        self.after_write(addr);
        r
    }
    fn write16(&self, addr: u32, val: u16) -> u32 {
        let r = self.write16_raw(addr, val);
        self.after_write(addr);
        r
    }
    fn write32(&self, addr: u32, val: u32) -> u32 {
        let r = self.write32_raw(addr, val);
        self.after_write(addr);
        r
    }
    fn write64(&self, addr: u32, val: u64) -> u32 {
        let r = self.write64_raw(addr, val);
        self.after_write(addr);
        r
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
pub struct PcTap {
    pc: AtomicU32,
    /// `ra` as well, because the PROM reaches most registers through one-line
    /// `ld`/`sd` accessors. Attributing a hot register to the accessor names
    /// the same three addresses every time and says nothing; the return
    /// address names the code that actually wanted it.
    ra: AtomicU32,
}

impl PcTap {
    pub fn set(&self, pc: u32) {
        self.pc.store(pc, Ordering::Relaxed);
    }
    pub fn set_ra(&self, ra: u32) {
        self.ra.store(ra, Ordering::Relaxed);
    }
    pub fn get(&self) -> u32 {
        self.pc.load(Ordering::Relaxed)
    }
    pub fn get_ra(&self) -> u32 {
        self.ra.load(Ordering::Relaxed)
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

    /// Send `b` the PROM's way: clock off, TX, then TxEN|CLKEN.
    fn ps2_send(k: &MacePs2, port: u32, b: u8) {
        let base = MACE_PS2 + 0x20 * port;
        k.write(base + 0x10, 0);
        k.write(base, b as u64);
        k.write(base + 0x10, ps2_ctl::TX_ENABLE | ps2_ctl::CLKEN);
    }

    fn ps2_recv(k: &MacePs2, port: u32) -> Option<u8> {
        let base = MACE_PS2 + 0x20 * port;
        (k.read(base + 0x18) & ps2_bit::RX_FULL != 0).then(|| k.read(base + 8) as u8)
    }

    #[test]
    fn the_keyboard_answers_the_proms_probe_and_identifies_itself() {
        let k = MacePs2::with_devices(false);
        ps2_send(&k, 0, 0xf5);
        assert_eq!(ps2_recv(&k, 0), Some(0xfa));
        assert_eq!(ps2_recv(&k, 0), None, "one reply");
        assert_eq!(k.read(MACE_PS2 + 8) as u8, 0xfa, "the latch keeps the last byte");
        ps2_send(&k, 0, 0xf2);
        let id: Vec<_> = std::iter::from_fn(|| ps2_recv(&k, 0)).collect();
        assert_eq!(id, [0xfa, 0xab, 0x83]);
        ps2_send(&k, 0, 0xf0);
        ps2_send(&k, 0, 0x03);
        ps2_send(&k, 0, 0xf0);
        ps2_send(&k, 0, 0x00);
        let got: Vec<_> = std::iter::from_fn(|| ps2_recv(&k, 0)).collect();
        assert_eq!(got, [0xfa, 0xfa, 0xfa, 0xfa, 0x03], "set 3 selected, then read back");
    }

    #[test]
    fn nothing_arrives_while_the_clock_is_held_or_on_an_empty_port() {
        let k = MacePs2::with_devices(false);
        k.write(MACE_PS2 + 0x10, 0);
        k.write(MACE_PS2, 0xf5);
        k.write(MACE_PS2 + 0x10, ps2_ctl::TX_ENABLE);
        assert_eq!(k.read(MACE_PS2 + 0x18) & ps2_bit::RX_FULL, 0, "clock held: the reply waits");
        k.write(MACE_PS2 + 0x10, ps2_ctl::TX_ENABLE | ps2_ctl::CLKEN);
        assert_eq!(ps2_recv(&k, 0), Some(0xfa));
        // No mouse: the byte drains and nothing ever comes back.
        ps2_send(&k, 1, 0xf6);
        assert_ne!(k.read(MACE_PS2 + 0x38) & ps2_bit::TX_EMPTY, 0);
        assert_eq!(ps2_recv(&k, 1), None);
    }

    /// OpenBSD's pms probes for a touchpad with E8 00 four times and E9, and
    /// waits for all three status bytes after the ACK.
    #[test]
    fn the_mouse_answers_a_status_request_in_full() {
        let k = MacePs2::with_devices(true);
        for _ in 0..4 {
            ps2_send(&k, 1, 0xe8);
            ps2_send(&k, 1, 0x00);
        }
        let acks: Vec<_> = std::iter::from_fn(|| ps2_recv(&k, 1)).collect();
        assert_eq!(acks, [0xfa; 8]);
        ps2_send(&k, 1, 0xe9);
        let st: Vec<_> = std::iter::from_fn(|| ps2_recv(&k, 1)).collect();
        assert_eq!(st, [0xfa, 0x00, 0x00, 100], "ACK, status, resolution 0, rate 100");
    }

    #[test]
    fn a_waiting_byte_raises_the_keyboard_interrupt_only_when_enabled() {
        let k = MacePs2::with_devices(false);
        ps2_send(&k, 0, 0xee);
        assert_eq!(k.isa_bits(), 1 << 10, "poll flag only");
        k.write(MACE_PS2 + 0x10, ps2_ctl::TX_ENABLE | ps2_ctl::CLKEN | ps2_ctl::RX_INT);
        assert_eq!(k.isa_bits(), (1 << 9) | (1 << 10));
        assert_eq!(ps2_recv(&k, 0), Some(0xee));
        assert_eq!(k.isa_bits(), 0);
    }

    #[test]
    fn crime_is_a_64_bit_register_file() {
        let c = Crime::new(std::sync::Arc::new(MaceUst::new()));
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
        let c = Crime::new(std::sync::Arc::new(MaceUst::new()));
        assert_ne!(c.read64(CRIME_BASE + crime_reg::REVISION).data, 0);
    }

    /// POST times its waits against this counter. A constant would spin forever.
    /// The PROM zeroes this counter and waits for it to reach `66 * n`, so it
    /// has to advance with *time* and not with reads: a per-read counter makes
    /// every delay cost a fixed number of polls no matter how long the wait
    /// was supposed to be, and makes the two clocks disagree.
    #[test]
    fn the_time_counter_tracks_the_clock_and_can_be_reset() {
        let ust = std::sync::Arc::new(MaceUst::new());
        let c = Crime::new(ust.clone());

        ust.advance_to(1_000 * UST_TICKS_DEN as u64);
        let a = c.read64(CRIME_BASE + crime_reg::TIME).data;
        assert!(a >= 1_000 * CRIME_TICKS_PER_US, "counter must follow the clock, got {a}");
        assert_eq!(c.read64(CRIME_BASE + crime_reg::TIME).data, a,
                   "and must not advance merely because it was read");

        // Zero it, wait 100us, and it must read about 100us worth of ticks.
        c.write64(CRIME_BASE + crime_reg::TIME, 0);
        assert_eq!(c.read64(CRIME_BASE + crime_reg::TIME).data, 0, "a write sets the counter");
        ust.advance_to(1_100 * UST_TICKS_DEN as u64);
        let d = c.read64(CRIME_BASE + crime_reg::TIME).data;
        assert_eq!(d, 100 * CRIME_TICKS_PER_US,
                   "100us must be 100 * {CRIME_TICKS_PER_US} ticks, got {d}");
    }

    #[test]
    fn every_touched_register_is_recorded_once_per_direction() {
        let c = Crime::new(std::sync::Arc::new(MaceUst::new()));
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
    /// The flash is readable and, deliberately, writable.
    ///
    /// This asserted the opposite until an install failed on it: an O2 has no
    /// separate NVRAM chip and keeps its environment variables in the flash,
    /// so `2) Install System Software` bus-errors saving its settings if the
    /// region rejects writes. The copy in memory is what changes; the image
    /// the caller handed us is theirs.
    fn the_flash_is_readable_and_writable() {
        let mut image = vec![0u8; PROM_SIZE as usize];
        image[..4].copy_from_slice(&0x1000_0011u32.to_be_bytes());
        let bus = Ip32Bus::new(1 << 20, image);
        assert_eq!(bus.read32(PROM_BASE).data, 0x1000_0011, "reset vector must read back");

        // The environment section, which is what firmware actually writes.
        let env = PROM_BASE + 0x4000;
        assert_eq!(bus.write32(env, 0xcafe_f00d), BUS_OK, "the flash must accept writes");
        assert_eq!(bus.read32(env).data, 0xcafe_f00d, "and read back what was written");
        assert!(bus.unmapped().is_empty(), "a flash write is not an unmapped access");
        assert_eq!(bus.prom_writes().len(), 4, "and is recorded, byte by byte");
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

    // ── Serial DMA ──────────────────────────────────────────────────────────

    /// Where the tests put the ring block: low memory, 32 KB aligned.
    const SDMA_RING: u32 = 0x0010_0000;
    const TX_CTRL: u32 = 0;
    const TX_RPTR: u32 = 1;
    const TX_WPTR: u32 = 2;
    const TX_DEPTH: u32 = 3;
    const RX_CTRL: u32 = 4;
    const RX_RPTR: u32 = 5;
    const RX_WPTR: u32 = 6;
    const RX_DEPTH: u32 = 7;

    fn sdma_bus() -> Ip32Bus {
        let bus = Ip32Bus::new(16 << 20, vec![0u8; PROM_SIZE as usize]);
        bus.write64(MACE_ISA_RING_BASE, SDMA_RING as u64);
        bus
    }

    fn sdma_reg(p: u32, reg: u32) -> u32 {
        MACE_SDMA + p * 0x4000 + reg * 8
    }

    fn sdma_get(bus: &Ip32Bus, p: u32, reg: u32) -> u64 {
        bus.read64(sdma_reg(p, reg)).data
    }

    fn sdma_set(bus: &Ip32Bus, p: u32, reg: u32, v: u64) {
        assert_eq!(bus.write64(sdma_reg(p, reg), v), BUS_OK);
    }

    fn tx_ring(p: u32) -> u32 {
        SDMA_RING + 0x4000 + p * 0x2000
    }

    fn rx_ring(p: u32) -> u32 {
        SDMA_RING + 0x5000 + p * 0x2000
    }

    /// Fill one transmit block as IRIX does: characters from pair 0 on with
    /// control 0x40, every unused slot zero.
    fn tx_block(bus: &Ip32Bus, p: u32, ptr: u32, text: &[u8]) {
        assert!(text.len() <= 16);
        let mut b = [0u8; 32];
        for (j, &c) in text.iter().enumerate() {
            b[8 * (j / 4) + j % 4] = 0x40;
            b[8 * (j / 4) + 4 + j % 4] = c;
        }
        tx_raw(bus, p, ptr, &b);
    }

    fn tx_raw(bus: &Ip32Bus, p: u32, ptr: u32, b: &[u8; 32]) {
        for k in 0..4 {
            let w = u64::from_be_bytes(b[8 * k..8 * k + 8].try_into().unwrap());
            bus.write64(tx_ring(p) + ptr + 8 * k as u32, w);
        }
    }

    fn isa_status(bus: &Ip32Bus) -> u64 {
        bus.read64(MACE_ISA_INT_STATUS).data
    }

    fn line4(bus: &Ip32Bus) -> bool {
        bus.crime.intstat() & (1 << IP32_ISA_CRIME_INT) != 0
    }

    #[test]
    fn serial_dma_registers_decode_and_start_in_reset() {
        let bus = sdma_bus();
        for p in 0..2 {
            assert_eq!(sdma_get(&bus, p, TX_CTRL), 0x400, "port {p} TX control resets held in reset");
            assert_eq!(sdma_get(&bus, p, RX_CTRL), 0x400, "port {p} RX control too");
            for reg in [TX_RPTR, TX_WPTR, TX_DEPTH, RX_RPTR, RX_WPTR, RX_DEPTH] {
                assert_eq!(sdma_get(&bus, p, reg), 0);
            }
        }
        // Port 1 is its own page, and keeps only the defined bits.
        sdma_set(&bus, 1, TX_CTRL, 0xffff_ffff);
        assert_eq!(sdma_get(&bus, 1, TX_CTRL), 0x6e0);
        assert_eq!(sdma_get(&bus, 0, TX_CTRL), 0x400, "port 0 untouched");
        // Gaps in a page alias to its eight registers.
        assert_eq!(bus.read64(MACE_SDMA + 0x40).data, 0x400);
        assert_eq!(bus.read64(MACE_SDMA + 0x4000 + 0x40).data, 0x6e0);
        // Every width answers.
        assert_eq!(bus.read32(sdma_reg(1, TX_CTRL) + 4).data, 0x6e0, "+4 is the low word");
        assert_eq!(bus.read32(sdma_reg(1, TX_CTRL)).data, 0, "+0 the high word");
        assert_eq!(bus.read16(sdma_reg(1, TX_CTRL) + 6).data, 0x6e0);
        assert_eq!(bus.read8(sdma_reg(1, TX_CTRL) + 7).data, 0xe0);
        assert_eq!(bus.write32(sdma_reg(1, TX_CTRL) + 4, 0x400), BUS_OK);
        assert_eq!(sdma_get(&bus, 1, TX_CTRL), 0x400);
        assert!(bus.unmapped().is_empty());
    }

    /// Blocks are sent in order, a partly filled one up to its first unused
    /// slot, and the engine is caught up before the store returns.
    #[test]
    fn serial_dma_transmits_full_and_partial_blocks() {
        let bus = sdma_bus();
        tx_block(&bus, 0, 0, b"0123456789abcdef");
        tx_block(&bus, 0, 32, b"xyz");
        sdma_set(&bus, 0, TX_CTRL, 0x200);
        sdma_set(&bus, 0, TX_WPTR, 64);
        assert_eq!(bus.com0.output(), "0123456789abcdefxyz");
        assert_eq!(sdma_get(&bus, 0, TX_RPTR), 64);
        assert_eq!(sdma_get(&bus, 0, TX_DEPTH), 0);
        assert_eq!(bus.com1.output(), "", "port 1 is a different UART");
    }

    #[test]
    fn a_disabled_transmit_channel_is_frozen_not_lost() {
        let bus = sdma_bus();
        tx_block(&bus, 0, 0, b"later");
        sdma_set(&bus, 0, TX_CTRL, 0x000);
        sdma_set(&bus, 0, TX_WPTR, 32);
        assert_eq!(bus.com0.output(), "");
        assert_eq!(sdma_get(&bus, 0, TX_DEPTH), 32, "IRIX reads the depth as bytes");
        bus.service();
        assert_eq!(bus.com0.output(), "", "the service tick does not run a frozen channel");
        sdma_set(&bus, 0, TX_CTRL, 0x200);
        assert_eq!(bus.com0.output(), "later");
        assert_eq!(sdma_get(&bus, 0, TX_DEPTH), 0);
    }

    /// Batches of three blocks, so some straddle the end of the ring and the
    /// write pointer goes from 0xfe0 (or near it) back past zero.
    #[test]
    fn the_transmit_ring_wraps() {
        let bus = sdma_bus();
        sdma_set(&bus, 0, TX_CTRL, 0x280);
        let mut sw = 0u32;
        let mut want = String::new();
        for batch in 0..60 {
            for k in 0..3 {
                let t = format!("<{batch}.{k}>");
                tx_block(&bus, 0, sw, t.as_bytes());
                want += &t;
                sw = (sw + 32) % 4096;
            }
            sdma_set(&bus, 0, TX_WPTR, sw as u64);
            assert_eq!(sdma_get(&bus, 0, TX_DEPTH), 0);
        }
        assert_eq!(bus.com0.output(), want);
        assert_eq!(sdma_get(&bus, 0, TX_RPTR), sw as u64);
    }

    #[test]
    fn a_transmit_pair_can_set_mcr_and_ask_for_a_pair_request() {
        let bus = sdma_bus();
        bus.write64(MACE_ISA_INT_MASK, sdma::tx_preq(0));
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&[0x80, 0x60, 0, 0, 0x0b, b'Q', 0, 0]);
        tx_raw(&bus, 0, 0, &b);
        sdma_set(&bus, 0, TX_CTRL, 0x200);
        sdma_set(&bus, 0, TX_WPTR, 32);
        assert_eq!(bus.read8(MACE_COM0 + 0x407).data, 0x0b, "the MCR pair landed in MCR");
        assert_eq!(bus.com0.output(), "Q");
        assert_ne!(isa_status(&bus) & sdma::tx_preq(0), 0, "bit 22 latched");
        assert!(line4(&bus));
        bus.write64(MACE_ISA_INT_STATUS, 0);
        assert_eq!(isa_status(&bus) & sdma::tx_preq(0), 0, "writing it 0 clears it");
        assert!(!line4(&bus));
    }

    /// The IRIX dispatcher's contract: it acknowledges by masking, and the
    /// line has to drop when it does.
    #[test]
    fn the_transmit_threshold_is_a_level_and_the_line_obeys_the_mask() {
        let bus = sdma_bus();
        bus.write64(MACE_ISA_INT_MASK, sdma::tx_thir(0));
        sdma_set(&bus, 0, TX_CTRL, 0x280);
        bus.service();
        assert_eq!(isa_status(&bus) & sdma::group(0), sdma::tx_thir(0));
        assert!(line4(&bus), "empty ring, EMPTY condition, unmasked");
        bus.write64(MACE_ISA_INT_MASK, 0);
        assert_eq!(isa_status(&bus) & sdma::tx_thir(0), 0, "status reads through the mask");
        assert!(!line4(&bus), "masked: CRIME input 4 drops at once");
        bus.service();
        assert!(!line4(&bus), "and stays down");
        bus.write64(MACE_ISA_INT_MASK, sdma::tx_thir(0));
        assert!(line4(&bus), "unmasked again: up again");
        sdma_set(&bus, 0, TX_CTRL, 0x200);
        assert_eq!(isa_status(&bus) & sdma::tx_thir(0), 0, "no condition, no bit");
        assert!(!line4(&bus));
    }

    #[test]
    fn the_threshold_conditions_follow_the_datasheet_table() {
        let blocks = [0u32, 31, 32, 64, 96, 127];
        // Input channel, per code 1..7, for each depth above.
        let input: [[bool; 6]; 7] = [
            [false, false, true, true, true, true],
            [false, false, false, true, true, true],
            [false, false, false, false, true, true],
            [true, false, false, false, false, false],
            [false, true, true, true, true, true],
            [false, false, false, false, false, true],
            [true, true, true, true, true, false],
        ];
        for code in 0..8u64 {
            for (i, &n) in blocks.iter().enumerate() {
                let ch = SdmaChan { ctrl: code << 5, rptr: 0, wptr: n * 32, blocks: 0, bytes: 0 };
                let (inp, out) = if code == 0 {
                    (false, false)
                } else {
                    let v = input[code as usize - 1][i];
                    // The fill levels invert for an output channel.
                    (v, if code <= 3 { !v } else { v })
                };
                assert_eq!(ch.threshold(false), inp, "input, code {code}, {n} blocks");
                assert_eq!(ch.threshold(true), out, "output, code {code}, {n} blocks");
                let held = SdmaChan { ctrl: ch.ctrl | sdma::CTRL_RESET, ..ch };
                assert!(!held.threshold(false) && !held.threshold(true), "never in reset");
            }
        }
    }

    #[test]
    fn received_characters_are_packed_into_whole_blocks() {
        let bus = sdma_bus();
        bus.write64(MACE_ISA_INT_MASK, sdma::rx_thir(0));
        sdma_set(&bus, 0, RX_CTRL, 0x2a0);
        assert!(!line4(&bus), "an empty ring is not 'not empty'");
        bus.com0.feed(b"abcde");
        bus.service();
        let words: Vec<u64> = (0..4).map(|k| bus.read64(rx_ring(0) + 8 * k).data).collect();
        assert_eq!(words, [0x8080_8080_6162_6364, 0x8000_0000_65ff_ffff, 0xffff_ffff, 0xffff_ffff]);
        assert_eq!(sdma_get(&bus, 0, RX_WPTR), 32);
        assert_eq!(sdma_get(&bus, 0, RX_DEPTH), 32);
        assert_eq!(bus.com0.pending_input(), 0);
        assert_ne!(isa_status(&bus) & sdma::rx_thir(0), 0);
        assert!(line4(&bus));
        sdma_set(&bus, 0, RX_RPTR, 32);
        assert_eq!(isa_status(&bus) & sdma::rx_thir(0), 0, "drained: the level drops");
        assert!(!line4(&bus));

        // Exactly one pair, then exactly a whole block.
        bus.com0.feed(b"wxyz");
        bus.service();
        let w: Vec<u64> = (0..4).map(|k| bus.read64(rx_ring(0) + 32 + 8 * k).data).collect();
        assert_eq!(w, [0x8080_8080_7778_797a, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff]);
        bus.com0.feed(b"0123456789ABCDEF");
        bus.service();
        let w: Vec<u64> = (0..4).map(|k| bus.read64(rx_ring(0) + 64 + 8 * k).data).collect();
        assert_eq!(w, [0x8080_8080_3031_3233, 0x8080_8080_3435_3637,
                       0x8080_8080_3839_4142, 0x8080_8080_4344_4546]);
        assert_eq!(sdma_get(&bus, 0, RX_WPTR), 96);
    }

    /// Take one block the way IRIX's reader does: compare the pointers, copy
    /// pairs until a zero control word, zero what it consumed, trim the last
    /// pair, and step the read pointer (+32, or 0 from the last block).
    fn irix_rx_block(bus: &Ip32Bus, sw: &mut u32) -> Option<Vec<u8>> {
        let rp = sdma_get(bus, 0, RX_RPTR);
        if rp == sdma_get(bus, 0, RX_WPTR) {
            return None;
        }
        let blk = rx_ring(0) + *sw;
        let mut chars = Vec::new();
        let (mut last_ctrl, mut last_at) = (0u32, 0usize);
        for k in 0..4 {
            let w = bus.read64(blk + 8 * k).data;
            let ctrl = (w >> 32) as u32;
            if ctrl == 0 {
                break;
            }
            for c in ctrl.to_be_bytes() {
                assert!(c == 0x80 || c == 0, "control byte {c:02x}");
            }
            last_at = chars.len();
            last_ctrl = ctrl;
            chars.extend_from_slice(&(w as u32).to_be_bytes());
            bus.write64(blk + 8 * k, w & 0xffff_ffff);
        }
        assert!(!chars.is_empty(), "a block was handed over with nothing in pair 0");
        let unused = last_ctrl.to_be_bytes().iter().rev().take_while(|&&c| c == 0).count();
        chars.truncate(last_at + 4 - unused);
        sdma_set(bus, 0, RX_RPTR, if *sw == 0xfe0 { 0 } else { rp + 32 });
        *sw = (*sw + 32) & 0xfe0;
        Some(chars)
    }

    #[test]
    fn a_long_paste_arrives_intact_through_an_irix_style_reader() {
        let bus = sdma_bus();
        sdma_set(&bus, 0, RX_CTRL, 0x2a0);
        let sent: Vec<u8> = (0..3000).map(|i| b'!' + (i % 90) as u8).collect();
        bus.com0.feed(&sent);
        bus.service();
        assert_eq!(sdma_get(&bus, 0, RX_DEPTH), 0xfe0, "full is one block short of the ring");
        assert_eq!(bus.com0.pending_input(), 3000 - 127 * 16, "the rest waits in the queue");
        let mut got = Vec::new();
        let mut sw = 0;
        for _ in 0..10_000 {
            match irix_rx_block(&bus, &mut sw) {
                Some(c) => got.extend(c),
                None => {
                    if got.len() == sent.len() {
                        break;
                    }
                    bus.service();
                }
            }
            assert!(sdma_get(&bus, 0, RX_DEPTH) <= 0xfe0);
        }
        assert_eq!(got.len(), sent.len());
        assert!(got == sent, "characters out of order or damaged");
        assert_eq!(bus.com0.pending_input(), 0);
    }

    /// Without receive DMA running, typed characters stay where the PROM, the
    /// BSDs and IRIX's own polled paths read them.
    #[test]
    fn without_receive_dma_input_stays_on_the_pio_path() {
        for ctrl in [None, Some(0x400), Some(0x000)] {
            let bus = sdma_bus();
            if let Some(c) = ctrl {
                sdma_set(&bus, 0, RX_CTRL, c);
            }
            bus.com0.feed(b"hi");
            bus.service();
            let lsr = MACE_COM0 + 0x507;
            assert_ne!(bus.read8(lsr).data & com_reg::LSR_DR, 0, "{ctrl:?}: data ready");
            assert_eq!(bus.read8(MACE_COM0 + 7).data, b'h');
            assert_eq!(bus.read8(MACE_COM0 + 7).data, b'i');
            assert_eq!(sdma_get(&bus, 0, RX_WPTR), 0, "{ctrl:?}: nothing went into the ring");
        }
    }

    /// The PROM can leave IER's transmit bit set, which holds the 16550's
    /// request high for ever. In DMA mode that request is not software's.
    #[test]
    fn the_uarts_own_request_is_held_low_in_dma_mode() {
        let bus = sdma_bus();
        bus.write64(MACE_ISA_INT_MASK, 0xffff_ffff);
        bus.write8(MACE_COM0 + 0x107, com_reg::IER_TX);
        bus.service();
        assert_eq!(isa_status(&bus) & sdma::group(0), sdma::group(0), "before: all six, as today");
        assert!(line4(&bus));
        sdma_set(&bus, 0, TX_CTRL, 0x200);
        bus.service();
        assert_eq!(isa_status(&bus) & sdma::group(0), 0, "DIR and the rest are now the DMA model's");
        assert!(!line4(&bus));
        // Both channels back in reset hands the port back.
        sdma_set(&bus, 0, TX_CTRL, 0x400);
        bus.service();
        assert_eq!(isa_status(&bus) & sdma::group(0), sdma::group(0));
    }

    /// What NetBSD and OpenBSD boot with: all six port-0 bits from the 16550,
    /// into line 4 whatever the mask.
    #[test]
    fn a_pio_port_keeps_the_unmasked_six_bit_behaviour() {
        assert_eq!(sdma::group(0), MACE_ISA_SERIAL0_MASK);
        for mask in [0xffff_0aff, 0] {
            let bus = sdma_bus();
            bus.write64(MACE_ISA_INT_MASK, mask);
            bus.write64(MACE_ISA_INT_STATUS, 0);
            bus.write8(MACE_COM0 + 0x107, com_reg::IER_RX);
            bus.com0.feed(b"x");
            bus.service();
            assert_eq!(isa_status(&bus) & 0xffff_0000, 0x03f0_0000, "mask {mask:x}");
            assert!(line4(&bus), "mask {mask:x}");
            assert_eq!(bus.read8(MACE_COM0 + 7).data, b'x');
            bus.service();
            assert_eq!(isa_status(&bus) & 0xffff_0000, 0, "mask {mask:x}: read, so quiet");
            assert!(!line4(&bus));
        }
    }

    /// IRIX's polled console write between two DMA batches: nothing lost,
    /// nothing reordered.
    #[test]
    fn a_polled_write_between_batches_keeps_its_place() {
        let bus = sdma_bus();
        sdma_set(&bus, 0, TX_CTRL, 0x280);
        tx_block(&bus, 0, 0, b"AB");
        sdma_set(&bus, 0, TX_WPTR, 32);
        let saved = sdma_get(&bus, 0, TX_CTRL);
        sdma_set(&bus, 0, TX_CTRL, saved & !0x280);
        for &c in b"cd" {
            assert_ne!(bus.read8(MACE_COM0 + 0x507).data & com_reg::LSR_THRE, 0);
            bus.write8(MACE_COM0 + 7, c);
        }
        sdma_set(&bus, 0, TX_CTRL, saved);
        tx_block(&bus, 0, 32, b"EF");
        sdma_set(&bus, 0, TX_WPTR, 64);
        assert_eq!(bus.com0.output(), "ABcdEF");
    }

    #[test]
    fn resetting_a_channel_clears_its_pointers_latches_and_level() {
        let bus = sdma_bus();
        bus.write64(MACE_ISA_INT_MASK, 0xffff_ffff);
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&[0x60, 0, 0, 0, b'!', 0, 0, 0]);
        tx_raw(&bus, 0, 0, &b);
        sdma_set(&bus, 0, TX_CTRL, 0x200);
        sdma_set(&bus, 0, TX_WPTR, 32);
        // Frozen with work queued and "not empty" asked for.
        sdma_set(&bus, 0, TX_CTRL, 0x0a0);
        sdma_set(&bus, 0, TX_WPTR, 96);
        assert_eq!(sdma_get(&bus, 0, TX_DEPTH), 64);
        assert_eq!(isa_status(&bus) & sdma::group(0), sdma::tx_thir(0) | sdma::tx_preq(0));
        sdma_set(&bus, 0, TX_CTRL, 0x4a0);
        assert_eq!(sdma_get(&bus, 0, TX_RPTR), 0);
        assert_eq!(sdma_get(&bus, 0, TX_WPTR), 0);
        assert_eq!(isa_status(&bus) & sdma::group(0), 0);
        assert!(!line4(&bus));
        assert_eq!(bus.com0.output(), "!");
    }

    /// The RTC runs, from host time, and a guest that sets it with SET moves it.
    #[test]
    fn the_rtc_runs_and_can_be_set() {
        let r = MaceRtc::new();
        let reg = |n: u32| (n << 8) | 7;
        // Host time on power-up: some year from 2026 on, in BCD.
        let yy = r.read8(reg(9)).data;
        assert!(yy >= 0x26 && yy & 0x0f < 10, "year {yy:#x}");
        // Set 1999-12-31 23:59:58 under SET, binary mode, 24-hour.
        r.write8(reg(0x0b), 0x80 | 0x04 | 0x02);
        for (n, v) in [(0u32, 58u8), (2, 59), (4, 23), (7, 31), (8, 12), (9, 99)] {
            r.write8(reg(n), v);
        }
        r.write8(reg(0x0b), 0x04 | 0x02);
        assert_eq!(r.read8(reg(9)).data, 99);
        assert_eq!(r.read8(reg(6)).data, 6, "a Friday");
        // Three seconds on, it is the year 2000.
        {
            let mut c = r.clock.lock().unwrap();
            c.1 -= std::time::Duration::from_secs(3);
        }
        assert_eq!((r.read8(reg(9)).data, r.read8(reg(8)).data, r.read8(reg(7)).data), (0, 1, 1));
        assert_eq!(r.read8(reg(0)).data, 1);
        // And BCD again on request.
        r.write8(reg(0x0b), 0x02);
        assert_eq!(r.read8(reg(8)).data, 0x01);
        // The century, in the extended bank (register A bit 4).
        r.write8(reg(0x0a), 0x10);
        assert_eq!(r.read8(reg(0x48)).data, 0x20);
        r.write8(reg(0x48), 0x19);
        r.write8(reg(0x0a), 0x00);
        assert_eq!(r.read8(reg(9)).data, 0x00, "1900: same two digits");
        r.write8(reg(0x0a), 0x10);
        assert_eq!(r.read8(reg(0x48)).data, 0x19);
        assert_eq!(days_from_civil(2000, 1, 1), 10957);
        // Software power-off: PAB in extended control register 4A.
        assert!(!r.powered_off());
        r.write8(reg(0x0a), 0x10);
        r.write8(reg(0x4a), 0x08);
        assert!(r.powered_off(), "PAB switches the machine off");
        assert_eq!(civil_from_days(10957), (2000, 1, 1));
    }

    /// The TCP console: a client gets the telnet handshake, then every byte
    /// the guest transmits, and what it types becomes the guest's input --
    /// with telnet's own commands taken out on the way.
    #[test]
    fn a_tcp_console_carries_both_directions() {
        use std::io::{Read, Write};
        let c = Com16550::new();
        let port = c.serve_tcp(0).expect("bind");
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut hs = [0u8; 12];
        client.read_exact(&mut hs).expect("handshake");
        assert_eq!(hs, crate::net::telnet::TelnetFilter::initial_handshake());
        // Typed, with a telnet command (IAC DO SGA, which it already offered) in between.
        client.write_all(&[b'l', 0xff, 0xfd, 0x03, b's', b'\r']).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while c.pending_input() < 3 && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.read8(MACE_COM0 + 7).data, b'l');
        assert_eq!(c.read8(MACE_COM0 + 7).data, b's');
        assert_eq!(c.read8(MACE_COM0 + 7).data, b'\r');
        assert_eq!(c.pending_input(), 0, "the telnet command is not input");
        // Transmitted: reaches the client (once it is registered as the client).
        while c.tcp.lock().unwrap().is_empty() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for &b in b"ok" {
            c.write8(MACE_COM0 + 7, b);
        }
        let mut got = [0u8; 2];
        client.read_exact(&mut got).expect("output");
        assert_eq!(&got, b"ok");
        assert_eq!(c.output(), "ok", "the console buffer still has it too");
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

    /// Firmware learns how much space a device wants by writing all ones to
    /// a BAR and reading back which address bits stuck. A BAR that stores
    /// whatever it is given claims four gigabytes and the bus layout
    /// collapses.
    #[test]
    fn a_bar_reports_its_size_to_a_probe() {
        let f = PciFunction::new("t", 0x9004, 0x8078, PCI_CLASS_SCSI, 1, [0x100, 0x1000, 0, 0, 0, 0]);
        assert_eq!(f.read(0), 0x8078_9004, "device and vendor id");

        f.write(4, 0xffff_ffff);
        assert_eq!(f.read(4), 0xffff_ff01, "256 bytes of I/O space");
        f.write(5, 0xffff_ffff);
        assert_eq!(f.read(5), 0xffff_f000, "4 KiB of memory space");
        // An unimplemented BAR reads back as zero however hard it is probed.
        f.write(6, 0xffff_ffff);
        assert_eq!(f.read(6), 0);

        f.write(5, 0x8000_1000);
        assert_eq!(f.bar(1), 0x8000_1000, "and then holds the address assigned to it");
    }

    /// PCI is little-endian and this CPU is not. Without the bridge's byte
    /// lane swap the AIC-7880's bring-up reads as traffic to registers 0x84,
    /// 0x91, 0x93 and 0xbc, which are nothing; with it they are HCNTRL,
    /// CLRINT, SCBPTR and the SCB array, which is exactly right.
    #[test]
    fn the_bridge_swaps_byte_lanes_for_a_little_endian_device() {
        assert_eq!(Ip32Bus::lane_swizzle(0x84, 1), 0x87, "HCNTRL");
        assert_eq!(Ip32Bus::lane_swizzle(0x91, 1), 0x92, "CLRINT");
        assert_eq!(Ip32Bus::lane_swizzle(0x93, 1), 0x90, "SCBPTR");
        assert_eq!(Ip32Bus::lane_swizzle(0xbc, 1), 0xbf, "last byte of the SCB");
        assert_eq!(Ip32Bus::lane_swizzle(0x84, 2), 0x86, "16-bit lanes swap in pairs");
        assert_eq!(Ip32Bus::lane_swizzle(0x84, 4), 0x84, "a whole word needs no swap");
    }

    /// A device only decodes memory once it has been told it may, and only
    /// within the BAR it was assigned.
    #[test]
    fn a_device_claims_only_its_own_window_and_only_when_enabled() {
        let pci = MacePci::new();
        let f = std::sync::Arc::new(PciFunction::new(
            "t", 0x9004, 0x8078, PCI_CLASS_SCSI, 1, [0x100, 0x1000, 0, 0, 0, 0]));
        pci.attach(IP32_SCSI_SLOT, f.clone());
        f.write(5, 0x8000_1000);

        assert!(pci.mem_target(0x8000_1000).is_none(),
                "memory decoding is off until the command register says otherwise");
        f.write(1, 0x2);
        let (got, off) = pci.mem_target(0x8000_1084).expect("in range");
        assert_eq!(got.name, "t");
        assert_eq!(off, 0x84);
        assert!(pci.mem_target(0x8000_2000).is_none(), "past the end of the BAR");
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

    use crate::cpu::mips_cache_v2::R5000Cache;
    use crate::cpu::mips_exec::{MipsCpuConfig, MipsExecutor};
    use crate::cpu::mips_tlb::MipsTlb;
    use std::sync::Arc;

    /// What jitv2's compile threads stop and start when one of them flushes
    /// the code arena: on a machine, the CPU thread. Here the CPU is this
    /// test's own loop, so `stop` asks it to park between instructions and
    /// waits until it has, and `start` lets it go.
    #[cfg(feature = "jitv2")]
    #[derive(Default)]
    struct JitPause {
        want: std::sync::atomic::AtomicBool,
        /// (parked, the loop has finished for good)
        state: std::sync::Mutex<(bool, bool)>,
        cv: std::sync::Condvar,
    }

    #[cfg(feature = "jitv2")]
    impl JitPause {
        /// Called by the loop between instructions.
        #[inline(always)]
        fn check(&self) {
            if self.want.load(std::sync::atomic::Ordering::Acquire) {
                let mut st = self.state.lock().unwrap();
                st.0 = true;
                self.cv.notify_all();
                while self.want.load(std::sync::atomic::Ordering::Acquire) {
                    st = self.cv.wait(st).unwrap();
                }
                st.0 = false;
            }
        }

        /// The loop is over: nothing will park again, so `stop` must not wait.
        fn finish(&self) {
            self.state.lock().unwrap().1 = true;
            self.cv.notify_all();
        }
    }

    #[cfg(feature = "jitv2")]
    impl crate::traits::Device for JitPause {
        fn step(&self, _cycles: u64) {}
        fn stop(&self) {
            self.want.store(true, std::sync::atomic::Ordering::Release);
            let mut st = self.state.lock().unwrap();
            while !st.0 && !st.1 {
                st = self.cv.wait(st).unwrap();
            }
        }
        fn start(&self) {
            let _st = self.state.lock().unwrap();
            self.want.store(false, std::sync::atomic::Ordering::Release);
            self.cv.notify_all();
        }
        fn is_running(&self) -> bool {
            !self.want.load(std::sync::atomic::Ordering::Acquire)
        }
        fn get_clock(&self) -> u64 { 0 }
    }

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
        // `MipsExecutor` and `MipsTlb` carry their arrays inline, and in a
        // debug build the construction temporaries overflow a test thread's
        // default stack — the whole binary aborts with SIGABRT, taking every
        // other test with it. `main.rs` solves the same problem the same way
        // for `Machine::new`.
        //
        // It went unnoticed because every test run in this project passes
        // `--release`, where the temporaries collapse. `cargo test` on its
        // own is what someone reviewing a patch would run.
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(trace_the_prom_from_reset_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    /// The CPU: `IRIS_IP32_CPU=r10000` (or `r10k`) for an R10000, as the O2
    /// was also sold with; an R5000 otherwise. Both run on the full cache
    /// models, not the shadow ones IP28 uses: the O2 PROM sizes memory with its
    /// stack in the data cache, before there is any RAM behind it, and a cache
    /// that keeps only tags loses that stack (the return address comes back as
    /// garbage straight after SizeMEM starts).
    fn trace_the_prom_from_reset_inner() {
        match std::env::var("IRIS_IP32_CPU").unwrap_or_default().to_ascii_lowercase().as_str() {
            "r10000" | "r10k" if std::env::var("IRIS_IP32_CACHE").as_deref() == Ok("handover") => {
                run_the_prom_from_reset::<crate::ip32_handover::HandoverCache>("R10000", |exec, steps| {
                    exec.cache.hand_over();
                    exec.resync_privilege_state();
                    eprintln!("ip32: the kernel started at step {steps}; caches written back, now on the shadow cache");
                })
            }
            "r10000" | "r10k" => run_the_prom_from_reset::<crate::cpu::mips_cache_v2::R10000O2Cache>("R10000", |_, _| {}),
            _ => run_the_prom_from_reset::<R5000Cache>("R5000", |_, _| {}),
        }
    }

    /// `on_kernel` runs once, the first time the PC enters a kernel's text:
    /// the R10000's cue to hand over from the full cache to the shadow one
    /// (`IRIS_IP32_CACHE=handover`).
    fn run_the_prom_from_reset<C>(cpu: &str, on_kernel: impl Fn(&mut MipsExecutor<MipsTlb, C>, u64))
    where
        C: crate::cpu::mips_cache_v2::CpuModel + From<Arc<dyn crate::traits::BusDevice>>,
    {
        let Some(prom) = prom_image() else { return };

        let sections = parse_prom_sections(&prom);
        eprintln!("ip32: PROM sections:");
        for s in &sections {
            eprintln!("   {:<10} v{:<5} @0x{:06x} len 0x{:05x}", s.name, s.version, s.file_off, s.length);
        }

        // Memory: IRIS_IP32_DIMMS names the kit in each DIMM pair from A
        // (`256,128,32`), IRIS_IP32_RAM_MB a total to fit the usual way, and
        // with neither it is 128 MB, a common O2 configuration. Anything an O2
        // cannot be fitted with is refused, with SGI's reason.
        let kits = match (std::env::var("IRIS_IP32_DIMMS"), std::env::var("IRIS_IP32_RAM_MB")) {
            (Ok(d), _) => d.split(',').map(|k| k.trim().trim_end_matches("MB").parse::<u32>()
                .map_err(|_| format!("IRIS_IP32_DIMMS: {k:?} is not a size in MB"))).collect::<Result<Vec<_>, _>>(),
            (_, Ok(mb)) => mb.trim().parse::<u32>().map_err(|_| format!("IRIS_IP32_RAM_MB: {mb:?}"))
                .and_then(crate::dev::crime_mem::kits_for),
            _ => Ok(vec![128]),
        };
        let mem = match kits.and_then(|k| O2Memory::from_kits(&k).map(|m| (k, m))) {
            Ok((k, m)) => {
                let pairs: Vec<String> = k.iter().zip("ABCD".chars()).map(|(s, p)| format!("{p} {s} MB")).collect();
                eprintln!("ip32: {cpu} with {} MB ({})", m.total() >> 20, pairs.join(", "));
                m
            }
            Err(e) => panic!("ip32: no such O2 memory fitting: {e}"),
        };
        let bus_prom = prom.clone();
        // IRIS_IP32_GFX=1: a keyboard and the graphics console's CRIME ID.
        let gfx = std::env::var("IRIS_IP32_GFX").as_deref() == Ok("1");
        let mut bus = Ip32Bus::with_memory(mem, prom);
        if gfx {
            // IRIS_IP32_MOUSE=1 adds the PS/2 mouse on port 1.
            bus.enable_graphics_with_mouse(std::env::var("IRIS_IP32_MOUSE").as_deref() == Ok("1"));
        }
        let bus = Arc::new(bus);
        // Keys from ip32-view (IRIS_IP32_KBD, a TCP port on 127.0.0.1).
        // The screen for ip32-view (IRIS_IP32_FB, a file both share) and, every
        // few seconds while it changes, a PNG of it (IRIS_IP32_PNG).
        let mut screen = std::env::var("IRIS_IP32_FB").ok().and_then(|p| match crate::ip32_screen::ScreenFile::create(&p) {
            Ok(s) => {
                eprintln!("ip32: screen in {p} (ip32-view {p})");
                Some(s)
            }
            Err(e) => {
                eprintln!("ip32: no screen file {p}: {e}");
                None
            }
        });
        let png_path = std::env::var("IRIS_IP32_PNG").ok();
        let mut frame = vec![0u8; 2048 * 2048 * 4];
        let mut last_frame = std::time::Instant::now();
        let mut last_png = std::time::Instant::now();
        let mut png_hash = 0u64;
        // IRIS_IP32_KEYS: what a person would type at the graphics console,
        // in segments split by `|`. Each waits until the guest has sat at a
        // prompt (polling an empty keyboard) for IRIS_IP32_KEYS_IDLE status
        // reads, then goes in one key per IRIS_IP32_KEY_GAP steps. `\r`,
        // `\e` and `\b` are Enter, Escape and Backspace.
        let mut key_script: std::collections::VecDeque<std::collections::VecDeque<char>> = std::env::var("IRIS_IP32_KEYS")
            .unwrap_or_default()
            .replace("\\r", "\r")
            .replace("\\e", "\x1b")
            .replace("\\b", "\x08")
            .split('|')
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().collect())
            .collect();
        let key_idle: u32 = std::env::var("IRIS_IP32_KEYS_IDLE").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
        let key_gap: u64 = std::env::var("IRIS_IP32_KEY_GAP").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000_000);
        // A kernel at a shell prompt reads the keyboard by interrupt, not by
        // polling it, so the idle test never fires there: after this many
        // steps without it, the next segment goes in anyway.
        let key_wait: u64 = std::env::var("IRIS_IP32_KEYS_WAIT").ok().and_then(|v| v.parse().ok()).unwrap_or(400_000_000);
        let mut typing = false;
        let mut next_key = 0u64;
        let mut segment_done = 0u64;
        let keys = std::env::var("IRIS_IP32_KBD").ok().and_then(|p| p.parse::<u16>().ok()).and_then(|p| {
            match crate::ip32_screen::KeyEvents::listen(p) {
                Ok(k) => {
                    eprintln!("ip32: keyboard from ip32-view on 127.0.0.1:{p}");
                    Some(k)
                }
                Err(e) => {
                    eprintln!("ip32: no keyboard port {p}: {e}");
                    None
                }
            }
        });
        let sysad: Arc<dyn crate::traits::BusDevice> = bus.clone();
        let cfg = MipsCpuConfig::for_model::<C>();
        let tlb = MipsTlb::new(cfg.tlb_entries);
        let mut exec: MipsExecutor<MipsTlb, C> = MipsExecutor::new(sysad, tlb, &cfg);

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
        // (step, vector, ExcCode, EPC, BadVAddr)
        let mut exceptions: Vec<(u64, u32, u32, u64, u64)> = Vec::new();
        let mut last_exc_pc = 0u32;
        // IRIS_IP32_FAST drops the per-instruction instrumentation -- the
        // stall detector's set operations, the loop map lookup and the
        // clock's mutex -- which together cost more than the emulation. For
        // a run whose point is "does it boot" rather than "where did it
        // stop", that is several times the throughput.
        let fast = std::env::var("IRIS_IP32_FAST").is_ok();
        // IRIS_IP32_BREAK=pc,pc,... prints the registers the first few times
        // each of those addresses is reached: what a function was called
        // with, or returned, without a debugger.
        // IRIS_IP32_STOP_ON_RESET ends the run soon after a kernel hands
        // control back to the PROM (a panic, a reboot, a reset), so the report
        // describes the boot that failed.
        let stop_on_reset = std::env::var("IRIS_IP32_STOP_ON_RESET").is_ok();
        let trap_log = std::env::var("IRIS_IP32_TRAP_LOG").is_ok();
        // IRIS_IP32_TRAP_LOG=all logs every exception but interrupts and TLB
        // refills, and
        // IRIS_IP32_TRAP_FROM=step starts logging there: the way to find the
        // first fault in front of a crash loop.
        let trap_all = std::env::var("IRIS_IP32_TRAP_LOG").as_deref() == Ok("all");
        // IRIS_IP32_TRAP_LOG=tlb adds TLB refills too (busy: use a window).
        let trap_tlb = std::env::var("IRIS_IP32_TRAP_LOG").as_deref() == Ok("tlb");
        let trap_from: u64 = std::env::var("IRIS_IP32_TRAP_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut trap_logged = 0u32;
        // IRIS_IP32_STOP_ON=text ends the run once the console has printed
        // `text` (a panic message, say), checked every million steps.
        let stop_on = std::env::var("IRIS_IP32_STOP_ON").ok().filter(|t| !t.is_empty());
        // IRIS_IP32_STOP_AFTER=steps keeps running that long past the match and
        // reports where the time went: the answer to "it printed X and then
        // nothing".
        let stop_after: u64 = std::env::var("IRIS_IP32_STOP_AFTER").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut stop_at: Option<u64> = None;
        // IRIS_IP32_PROFILE_FROM=step starts the same profile at a step count,
        // for a hang on the graphics console, where STOP_ON (which reads
        // serial) never sees the text.
        let profile_from: Option<u64> = std::env::var("IRIS_IP32_PROFILE_FROM").ok().and_then(|v| v.parse().ok());
        let mut hot: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
        // IRIS_IP32_BREAK_AFTER_STOP arms the breakpoints only once
        // IRIS_IP32_STOP_ON has matched, to look at a hang rather than a boot.
        let break_after_stop = std::env::var("IRIS_IP32_BREAK_AFTER_STOP").is_ok();
        let mut breaks: Vec<(u32, u32)> = std::env::var("IRIS_IP32_BREAK").unwrap_or_default()
            .split(',').filter_map(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok())
            .map(|a| (a, 0)).collect();
        // A loaded kernel lives in kseg0 well above the PROM's own use of it.
        // Track when we get there and what the last thing before leaving was.
        let mut kernel_entered = 0u64;
        let mut kernel_max = 0u32;
        let mut kernel_exit: Vec<u32> = Vec::new();
        let mut kernel_exit_at = 0u64;
        let mut kernel_first = 0u32;
        let mut kernel_trace: Vec<u32> = Vec::new();
        // By default, drive the machine the way a person would: take the
        // menu into the command monitor and ask it what it thinks it is.
        // That turns the trace into an end-to-end check -- the answer comes
        // back through the serial port, and the Ethernet address in it came
        // off the 1-Wire chip.
        let feed_bytes: Option<Vec<u8>> = Some(
            std::env::var("IRIS_IP32_INPUT")
                .unwrap_or_else(|_| "5\\rversion\\rhinv -v\\rprintenv\\r".to_string())
                .replace("\\r", "\r")
                .into_bytes(),
        );
        // What to wait for before typing. The sloader prompt was the only
        // thing that ever appeared; now that the PROM reaches its menu,
        // "Option?" is the interesting one.
        if let Ok(path) = std::env::var("IRIS_IP32_CONSOLE") {
            if let Err(e) = bus.com0.log_to(std::path::Path::new(&path)) {
                eprintln!("ip32: console log {path}: {e}");
            }
        }
        // IRIS_IP32_NET=1: the Ethernet behind IRIS's NAT gateway (DHCP,
        // DNS), with IRIS_IP32_FWD="tcp:2263:22,..." forwarding host ports
        // on 127.0.0.1 to the guest.
        if std::env::var("IRIS_IP32_NET").is_ok() {
            let mut cfg = crate::net::GatewayConfig::default();
            for rule in std::env::var("IRIS_IP32_FWD").unwrap_or_default().split(',').filter(|r| !r.is_empty()) {
                let parts: Vec<&str> = rule.split(':').collect();
                let proto = match parts.first().copied() {
                    Some("udp") => crate::config::ForwardProto::Udp,
                    _ => crate::config::ForwardProto::Tcp,
                };
                let (Some(Ok(host_port)), Some(Ok(guest_port))) =
                    (parts.get(1).map(|p| p.parse::<u16>()), parts.get(2).map(|p| p.parse::<u16>()))
                else {
                    eprintln!("ip32: IRIS_IP32_FWD rule {rule:?}: want proto:host_port:guest_port");
                    continue;
                };
                cfg.port_forwards.push(crate::config::PortForwardConfig {
                    proto,
                    host_port,
                    guest_port,
                    bind: crate::config::ForwardBind::Localhost,
                });
                eprintln!("ip32: forwarding 127.0.0.1:{host_port} to the guest's port {guest_port}");
            }
            eprintln!("ip32: Ethernet on NAT: gateway {}, guest {} (DHCP)", cfg.gateway_ip, cfg.client_ip);
            bus.mec.start_nat(cfg);
        }
        // IRIS_IP32_SERIAL=port: com0 as a live console on 127.0.0.1:port
        // (telnet or nc), alongside the log and any script.
        if let Some(port) = std::env::var("IRIS_IP32_SERIAL").ok().and_then(|v| v.parse::<u16>().ok()) {
            match bus.com0.serve_tcp(port) {
                Ok(port) => eprintln!("ip32: com0 console on 127.0.0.1:{port} (telnet 127.0.0.1 {port})"),
                Err(e) => eprintln!("ip32: com0 console on port {port}: {e}"),
            }
        }
        let expect = std::env::var("IRIS_IP32_EXPECT")
            .unwrap_or_else(|_| "Option?".to_string());
        // A whole conversation, not one line: IRIS_IP32_SCRIPT is
        // `wait-for=>type-this` steps separated by `;;`, each armed only once
        // the one before it has fired. Booting to multi-user means answering
        // the PROM, then the bootloader, then init, and each prompt only
        // exists after the previous answer.
        let mut script: Vec<(String, Vec<u8>)> = std::env::var("IRIS_IP32_SCRIPT")
            .ok()
            .map(|v| {
                v.split(";;")
                    .filter_map(|step| {
                        let (w, t) = step.split_once("=>")?;
                        Some((w.to_string(), t.replace("\\r", "\r").into_bytes()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        script.reverse();
        let mut script_seen = 0usize;
        let mut last_out_len = usize::MAX;
        // post1 contains real timed delays — one of them waits a full second
        // of UST, which at UST_TICKS_DEN instructions per tick is 100M steps
        // on its own. The default budget clears that with room to spare;
        // IRIS_IP32_STEPS raises it for longer explorations.
        let limit: u64 = std::env::var("IRIS_IP32_STEPS").ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500_000_000);
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

        // Status writes have to reach the executor, or a kernel that sets KX
        // keeps translating 64-bit addresses as 32-bit ones: XKPHYS loads go to
        // the TLB and fault (OpenBSD's first device access). Installed here,
        // where `exec` has stopped moving, since the callback keeps a pointer.
        exec.install_status_cb();
        // A build with tcache (jitv2 implies it) would run the PROM on a cache
        // that holds no data, and POST's cache-held stack would go with it. So
        // the cache holds its data until a kernel starts, then turns
        // transparent. A no-op without the feature.
        exec.cache.set_tcache_live(false);
        // jitv2 takes over from the interpreter once a kernel runs, compiling
        // on a pool of IRIS_IP32_JIT_THREADS threads (4 by default, as every
        // machine here runs). IRIS_IP32_JIT=0 keeps the interpreter throughout.
        #[cfg(feature = "jitv2")]
        let mut jit_on = false;
        #[cfg(feature = "jitv2")]
        let jit_pause = Arc::new(JitPause::default());
        #[cfg(feature = "jitv2")]
        let jit_pause_dev: Arc<dyn crate::traits::Device> = jit_pause.clone();
        // A kernel calls back into the firmware (ARCS PowerDown, Reboot), and
        // that path sets the caches up again and keeps data in them the way
        // POST does. So while the PC is in the PROM or its copy in RAM the
        // cache holds data again, and the interpreter runs it.
        let mut in_firmware = false;
        let mut firmware_calls = 0u64;
        // IRIS_IP32_ARCS_FPU=1, a test aid and not hardware behaviour: mark
        // the FPU usable while a kernel is inside the firmware, as NetBSD
        // itself does around ArcPowerDown. The PROM's graphics console scrolls
        // with ldc1/sdc1, and a kernel printing through it with CU1 clear
        // (NetBSD on IP32 until crmfb attaches) takes a coprocessor-unusable
        // fault there.
        let arcs_fpu = std::env::var("IRIS_IP32_ARCS_FPU").as_deref() == Ok("1");
        let mut cu1_saved = 0u32;
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
            if (0x8006_0000..0x8100_0000).contains(&pc) {
                if kernel_entered == 0 {
                    kernel_entered = steps;
                    kernel_first = pc;
                    exec.cache.set_tcache_live(true);
                    if cfg!(feature = "tcache") {
                        eprintln!("ip32: the kernel started at step {steps}; caches written back, now transparent (tcache)");
                    }
                    on_kernel(&mut exec, steps);
                    #[cfg(feature = "jitv2")]
                    if std::env::var("IRIS_IP32_JIT").as_deref() != Ok("0") {
                        let threads: usize = std::env::var("IRIS_IP32_JIT_THREADS").ok()
                            .and_then(|v| v.parse().ok()).unwrap_or(4).max(1);
                        exec.install_jit_hooks();
                        let jit = exec.jitv2.clone();
                        {
                            let mut g = jit.lock();
                            g.compile_queue.set_thread_count(threads);
                            g.compile_queue.set_cpu(Arc::downgrade(&jit_pause_dev));
                            g.compile_queue.set_owner(Arc::downgrade(&jit));
                            let stats = g.stats.clone();
                            g.compile_queue.start(bus.clone() as Arc<dyn crate::traits::BusDevice>, stats);
                        }
                        jit_on = true;
                        eprintln!("ip32: jitv2 on from here, {threads} compile threads");
                    }
                }
                // The opening moves, which is where a kernel that runs away
                // decides to.
                if kernel_trace.len() < 96 && kernel_trace.last() != Some(&pc) {
                    kernel_trace.push(pc);
                }
                if pc > kernel_max {
                    kernel_max = pc;
                }
            } else if kernel_entered != 0 && kernel_exit.is_empty() {
                // Left the kernel for PROM or reset space: this is where it
                // stopped being a kernel, and the ring says how it got there.
                kernel_exit = ring_path(&ring, ring_at);
                kernel_exit_at = steps;
            }
            // Exception vectors. Landing on one is the single most
            // informative event in a kernel bring-up, and without this the
            // only symptom is "it restarted".
            if matches!(pc, 0x8000_0000 | 0x8000_0080 | 0x8000_0100 | 0x8000_0180
                            | 0xa000_0000 | 0xa000_0080 | 0xa000_0100 | 0xa000_0180
                            | 0xbfc0_0200 | 0xbfc0_0280 | 0xbfc0_0300 | 0xbfc0_0380
                            | 0xbfc0_0400 | 0xbfc0_0480)
                && last_exc_pc != pc
            {
                let cause = exec.core.cp0_cause as u32;
                let exc = (cause >> 2) & 0x1f;
                // IRIS_IP32_TRAP_LOG: every address error and bus error, as it
                // happens, with the instruction -- these are what a kernel
                // turns into SIGBUS, and in user mode they are otherwise
                // invisible.
                if trap_log && steps >= trap_from && (matches!(exc, 4..=7) || (trap_all && !matches!(exc, 0 | 2 | 3)) || (trap_tlb && exc != 0)) && trap_logged < 200 {
                    trap_logged += 1;
                    let epc = exec.core.cp0_epc;
                    let insn = exec.debug_read(epc, 4).map(|w| w as u32).ok();
                    let dis = insn.map(|w| crate::cpu::mips_dis::disassemble(w, epc, None)).unwrap_or_default();
                    let g = exec.core.gpr;
                    let pa = |exec: &mut MipsExecutor<MipsTlb, C>, va: u64| {
                        let t = exec.debug_translate(va);
                        if t.is_exception() { String::from("-") } else { format!("{:x}", t.phys) }
                    };
                    let (pc_pa, sp_pa) = (pa(&mut exec, epc), pa(&mut exec, g[29]));
                    eprintln!("ip32: trap @{steps} exc {exc} epc {epc:x} (pa {pc_pa}) badva {:x} status {:x} [{}] a0 {:x} a1 {:x} v0 {:x} v1 {:x} t0 {:x} t1 {:x} sp {:x} (pa {sp_pa}) ra {:x}",
                              exec.core.cp0_badvaddr, exec.core.cp0_status, dis,
                              g[4], g[5], g[2], g[3], g[8], g[9], g[29], g[31]);
                }
                if exceptions.len() < 24 {
                    exceptions.push((steps, pc, exc, exec.core.cp0_epc, exec.core.cp0_badvaddr));
                }
                last_exc_pc = pc;
            } else if pc != last_exc_pc {
                last_exc_pc = 0;
            }
            // Publish the PC so devices can attribute the accesses this
            // instruction is about to make.
            bus.pc.set(pc);
            bus.pc.set_ra(exec.core.gpr[31] as u32);
            if stop_at.is_none() && profile_from == Some(steps) {
                eprintln!("ip32: profiling from step {steps} for {stop_after} steps");
                stop_at = Some(steps + stop_after);
            }
            if let Some(at) = stop_at {
                *hot.entry(pc).or_insert(0) += 1;
                if steps >= at {
                    let mut v: Vec<_> = hot.iter().map(|(&p, &n)| (n, p)).collect();
                    v.sort_unstable_by(|a, b| b.cmp(a));
                    eprintln!("ip32: hottest PCs over the last {stop_after} steps:");
                    for (n, p) in v.iter().take(24) {
                        let w = bus.read32(p & 0x1fff_ffff).data;
                        eprintln!("   {p:08x} {n:>10}  {}", crate::cpu::mips_dis::disassemble(w, *p as u64, None));
                    }
                    // The same time by 256-byte region, so a wait spread over a
                    // whole function still shows up.
                    let mut regions: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
                    for (&p, &n) in hot.iter() {
                        *regions.entry(p & !0xff).or_insert(0) += n;
                    }
                    let mut r: Vec<_> = regions.into_iter().map(|(p, n)| (n, p)).collect();
                    r.sort_unstable_by(|a, b| b.cmp(a));
                    eprintln!("ip32: hottest 256-byte regions:");
                    for (n, p) in r.iter().take(16) {
                        eprintln!("   {p:08x} {n:>10}  {:.1}%", *n as f64 * 100.0 / stop_after.max(1) as f64);
                    }
                    break;
                }
            } else if let Some(t) = &stop_on {
                if steps % 1_000_000 == 0 && bus.com0.output_tail(t.len() + 4096).contains(t.as_str()) {
                    eprintln!("ip32: console printed {t:?} by step {steps}; stopping after {stop_after} more");
                    if stop_after == 0 {
                        break;
                    }
                    stop_at = Some(steps + stop_after);
                }
            }
            if stop_on_reset && !kernel_exit.is_empty() && steps > kernel_exit_at + 20_000_000 {
                eprintln!("ip32: the kernel left at step {kernel_exit_at}; stopping");
                break;
            }
            if !breaks.is_empty() && (!break_after_stop || stop_at.is_some()) {
                let pa = pc & 0x1fff_ffff;
                if let Some(b) = breaks.iter_mut().find(|b| b.0 & 0x1fff_ffff == pa && b.1 < 8) {
                    b.1 += 1;
                    let g = &exec.core.gpr;
                    eprintln!("ip32: break {pc:08x} @{steps}: a0 {:x} a1 {:x} a2 {:x} a3 {:x} v0 {:x} v1 {:x} s0 {:x} s1 {:x} s2 {:x} s3 {:x} sp {:x} ra {:x} status {:x} cause {:x} count {:x} compare {:x} crime stat {:x} mask {:x}",
                              g[4], g[5], g[6], g[7], g[2], g[3], g[16], g[17], g[18], g[19], g[29], g[31],
                              exec.core.cp0_status, exec.core.cp0_cause,
                              exec.core.cp0_count as u32, exec.core.cp0_compare as u32,
                              bus.crime.load(crime_reg::INTSTAT), bus.crime.load(crime_reg::INTMASK));
                }
            }
            // The clock only has to be right when something reads it, and
            // the readers are all on the periodic path below.
            if !fast || steps % TIMER_POLL_STEPS == 0 {
                bus.ust.advance_to(steps);
            }

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
            // spent in it.
            // A loop that costs half the run is the thing to
            // fix, whether or not it ever exits.
            if !fast { if let Some(&key) = loop_of.get(&pc) {
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
            } }
            if !fast { if let Some(body) = stall.step(pc) {
                for &p in &body {
                    loop_of.entry(p).or_insert(body[0]);
                }
                if stalls.len() < 8 {
                    let cause = exec.core.cp0_cause as u32;
                    let g = exec.core.gpr;
                    stalls.push((steps, body, cause, exec.core.cp0_epc, exec.core.cp0_badvaddr,
                                 g));
                }
            } }

            // Optionally answer sloader's prompt, for experimenting with what
            // it accepts. Off unless IRIS_IP32_INPUT is set, and `contains` is
            // only evaluated while it is: scanning the output buffer on every
            // instruction is not something to do by default.
            // Script steps fire in order, each waiting for its own prompt.
            //
            // Only look when the guest has actually said something new.
            // `output()` copies the whole console buffer, and calling it on
            // every instruction costs more than the emulation -- it turned a
            // boot into something that had not finished after half an hour.
            let out_len = bus.com0.bytes_out();
            let said_something = out_len != last_out_len;
            last_out_len = out_len;
            if said_something {
                if let Some((wait, text)) = script.last() {
                    if bus.com0.output()[script_seen..].contains(wait.as_str()) {
                        bus.com0.feed(text);
                        script_seen = out_len;
                        script.pop();
                    }
                }
            }
            if let Some(feed) = feed_bytes.as_ref() {
                if !fed && said_something && bus.com0.output().contains(expect.as_str()) {
                    bus.com0.feed(feed);
                    fed = true;
                    fed_at = steps;
                }
            }
            if (0xbfc0_0000..0xbfc8_0000).contains(&pc) && pc > max_prom_pc {
                max_prom_pc = pc;
            }
            if kernel_entered != 0 {
                let fw = (0xbfc0_0000..0xbfc8_0000).contains(&pc)
                    || (FIRMWARE_LOAD_VA..FIRMWARE_LOAD_VA + FIRMWARE_SPAN).contains(&pc);
                if fw != in_firmware {
                    in_firmware = fw;
                    exec.cache.set_tcache_live(!fw);
                    if arcs_fpu {
                        const CU1: u32 = 1 << 29;
                        if fw {
                            cu1_saved = exec.core.cp0_status & CU1;
                            exec.core.cp0_status |= CU1;
                        } else {
                            exec.core.cp0_status = (exec.core.cp0_status & !CU1) | cu1_saved;
                        }
                        exec.resync_privilege_state();
                    }
                    if fw {
                        firmware_calls += 1;
                        if firmware_calls <= 8 {
                            eprintln!("ip32: into the firmware at 0x{pc:08x}, step {steps}");
                        }
                    }
                }
            }
            // About 25 frames a second to the viewer, by host time.
            if steps % 65536 == 0 && (screen.is_some() || png_path.is_some())
                && last_frame.elapsed() >= std::time::Duration::from_millis(40)
            {
                last_frame = std::time::Instant::now();
                let size = bus.gbe.render_from(&bus.ram, &mut frame);
                let (w, h) = size.unwrap_or((0, 0));
                if let Some(s) = screen.as_mut() {
                    let n = (w * h * 4) as usize;
                    s.pixels(w, h)[..n].copy_from_slice(&frame[..n]);
                    s.publish(w, h);
                }
                if let (Some(p), Some((w, h))) = (png_path.as_ref(), size) {
                    if last_png.elapsed() >= std::time::Duration::from_secs(5) {
                        last_png = std::time::Instant::now();
                        let n = (w * h * 4) as usize;
                        let hash = frame[..n].iter().step_by(61).fold(0u64, |a, &b| a.wrapping_mul(31).wrapping_add(b as u64));
                        if hash != png_hash {
                            png_hash = hash;
                            if let Err(e) = crate::ip32_screen::save_png(p, &frame[..n], w, h) {
                                eprintln!("ip32: PNG {p}: {e}");
                            }
                        }
                    }
                }
            }
            #[cfg(feature = "jitv2")]
            if jit_on {
                jit_pause.check();
            }
            #[cfg(feature = "jitv2")]
            if jit_on && !in_firmware { exec.step_jit(); } else { exec.step_int(); }
            #[cfg(not(feature = "jitv2"))]
            exec.step_int();
            steps += 1;

            // Deliver the CP0 timer interrupt. IRIS normally arms this as an
            // hptimer one-shot when the guest writes Compare, on a thread
            // this harness does not run -- so without this no time passes at
            // all inside a guest kernel, and NetBSD sits forever "waiting 2
            // seconds for devices to settle" with its clock stopped.
            if steps % TIMER_POLL_STEPS == 0 {
                // Device interrupts reach the CPU through CRIME, which
                // NetBSD attaches as interrupt 0 -- IP2. Without this the
                // driver only discovers a finished command when its own
                // watchdog fires, which it reports as "Interrupts may not be
                // functioning", and it is right.
                bus.service();
                // Keys pushed from the viewer arrive between register accesses.
                if let (Some(k), Some(dev)) = (keys.as_ref(), bus.ps2.devices()) {
                    while let Some((name, down)) = k.pop() {
                        if let Some(code) = crate::ip32_screen::keycode(&name) {
                            dev.push_kb(code, down);
                        }
                    }
                }
                if let Some(dev) = bus.ps2.devices() {
                    if !typing && !key_script.is_empty() && dev.scanning()
                        && (bus.ps2.idle_polls() >= key_idle || (segment_done != 0 && steps - segment_done >= key_wait))
                    {
                        typing = true;
                        next_key = steps;
                    }
                    if typing && steps >= next_key {
                        let seg = key_script.front_mut().unwrap();
                        if let Some(ch) = seg.pop_front() {
                            dev.type_char(ch);
                            next_key = steps + key_gap;
                        }
                        if seg.is_empty() {
                            key_script.pop_front();
                            typing = false;
                            segment_done = steps;
                            bus.ps2.reset_idle();
                            eprintln!("ip32: typed a key segment at step {steps}");
                        }
                    }
                }
                bus.ps2_touched();
                let bits = &exec.core.hot.interrupts;
                let cur = bits.load(std::sync::atomic::Ordering::Relaxed);
                let ip2 = crate::cpu::mips_core::CAUSE_IP2 as u64;
                let want = if bus.crime.interrupting() { cur | ip2 } else { cur & !ip2 };
                if want != cur {
                    bits.store(want, std::sync::atomic::Ordering::Relaxed);
                }

                if bus.rtc.powered_off() {
                    eprintln!("ip32: the guest switched the machine off at step {steps}");
                    break;
                }
                // Advancing Count is enough: the core raises IP7 when an
                // advance crosses Compare, once, and a Compare write is the
                // acknowledgement, as on silicon. This used to raise IP7 as a
                // *level* whenever Count was within 2^30 past Compare, which
                // re-fired forever on a timer parked at Compare = Count - 1
                // (OpenBSD before its clock starts): an interrupt storm.
                let _ = exec.core.count_now();
            }
            if arm_call.is_none() && exec.core.gpr[31] as u32 == pc.wrapping_add(8) {
                arm_call = Some((pc.wrapping_add(8), steps, 1));
            }
        }
        #[cfg(feature = "jitv2")]
        jit_pause.finish();

        let pc = exec.core.pc as u32;
        for (name, hot) in [("gbe", bus.gbe.hot.lock().unwrap().clone()), ("crime-re", bus.crime_re.hot.lock().unwrap().clone())] {
            let mut v: Vec<_> = hot.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            let top: Vec<String> = v.iter().take(8).map(|(o, n)| format!("+{o:#x} x{n}")).collect();
            eprintln!("ip32: {name} hottest reads: {}", top.join(", "));
        }
        eprintln!("ip32: crime-re primitives run: {}", bus.crime_re.drawn.load(Ordering::Relaxed));
        eprintln!("ip32: gbe {}", bus.gbe.summary());
        eprintln!("ip32: ps2 traffic (port>to device, port<from device): {}", bus.ps2.traffic());
        let shown = bus.gbe.render_from(&bus.ram, &mut frame);
        eprintln!("ip32: gbe would show {shown:?}");
        if let (Some(p), Some((w, h))) = (png_path.as_ref(), shown) {
            let _ = crate::ip32_screen::save_png(p, &frame[..(w * h * 4) as usize], w, h);
        }
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
            for (a, w, v, p, t, _) in watched.iter().take(40) {
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
                            .map(|&r| format!("{}=0x{:x}", crate::cpu::mips_dis::reg_name(r as u32), g[r]))
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
        // IRIS_IP32_DUMP=addr:len hex-dumps physical memory after the run.
        if let Ok(spec) = std::env::var("IRIS_IP32_DUMP") {
            for part in spec.split(',') {
                if let Some((a, n)) = part.split_once(':') {
                    let a = u32::from_str_radix(a.trim().trim_start_matches("0x"), 16).unwrap_or(0);
                    let n = n.trim().parse::<u32>().unwrap_or(64);
                    eprintln!("ip32: memory at 0x{a:08x}:");
                    for row in 0..n.div_ceil(16) {
                        let base = a + row * 16;
                        let mut line = format!("   0x{base:08x} ");
                        for i in 0..4 {
                            line += &format!(" {:08x}", bus.read32(base + i * 4).data);
                        }
                        eprintln!("{line}");
                    }
                }
            }
        }
        // IRIS_IP32_DIS=start:end[,start:end...] disassembles ranges once the
        // run is over, reading through the bus so RAM-resident code (post1)
        // is visible.
        for spec in std::env::var("IRIS_IP32_DIS").unwrap_or_default().split(',') {
            if let Some((a, b)) = spec.split_once(':') {
                let a = u32::from_str_radix(a.trim_start_matches("0x"), 16).unwrap_or(0);
                let b = u32::from_str_radix(b.trim_start_matches("0x"), 16).unwrap_or(0);
                eprintln!("ip32: disassembly 0x{a:08x}..0x{b:08x}");
                let mut p = a;
                while p < b {
                    let w = bus.read32(p & 0x1fff_ffff).data;
                    eprintln!("   0x{p:08x}  {w:08x}  {}",
                              crate::cpu::mips_dis::disassemble(w, p as u64, None));
                    p += 4;
                }
            }
        }
        if kernel_entered != 0 {
            eprintln!("ip32: kernel entered at step {kernel_entered} at 0x{kernel_first:08x}, furthest PC 0x{kernel_max:08x}");
            eprintln!("   its first {} PCs:", kernel_trace.len());
            for chunk in kernel_trace.chunks(8) {
                let line: Vec<String> = chunk.iter().map(|p| format!("{p:08x}")).collect();
                eprintln!("     {}", line.join(" "));
            }
            if !kernel_exit.is_empty() {
                eprintln!("   left it at step {kernel_exit_at}; last distinct PCs:");
                for chunk in kernel_exit.iter().rev().take(32).rev().collect::<Vec<_>>().chunks(8) {
                    let line: Vec<String> = chunk.iter().map(|p| format!("{p:08x}")).collect();
                    eprintln!("     {}", line.join(" "));
                }
            } else {
                eprintln!("   and never left it");
            }
        }
        eprintln!("ip32: exceptions taken: {}", exceptions.len());
        for (at, vec, exc, epc, bad) in exceptions.iter().take(12) {
            eprintln!("   @{at} vector 0x{vec:08x} {} EPC 0x{epc:08x} BadVAddr 0x{bad:08x}",
                      exc_name(*exc));
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
                              crate::cpu::mips_dis::disassemble(w, p as u64, None));
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
                    .map(|&r| format!("{}=0x{:016x}", crate::cpu::mips_dis::reg_name(r as u32), gpr[r]))
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
            let who: Vec<String> = bus.mace.who_touched(off).iter()
                .map(|(p, ra)| format!("{p:08x}<-{ra:08x}")).collect();
            eprintln!("   +0x{:06x}  {:>8} R  {:>4} W   from {}",
                      off, r, w, who.join(" "));
        }
        eprintln!("ip32: serial DMA:\n   {}", bus.sdma.summary().replace('\n', "\n   "));

        for slot in 0..8u8 {
            if let Some(f) = bus.macepci.slot(slot) {
                let t: Vec<String> = f.touched().iter()
                    .map(|(r, w)| format!("{:02x}{}", r, if *w { "W" } else { "R" }))
                    .collect();
                eprintln!("ip32: PCI slot {slot} ({}) config touched: {}", f.name, t.join(" "));
                for i in 0..6 {
                    let b = f.bar(i);
                    if b != 0 {
                        eprintln!("   BAR{i} programmed to 0x{b:08x}");
                    }
                }
                eprintln!("   memory decoding {}",
                          if f.memory_enabled() { "enabled" } else { "disabled" });
                let ml = f.mem_log();
                eprintln!("   {} accesses to its registers:", ml.len());
                // Collapse repeats: a poll says one thing however often it
                // happens, and the distinct offsets are the shopping list.
                let mut runs: Vec<(u32, bool, u32, u32, u32)> = Vec::new();
                for (off, w, v, pc, _) in ml.iter().copied() {
                    match runs.last_mut() {
                        Some(r) if r.0 == off && r.1 == w && r.2 == v => r.4 += 1,
                        _ => runs.push((off, w, v, pc, 1)),
                    }
                }
                {
                    // What the whole conversation touched, as a shopping
                    // list: this is the part of the chip that has to exist.
                    let mut by_off: std::collections::BTreeMap<u32, (u32, u32)> =
                        Default::default();
                    for (off, w, _, _, _) in ml.iter() {
                        let e = by_off.entry(*off).or_insert((0, 0));
                        if *w { e.1 += 1 } else { e.0 += 1 }
                    }
                    let line: Vec<String> = by_off.iter()
                        .map(|(o, (r, w))| format!("{o:02x}:{r}r/{w}w"))
                        .collect();
                    eprintln!("      registers touched ({}): {}",
                              by_off.len(), line.join(" "));
                }
                // The tail, with the poll registers dropped: HCNTRL and
                // INTSTAT are read thousands of times and say nothing.
                let quiet: Vec<&(u32, bool, u32, u32, u32)> = ml.iter()
                    .filter(|(o, _, _, _, _)| !matches!(*o, 0x87 | 0x91 | 0x61))
                    .collect();
                {
                    // The sequencer's scratch RAM, as the driver left it. A
                    // four-byte run that looks like a DMA address is what we
                    // are after: the host-memory array of SCB addresses.
                    let mut sram = [0u8; 0x40];
                    for (o, w, v, _, _) in ml.iter() {
                        if *w && (0x20..0x60).contains(o) {
                            sram[(*o - 0x20) as usize] = *v as u8;
                        }
                    }
                    eprint!("      sequencer scratch RAM 0x20..0x5f:");
                    for (i, b) in sram.iter().enumerate() {
                        if i % 16 == 0 {
                            eprint!("\n        {:02x}:", 0x20 + i);
                        }
                        eprint!(" {b:02x}");
                    }
                    eprintln!();
                }
                {
                    // The SCSI block, 0x00..0x1f: selection, transfer control
                    // and IDs. If the target a command is for is not in the
                    // SCB, it is set here.
                    let sel: Vec<String> = ml.iter()
                        .filter(|(o, w, _, _, _)| *w && *o < 0x20)
                        .map(|(o, _, v, _, _)| format!("{o:02x}={v:02x}"))
                        .collect();
                    eprintln!("      SCSI block writes ({}): {}", sel.len(), sel.join(" "));
                }
                eprintln!("      last {} non-polling accesses:", quiet.len().min(24));
                for (off, w, v, pc, ra) in quiet.iter().rev().take(24).rev() {
                    eprintln!("      +0x{off:03x} {} 0x{v:02x}  from PC 0x{pc:08x}<-0x{ra:08x}",
                              if *w { "W" } else { "R" });
                }
                for (off, w, v, pc, n) in runs.iter().take(6) {
                    let times = if *n > 1 { format!(" x{n}") } else { String::new() };
                    eprintln!("      +0x{off:03x} {} 0x{v:08x}  from PC 0x{pc:08x}{times}",
                              if *w { "W" } else { "R" });
                }
            }
        }
        for (scsi, _) in bus.scsi.lock().unwrap().iter() {
            eprintln!("ip32: SCSI controller: {} bytes of sequencer program downloaded",
                      scsi.seqram_len());
            let (c, bi, bo) = scsi.totals();
            eprintln!("   {c} commands all session: {bi} bytes read, {bo} written");
            let hits = scsi.dma_hits();
            if !hits.is_empty() {
                eprintln!("   {} DMA writes in the watched range, first few:", hits.len());
                for (a, v) in hits.iter().take(8) {
                    eprintln!("      0x{a:08x} <- 0x{v:02x}");
                }
            }
            eprintln!("   sequencer paused/restarted {} times", scsi.pauses());
            for n in scsi.notes().iter() {
                eprintln!("   {n}");
            }
            let ex = scsi.executed();
            eprintln!("   {} command(s) fetched and run", ex.len());
            for (tag, scb, cdb) in ex.iter().rev().take(60).rev() {
                let hex: Vec<String> = cdb.iter().map(|b| format!("{b:02x}")).collect();
                eprintln!("      tag {tag:2} scb 0x{scb:08x}  {}", hex.join(" "));
            }
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

        {
            let pw = bus.prom_writes();
            if !pw.is_empty() {
                eprintln!("ip32: {} write(s) into the flash, first few:", pw.len());
                for (a, v) in pw.iter().take(12) {
                    eprintln!("   0x{a:08x} <- 0x{v:02x}");
                }
            }
        }
        bus.save_flash();
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

        // The milestone, stated as a check. Each of these is a different part
        // of the machine answering, and the trace above says which one gave
        // up if any of them stops being true.
        let out = bus.com0.output();
        for want in [
            // POST finished and the PROM took over the console.
            "System Maintenance Menu",
            // It accepted input on the serial port and ran a command.
            "Command Monitor",
            // It knows what it is.
            "System: IP32",
            "Memory size: 128 Mbytes",
            // And this came off the 1-Wire identity chip.
            "eaddr=08:00:69:12:34:56",
        ] {
            assert!(out.contains(want), "PROM console never said {want:?}");
        }
        assert!(
            !out.contains("ds2502"),
            "the identity chip reported a failure: the console is above"
        );
    }
}

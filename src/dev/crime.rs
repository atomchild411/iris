//! CRIME's CPU interface on the O2: its interrupt controller and memory
//! bank control (the rendering engine is `o2gfx`).

use std::io::Write;
use std::sync::Mutex;

use crate::traits::{BusDevice, BusRead32, BusRead64, Device, Resettable, BUS_OK};
use crate::ip32::*;

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
    /// The guest's last accesses to the interrupt registers (status, mask,
    /// HARDINT, SOFTINT): `(write, offset, value)`, newest last, for the
    /// control port's `crimelog` (chasing interrupt storms).
    irqlog: Mutex<std::collections::VecDeque<(bool, u32, u64)>>,
    pub(crate) revision: u64,
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
            irqlog: Mutex::new(std::collections::VecDeque::new()),
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
    pub(crate) fn intstat(&self) -> u64 {
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
        self.ust.guest_us().wrapping_mul(CRIME_TICKS_PER_US)
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

    fn log_irq(&self, write: bool, off: u32, val: u64) {
        if matches!(off, crime_reg::INTSTAT | crime_reg::INTMASK | crime_reg::HARDINT | crime_reg::SOFTINT) {
            let mut l = self.irqlog.lock().unwrap();
            if l.len() >= 8192 {
                l.pop_front();
            }
            l.push_back((write, off, val));
        }
    }

    /// The interrupt-register log, one access per line, oldest first.
    pub fn irqlog_text(&self) -> String {
        self.irqlog.lock().unwrap().iter().map(|&(w, off, v)| format!("{} {:#04x} {:#018x}\n", if w { 'W' } else { 'R' }, off, v)).collect()
    }

    pub(crate) fn load(&self, off: u32) -> u64 {
        let v = self.load_inner(off);
        self.log_irq(false, off, v);
        v
    }

    fn load_inner(&self, off: u32) -> u64 {
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
        self.log_irq(true, off, val);
        self.store_inner(off, val);
    }

    fn store_inner(&self, off: u32, val: u64) {
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

impl Device for Crime {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool { true }
    fn get_clock(&self) -> u64 { 0 }

    fn register_commands(&self) -> Vec<(String, String)> {
        vec![("crime".to_string(), "CRIME: crime status | crime regs | crime irqlog [n] | crime touched".to_string())]
    }

    fn execute_command(&self, cmd: &str, args: &[&str], mut w: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        if cmd != "crime" {
            return Err("Command not found".to_string());
        }
        let reg = |off: u32| self.regs.lock().unwrap()[(off / 8) as usize];
        match args.first().copied().unwrap_or("status") {
            "status" => {
                let (stat, mask) = (self.intstat(), reg(crime_reg::INTMASK));
                writeln!(w, "CRIME revision {:#x}", self.revision).ok();
                writeln!(w, "  interrupts: status {stat:#010x} mask {mask:#010x} pending {:#010x} latched {:#010x} soft {:#010x}",
                    stat & mask, *self.latched.lock().unwrap(), reg(crime_reg::SOFTINT)).ok();
                writeln!(w, "  control {:#x} time {:#x}", reg(crime_reg::CONTROL), self.load_inner(crime_reg::TIME)).ok();
                for i in 0..8u32 {
                    writeln!(w, "  bank {i}: ctrl {:#06x}", reg(crime_reg::MEM_BANK_CTRL0 + 8 * i)).ok();
                }
            }
            "regs" => {
                for (i, &v) in self.regs.lock().unwrap().iter().enumerate() {
                    if v != 0 {
                        writeln!(w, "  {:#06x}: {v:#018x}", i * 8).ok();
                    }
                }
            }
            "irqlog" => {
                let n: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(32);
                let log = self.irqlog_text();
                let lines: Vec<&str> = log.lines().collect();
                for l in &lines[lines.len().saturating_sub(n)..] {
                    writeln!(w, "  {l}").ok();
                }
            }
            "touched" => {
                for (off, write) in self.touched() {
                    writeln!(w, "  {off:#06x} {}", if write { "W" } else { "R" }).ok();
                }
            }
            _ => return Err("Usage: crime status | regs | irqlog [n] | touched".to_string()),
        }
        Ok(())
    }
}

impl Resettable for Crime {
    /// Registers, latched interrupts and the counter's offset back to zero;
    /// the revision is the chip's and stays.
    fn power_on(&self) {
        self.regs.lock().unwrap().fill(0);
        *self.latched.lock().unwrap() = 0;
        *self.time_base.lock().unwrap() = 0;
        self.irqlog.lock().unwrap().clear();
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

//! Adaptec AIC-7880, emulated above its sequencer.
//!
//! The chip has almost no fixed behaviour. A driver downloads a sequencer
//! program into it and starts it, and everything the controller then does is
//! that program running — so there is no register-level contract to implement
//! in the way there is for, say, a UART.
//!
//! This models the level *above* that: the driver fills a SCB, hands its index
//! to the queue-in FIFO, and expects the index to come back on the queue-out
//! FIFO with status in the SCB and `CMDCMPLT` raised. We do the SCSI work
//! ourselves and post the result, and the downloaded sequencer program is
//! stored and never executed.
//!
//! The trade is explicit: this is a contract with what drivers *observe*
//! rather than with the hardware, so it has to be checked against each one.
//! The SCB layout in particular is defined by whichever sequencer program was
//! downloaded, not by the silicon, so it is discovered by watching what the
//! driver writes before it queues — see `docs/ip32-o2-bringup.md`.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Registers, at the offsets the chip presents them. The bridge's byte-lane
/// swap is applied before we get here, so these are device offsets.
pub mod reg {
    pub const SCSISEQ: u32 = 0x00;
    pub const SCSIID: u32 = 0x05;
    pub const SSTAT0: u32 = 0x0b;
    pub const SSTAT1: u32 = 0x0c;
    pub const SIMODE0: u32 = 0x10;
    pub const SIMODE1: u32 = 0x11;
    pub const SELID: u32 = 0x19;
    pub const SBLKCTL: u32 = 0x1f;
    /// Sequencer scratch RAM, where the driver keeps its configuration.
    pub const SRAM_FIRST: u32 = 0x20;
    pub const SRAM_LAST: u32 = 0x5f;
    pub const SEQCTL: u32 = 0x60;
    pub const SEQRAM: u32 = 0x61;
    pub const SEQADDR0: u32 = 0x62;
    pub const SEQADDR1: u32 = 0x63;
    pub const HCNTRL: u32 = 0x87;
    pub const HADDR: u32 = 0x88;
    pub const HCNT: u32 = 0x8c;
    pub const SCBPTR: u32 = 0x90;
    pub const INTSTAT: u32 = 0x91;
    /// Reads as the sequencer error register, written to clear interrupts.
    pub const ERROR_CLRINT: u32 = 0x92;
    pub const DFCNTRL: u32 = 0x93;
    pub const DFSTATUS: u32 = 0x94;
    pub const SCBCNT: u32 = 0x9a;
    pub const QINFIFO: u32 = 0x9b;
    pub const QINCNT: u32 = 0x9c;
    pub const QOUTFIFO: u32 = 0x9d;
    pub const QOUTCNT: u32 = 0x9e;
    /// The window onto the selected SCB.
    pub const SCBARRAY_FIRST: u32 = 0xa0;
    pub const SCBARRAY_LAST: u32 = 0xbf;
}

pub mod hcntrl {
    /// Reset the chip. Reads back as an acknowledgement.
    pub const CHIPRST: u8 = 0x01;
    pub const INTEN: u8 = 0x02;
    /// Stop the sequencer. The driver sets this before touching shared state.
    pub const PAUSE: u8 = 0x04;
    pub const IRQMS: u8 = 0x08;
    pub const SWINT: u8 = 0x10;
    pub const POWRDN: u8 = 0x40;
}

pub mod intstat {
    pub const SEQINT: u8 = 0x01;
    /// A command finished and its index is on the queue-out FIFO.
    pub const CMDCMPLT: u8 = 0x02;
    pub const SCSIINT: u8 = 0x04;
    pub const BRKADRINT: u8 = 0x08;
}

pub mod seqctl {
    /// Writes to SEQRAM load the sequencer program.
    pub const LOADRAM: u8 = 0x01;
    pub const SEQRESET: u8 = 0x02;
    pub const STEP: u8 = 0x04;
}

/// How many SCBs the chip has, and how big the window onto one is.
pub const SCB_COUNT: usize = 16;
pub const SCB_SIZE: usize = (reg::SCBARRAY_LAST - reg::SCBARRAY_FIRST + 1) as usize;

/// Everything the guest can see, plus a record of what it did.
struct State {
    regs: [u8; 0x100],
    scb: [[u8; SCB_SIZE]; SCB_COUNT],
    seqram: Vec<u8>,
    seqaddr: u16,
    qin: VecDeque<u8>,
    qout: VecDeque<u8>,
    intstat: u8,
    /// SCBs handed to the queue-in FIFO, with their contents as queued. This
    /// is the evidence for what the SCB layout actually is.
    queued: Vec<(u8, [u8; SCB_SIZE])>,
    notes: Vec<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            regs: [0; 0x100],
            scb: [[0; SCB_SIZE]; SCB_COUNT],
            seqram: Vec::new(),
            seqaddr: 0,
            qin: VecDeque::new(),
            qout: VecDeque::new(),
            intstat: 0,
            queued: Vec::new(),
            notes: Vec::new(),
        }
    }
}

pub struct Aic7880 {
    st: Mutex<State>,
}

impl Default for Aic7880 {
    fn default() -> Self {
        Self::new()
    }
}

impl Aic7880 {
    pub fn new() -> Self {
        Self { st: Mutex::new(State::default()) }
    }

    fn note(st: &mut State, what: String) {
        if st.notes.len() < 256 {
            st.notes.push(what);
        }
    }

    /// A running commentary on what the driver asked for.
    pub fn notes(&self) -> Vec<String> {
        self.st.lock().unwrap().notes.clone()
    }

    /// The sequencer program the driver downloaded. Never executed; its
    /// length is how we know the download happened at all.
    pub fn seqram_len(&self) -> usize {
        self.st.lock().unwrap().seqram.len()
    }

    /// Every SCB handed to the queue-in FIFO, as it looked when queued.
    pub fn queued(&self) -> Vec<(u8, [u8; SCB_SIZE])> {
        self.st.lock().unwrap().queued.clone()
    }

    fn scb_index(st: &State) -> usize {
        (st.regs[reg::SCBPTR as usize] as usize) % SCB_COUNT
    }

    fn read8(&self, off: u32) -> u8 {
        let mut st = self.st.lock().unwrap();
        match off {
            reg::INTSTAT => st.intstat,
            reg::QINCNT => st.qin.len() as u8,
            reg::QOUTCNT => st.qout.len() as u8,
            reg::QOUTFIFO => st.qout.pop_front().unwrap_or(0xff),
            reg::SEQRAM => {
                let a = st.seqaddr as usize;
                st.seqaddr = st.seqaddr.wrapping_add(1);
                st.seqram.get(a).copied().unwrap_or(0)
            }
            reg::SEQADDR0 => st.seqaddr as u8,
            reg::SEQADDR1 => (st.seqaddr >> 8) as u8,
            reg::SCBARRAY_FIRST..=reg::SCBARRAY_LAST => {
                let i = Self::scb_index(&st);
                st.scb[i][(off - reg::SCBARRAY_FIRST) as usize]
            }
            _ => st.regs[off as usize],
        }
    }

    fn write8(&self, off: u32, val: u8) {
        let mut st = self.st.lock().unwrap();
        match off {
            reg::HCNTRL => {
                if val & hcntrl::CHIPRST != 0 {
                    // A chip reset clears everything except the program we
                    // were given, which the driver reloads anyway.
                    let notes = std::mem::take(&mut st.notes);
                    *st = State { notes, ..Default::default() };
                    Self::note(&mut st, "chip reset".into());
                    // The reset bit reads back as an acknowledgement.
                    st.regs[reg::HCNTRL as usize] = hcntrl::CHIPRST;
                    return;
                }
                let was = st.regs[reg::HCNTRL as usize];
                st.regs[reg::HCNTRL as usize] = val;
                if was & hcntrl::PAUSE != val & hcntrl::PAUSE {
                    let s = if val & hcntrl::PAUSE != 0 { "paused" } else { "running" };
                    Self::note(&mut st, format!("sequencer {s}"));
                }
            }
            reg::SEQCTL => {
                let was = st.regs[reg::SEQCTL as usize];
                st.regs[off as usize] = val;
                if val & seqctl::SEQRESET != 0 {
                    st.seqaddr = 0;
                }
                // Only the rising edge starts a download. The driver writes
                // this register several times during one, and clearing the
                // program on each write throws the whole thing away.
                if val & seqctl::LOADRAM != 0 && was & seqctl::LOADRAM == 0 {
                    // LOADRAM only points SEQRAM accesses at the program
                    // store; it does not erase it. The driver sets it a
                    // second time to read the program back and check it, and
                    // clearing here would hand it zeros and fail the check.
                    st.seqaddr = 0;
                    Self::note(&mut st, "sequencer RAM selected".into());
                }
                if was & seqctl::LOADRAM != 0 && val & seqctl::LOADRAM == 0 {
                    let n = st.seqram.len();
                    Self::note(&mut st, format!("sequencer RAM deselected, {n} bytes held"));
                }
            }
            reg::SEQADDR0 => st.seqaddr = (st.seqaddr & 0xff00) | val as u16,
            reg::SEQADDR1 => st.seqaddr = (st.seqaddr & 0x00ff) | ((val as u16) << 8),
            reg::SEQRAM => {
                let a = st.seqaddr as usize;
                if st.seqram.len() <= a {
                    st.seqram.resize(a + 1, 0);
                }
                st.seqram[a] = val;
                st.seqaddr = st.seqaddr.wrapping_add(1);
            }
            reg::ERROR_CLRINT => {
                // Write-one-to-clear, sharing an address with the sequencer
                // error register that reads back.
                st.intstat &= !val;
            }
            reg::QINFIFO => {
                let i = (val as usize) % SCB_COUNT;
                let scb = st.scb[i];
                st.queued.push((val, scb));
                let cdb: Vec<String> = scb.iter().map(|b| format!("{b:02x}")).collect();
                Self::note(&mut st, format!("queued SCB {val}: {}", cdb.join(" ")));
                st.qin.push_back(val);
            }
            reg::SCBARRAY_FIRST..=reg::SCBARRAY_LAST => {
                let i = Self::scb_index(&st);
                st.scb[i][(off - reg::SCBARRAY_FIRST) as usize] = val;
            }
            _ => st.regs[off as usize] = val,
        }
    }
}

impl crate::ip32::PciDeviceOps for Aic7880 {
    fn read(&self, off: u32, width: usize) -> u32 {
        // Everything here is byte-wide; a wider access reads consecutive
        // registers, most significant first, the way the bus presents them.
        let mut v = 0u32;
        for i in 0..width as u32 {
            v = (v << 8) | self.read8(off + i) as u32;
        }
        v
    }

    fn write(&self, off: u32, val: u32, width: usize) {
        for i in 0..width as u32 {
            let shift = 8 * (width as u32 - 1 - i);
            self.write8(off + i, (val >> shift) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ip32::PciDeviceOps;

    #[test]
    fn a_chip_reset_acknowledges_itself() {
        let c = Aic7880::new();
        c.write(reg::HCNTRL, hcntrl::CHIPRST as u32, 1);
        assert_eq!(
            c.read(reg::HCNTRL, 1) as u8 & hcntrl::CHIPRST,
            hcntrl::CHIPRST,
            "the reset bit reads back as an acknowledgement"
        );
    }

    /// The driver selects an SCB and then reads and writes it through a fixed
    /// window. Getting this wrong silently corrupts every command.
    #[test]
    fn the_scb_window_follows_the_pointer() {
        let c = Aic7880::new();
        c.write(reg::SCBPTR, 3, 1);
        c.write(reg::SCBARRAY_FIRST + 4, 0xab, 1);
        c.write(reg::SCBPTR, 7, 1);
        assert_eq!(c.read(reg::SCBARRAY_FIRST + 4, 1), 0, "a different SCB is a different buffer");
        c.write(reg::SCBPTR, 3, 1);
        assert_eq!(c.read(reg::SCBARRAY_FIRST + 4, 1), 0xab, "and the first one kept its contents");
    }

    /// The download is how the driver installs the behaviour we are standing
    /// in for, so it has to be accepted and read back.
    #[test]
    fn the_sequencer_program_loads_and_reads_back() {
        let c = Aic7880::new();
        c.write(reg::SEQCTL, seqctl::LOADRAM as u32, 1);
        for (i, b) in [1u8, 2, 3, 4].iter().enumerate() {
            c.write(reg::SEQRAM, *b as u32, 1);
            assert_eq!(c.read(reg::SEQADDR0, 1), i as u32 + 1, "SEQRAM auto-increments");
        }
        assert_eq!(c.seqram_len(), 4);

        c.write(reg::SEQADDR0, 0, 1);
        c.write(reg::SEQADDR1, 0, 1);
        assert_eq!(c.read(reg::SEQRAM, 1), 1, "and reads back what was loaded");
    }

    /// Queueing captures the SCB as it stood, which is the evidence the
    /// layout is read off.
    #[test]
    fn queueing_an_scb_records_it() {
        let c = Aic7880::new();
        c.write(reg::SCBPTR, 2, 1);
        c.write(reg::SCBARRAY_FIRST, 0x12, 1);
        c.write(reg::QINFIFO, 2, 1);

        assert_eq!(c.read(reg::QINCNT, 1), 1, "the queue reports its depth");
        let q = c.queued();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, 2, "the index queued");
        assert_eq!(q[0].1[0], 0x12, "and the SCB as it was when queued");
    }

    #[test]
    fn interrupts_are_cleared_by_writing_their_bits() {
        let c = Aic7880::new();
        {
            let mut st = c.st.lock().unwrap();
            st.intstat = intstat::CMDCMPLT | intstat::SEQINT;
        }
        c.write(reg::ERROR_CLRINT, intstat::CMDCMPLT as u32, 1);
        assert_eq!(c.read(reg::INTSTAT, 1) as u8, intstat::SEQINT,
                   "clearing one interrupt must not clear the others");
    }
}

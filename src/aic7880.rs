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

/// Scratch RAM the downloaded program uses to find its work. These offsets
/// belong to that program, not to the chip, and were read out of the driver:
/// see `docs/ip32-o2-bringup.md`.
pub mod sram {
    /// Bus address of an array of SCB bus addresses, indexed by tag.
    pub const SCB_ARRAY: u32 = 0x3d;
    /// Bus address of the queue-out FIFO in host memory.
    pub const QOUTFIFO: u32 = 0x56;
}

/// SCSI block size. Everything SGI boots from uses this.
pub const BLOCK_SIZE: usize = 512;

/// A scatter-gather entry: a bus address and a length. The top byte of the
/// length carries flags we do not need, so it is masked off.
pub const SG_LEN_MASK: u32 = 0x00ff_ffff;
/// How many entries to follow before deciding the list is not a list.
///
/// This was 32, which is a plausible-looking number and was wrong. A single
/// 264-block read scatters into 33 pages, so the walk stopped one entry short
/// and dropped the tail — silently, because the command still reported
/// success. The symptom was a 5456-byte hole in a loaded image, which
/// happened to contain the entry point: the PROM loaded `sashARCS`, printed
/// the right sizes and the right entry, jumped, and executed whatever had
/// been in that memory before.
///
/// The bound only exists so a corrupt list cannot spin forever. A transfer
/// ends when the data runs out or an entry is null, both of which come first
/// in any healthy list.
pub const SG_MAX: usize = 4096;

/// A disk behind the controller.
pub struct ScsiDisk {
    file: Mutex<std::fs::File>,
    pub blocks: u64,
}

impl ScsiDisk {
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let blocks = file.metadata()?.len() / BLOCK_SIZE as u64;
        Ok(Self { file: Mutex::new(file), blocks })
    }

    fn write_blocks(&self, lba: u64, data: &[u8]) -> bool {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = self.file.lock().unwrap();
        // Refuse to write past the end rather than growing the image: a
        // disk that silently gets bigger is not a disk.
        let end = lba + (data.len() / BLOCK_SIZE) as u64;
        if end > self.blocks {
            return false;
        }
        f.seek(SeekFrom::Start(lba * BLOCK_SIZE as u64)).is_ok() && f.write_all(data).is_ok()
    }

    fn read_blocks(&self, lba: u64, count: usize) -> Vec<u8> {
        use std::io::{Read, Seek, SeekFrom};
        let mut buf = vec![0u8; count * BLOCK_SIZE];
        let mut f = self.file.lock().unwrap();
        if f.seek(SeekFrom::Start(lba * BLOCK_SIZE as u64)).is_ok() {
            // A short read at the end of the image leaves zeros, which is
            // what reading past the end of a real disk's data would give.
            let _ = f.read(&mut buf);
        }
        buf
    }
}

/// The host queue-out FIFO is 256 entries, filled with this until used.
pub const QOUTFIFO_LEN: u32 = 256;
pub const QOUTFIFO_EMPTY: u8 = 0xff;

/// Offsets within the 32-byte SCB that are known. The rest are still unread.
pub mod scb {
    pub const CONTROL: usize = 0;
    /// The length of the CDB, in bytes.
    ///
    /// This was first read as a target/lun byte, on the strength of the
    /// aic7xxx register named `SCB_TCL` sitting at the same offset. It is
    /// not: it holds `0x06` for INQUIRY, TEST UNIT READY, MODE SENSE and
    /// START STOP UNIT — every six-byte command — and `0x0a` for READ(10).
    /// Header names again, observed behaviour again, and the behaviour wins.
    pub const CDB_LEN: usize = 1;
    /// Bus address of the scatter-gather list.
    pub const SG_PTR: usize = 4;
    /// Bus address of the CDB.
    pub const CMD_PTR: usize = 8;
    /// Where the SCSI status is handed back. The driver pre-fills this with
    /// 0x40, which is not a valid status, so it is reading it expecting
    /// somebody to overwrite it.
    pub const TARGET_STATUS: usize = 2;
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
    /// Commands actually fetched and run: tag, SCB address, CDB.
    executed: Vec<(u8, u32, Vec<u8>)>,
    /// Where the next completion goes in the host queue-out FIFO.
    qoutpos: u32,
    /// How many times the sequencer was paused or restarted.
    pauses: u64,
    /// Per SCB: filled in through the register window since last queued.
    scb_onchip: [bool; SCB_COUNT],
    /// Totals that survive a chip reset, because the drivers reset often and
    /// per-reset counts answer the wrong question. "Was the whole file
    /// transferred" needs a number for the whole session.
    total_cmds: u64,
    total_in: u64,
    total_out: u64,
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
            executed: Vec::new(),
            qoutpos: 0,
            pauses: 0,
            scb_onchip: [false; SCB_COUNT],
            total_cmds: 0,
            total_in: 0,
            total_out: 0,
            notes: Vec::new(),
        }
    }
}

pub struct Aic7880 {
    st: Mutex<State>,
    disk: Mutex<Option<ScsiDisk>>,
    /// Host memory. The controller fetches its work from there rather than
    /// being handed it a register at a time, so without this it can see the
    /// tag of a command and nothing else about it.
    ram: std::sync::Arc<Mutex<Vec<u8>>>,
    /// An address range to report DMA writes for, from IRIS_IP32_DMAWATCH.
    /// "Did the loader ever write the entry point" is not answerable from the
    /// command log, because the log says where a transfer was aimed and not
    /// which bytes moved.
    dma_watch: Option<(u32, u32)>,
    dma_hits: Mutex<Vec<(u32, u8)>>,
}

impl Aic7880 {
    pub fn new(ram: std::sync::Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            st: Mutex::new(State::default()),
            disk: Mutex::new(None),
            ram,
            dma_watch: std::env::var("IRIS_IP32_DMAWATCH").ok().and_then(|v| {
                let (a, b) = v.split_once(':')?;
                let p = |x: &str| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok();
                Some((p(a)?, p(b)?))
            }),
            dma_hits: Mutex::new(Vec::new()),
        }
    }

    /// Offset into RAM for an address the device sees.
    ///
    /// Memory answers at its own base and again in a low alias, and the
    /// driver uses both in the same breath: the SCB's own address is given as
    /// `0x410752fc` while the CDB pointer inside it is `0x0107532c`, which is
    /// the same bytes. Resolving only one of them reads the SCB correctly and
    /// then finds an all-zero command.
    fn ram_offset(&self, addr: u32, len: usize) -> Option<usize> {
        let i = if addr >= crate::ip32::RAM_BASE {
            addr.wrapping_sub(crate::ip32::RAM_BASE) as usize
        } else {
            addr as usize
        };
        (i < len).then_some(i)
    }

    /// Read a byte of host memory, given the address the device sees.
    fn dma_read8(&self, addr: u32) -> u8 {
        let r = self.ram.lock().unwrap();
        match self.ram_offset(addr, r.len()) {
            Some(i) => r[i],
            None => 0,
        }
    }

    fn dma_write8(&self, addr: u32, val: u8) {
        if let Some((lo, hi)) = self.dma_watch {
            let a = addr & 0x1fff_ffff;
            if a >= lo && a < hi {
                let mut w = self.dma_hits.lock().unwrap();
                if w.len() < 64 {
                    w.push((addr, val));
                }
            }
        }
        let mut r = self.ram.lock().unwrap();
        if let Some(i) = self.ram_offset(addr, r.len()) {
            r[i] = val;
        }
    }

    /// DMA writes seen inside the watch range, as `(address, byte)`.
    pub fn dma_hits(&self) -> Vec<(u32, u8)> {
        self.dma_hits.lock().unwrap().clone()
    }

    /// A 32-bit word, in the order the host wrote it.
    ///
    /// The driver builds these with ordinary big-endian stores, and reading
    /// them any other way turns `0x410752fc` into `0xfc520741` and every
    /// pointer into nonsense. The chip's own scratch RAM is the opposite —
    /// written a register at a time, least significant first — so the two
    /// orders sit side by side and both are settled by observation.
    fn dma_read32(&self, addr: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..4 {
            v = (v << 8) | self.dma_read8(addr + i) as u32;
        }
        v
    }

    fn dma_read_bytes(&self, addr: u32, n: usize) -> Vec<u8> {
        (0..n as u32).map(|i| self.dma_read8(addr + i)).collect()
    }

    fn note(st: &mut State, what: String) {
        if st.notes.len() < 4096 {
            st.notes.push(what);
        }
    }

    /// A running commentary on what the driver asked for.
    pub fn notes(&self) -> Vec<String> {
        self.st.lock().unwrap().notes.clone()
    }

    /// Commands, bytes read and bytes written for the whole session.
    pub fn totals(&self) -> (u64, u64, u64) {
        let st = self.st.lock().unwrap();
        (st.total_cmds, st.total_in, st.total_out)
    }

    /// How many times the driver paused or restarted the sequencer.
    pub fn pauses(&self) -> u64 {
        self.st.lock().unwrap().pauses
    }

    /// The sequencer program the driver downloaded. Never executed; its
    /// length is how we know the download happened at all.
    pub fn seqram_len(&self) -> usize {
        self.st.lock().unwrap().seqram.len()
    }

    /// Every SCB handed to the queue-in FIFO, as it looked when queued.
    /// Commands the controller fetched and ran.
    pub fn executed(&self) -> Vec<(u8, u32, Vec<u8>)> {
        self.st.lock().unwrap().executed.clone()
    }

    pub fn queued(&self) -> Vec<(u8, [u8; SCB_SIZE])> {
        self.st.lock().unwrap().queued.clone()
    }

    /// A 32-bit value from the sequencer's scratch RAM, which the driver
    /// fills a register at a time, least significant byte first.
    fn scratch_le32(st: &State, off: u32) -> u32 {
        let b = |i: u32| st.regs[(off + i) as usize] as u32;
        b(0) | (b(1) << 8) | (b(2) << 16) | (b(3) << 24)
    }

    /// A CDB's length follows from its opcode group. Taking it from the
    /// opcode rather than from a field in the SCB means one less piece of the
    /// layout has to be guessed, and it is how SCSI defines it anyway.
    fn cdb_len(op: u8) -> usize {
        match op >> 5 {
            0 => 6,
            1 | 2 => 10,
            4 => 16,
            5 => 12,
            _ => 6,
        }
    }

    /// Attach a disk image.
    pub fn attach_disk(&self, path: &std::path::Path) -> std::io::Result<u64> {
        let d = ScsiDisk::open(path)?;
        let blocks = d.blocks;
        *self.disk.lock().unwrap() = Some(d);
        Ok(blocks)
    }

    /// Run a CDB against the attached disk. Returns the data to send back and
    /// a SCSI status byte.
    fn execute(&self, cdb: &[u8]) -> (Vec<u8>, u8) {
        const GOOD: u8 = 0x00;
        const CHECK_CONDITION: u8 = 0x02;
        let disk = self.disk.lock().unwrap();
        let Some(disk) = disk.as_ref() else {
            return (Vec::new(), CHECK_CONDITION);
        };
        let be16 = |i: usize| ((cdb[i] as usize) << 8) | cdb[i + 1] as usize;
        let be32 = |i: usize| {
            ((cdb[i] as u64) << 24) | ((cdb[i + 1] as u64) << 16)
                | ((cdb[i + 2] as u64) << 8) | cdb[i + 3] as u64
        };
        match cdb[0] {
            // TEST UNIT READY, START STOP UNIT, and the rest of the
            // no-data commands the PROM sends while probing.
            0x00 | 0x1b | 0x15 | 0x16 | 0x17 | 0x2f => (Vec::new(), GOOD),
            // INQUIRY.
            0x12 => {
                let mut d = vec![0u8; 36];
                d[0] = 0x00; // direct access device
                d[1] = 0x00; // not removable
                d[2] = 0x02; // SCSI-2
                d[3] = 0x02; // response format
                d[4] = 31; // additional length
                d[8..16].copy_from_slice(b"SGI     ");
                d[16..32].copy_from_slice(b"IRIS EMULATED   ");
                d[32..36].copy_from_slice(b"1.0 ");
                let want = cdb[4] as usize;
                d.truncate(want.min(d.len()));
                (d, GOOD)
            }
            // READ CAPACITY: last addressable block, then block size.
            0x25 => {
                let last = disk.blocks.saturating_sub(1) as u32;
                let mut d = Vec::with_capacity(8);
                d.extend_from_slice(&last.to_be_bytes());
                d.extend_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
                (d, GOOD)
            }
            // READ(6) and READ(10).
            0x08 => {
                let lba = (((cdb[1] & 0x1f) as u64) << 16)
                    | ((cdb[2] as u64) << 8) | cdb[3] as u64;
                let n = if cdb[4] == 0 { 256 } else { cdb[4] as usize };
                (disk.read_blocks(lba, n), GOOD)
            }
            0x28 => (disk.read_blocks(be32(2), be16(7)), GOOD),
            // MODE SENSE(6): an empty parameter header is enough to say
            // "no special modes" without claiming anything untrue.
            0x1a => (vec![3, 0, 0, 0], GOOD),
            // REQUEST SENSE: no error to report.
            0x03 => {
                let mut d = vec![0u8; 18];
                d[0] = 0x70; // current error, no sense
                d[7] = 10;
                (d, GOOD)
            }
            _ => (Vec::new(), CHECK_CONDITION),
        }
    }

    /// Collect `want` bytes out of the buffers the driver listed, for a
    /// command that sends data to the device.
    fn gather(&self, sg_ptr: u32, want: usize) -> (Vec<u8>, Vec<(u32, u32)>) {
        let mut entries = Vec::new();
        let mut out = Vec::with_capacity(want);
        for i in 0..SG_MAX {
            if out.len() >= want {
                break;
            }
            let a = self.dma_read32(sg_ptr + i as u32 * 8);
            let l = self.dma_read32(sg_ptr + i as u32 * 8 + 4) & SG_LEN_MASK;
            if a == 0 || l == 0 {
                break;
            }
            entries.push((a, l));
            let n = (l as usize).min(want - out.len());
            out.extend(self.dma_read_bytes(a, n));
        }
        (out, entries)
    }

    /// Copy data back into the buffers the driver listed. Returns how much
    /// was placed, and the list as read, for the trace.
    fn scatter(&self, sg_ptr: u32, data: &[u8]) -> (usize, Vec<(u32, u32)>) {
        let mut entries = Vec::new();
        let mut done = 0usize;
        for i in 0..SG_MAX {
            if done >= data.len() {
                break;
            }
            let a = self.dma_read32(sg_ptr + i as u32 * 8);
            let l = self.dma_read32(sg_ptr + i as u32 * 8 + 4) & SG_LEN_MASK;
            if a == 0 || l == 0 {
                break;
            }
            entries.push((a, l));
            let n = (l as usize).min(data.len() - done);
            for k in 0..n {
                self.dma_write8(a + k as u32, data[done + k]);
            }
            done += n;
        }
        (done, entries)
    }

    /// How many bytes a command sends *to* the device, if it is one that
    /// does. Everything else either receives data or carries none.
    fn data_out_len(cdb: &[u8]) -> Option<usize> {
        match cdb[0] {
            // WRITE(6): a zero count means 256 blocks, as with READ(6).
            0x0a => Some(if cdb[4] == 0 { 256 } else { cdb[4] as usize } * BLOCK_SIZE),
            // WRITE(10).
            0x2a => Some((((cdb[7] as usize) << 8) | cdb[8] as usize) * BLOCK_SIZE),
            _ => None,
        }
    }

    /// Run a command that carries data to the disk.
    fn execute_write(&self, cdb: &[u8], data: &[u8]) -> u8 {
        const GOOD: u8 = 0x00;
        const CHECK_CONDITION: u8 = 0x02;
        let disk = self.disk.lock().unwrap();
        let Some(disk) = disk.as_ref() else {
            return CHECK_CONDITION;
        };
        let lba = match cdb[0] {
            0x0a => (((cdb[1] & 0x1f) as u64) << 16) | ((cdb[2] as u64) << 8) | cdb[3] as u64,
            _ => {
                ((cdb[2] as u64) << 24) | ((cdb[3] as u64) << 16)
                    | ((cdb[4] as u64) << 8) | cdb[5] as u64
            }
        };
        if disk.write_blocks(lba, data) { GOOD } else { CHECK_CONDITION }
    }

    /// Fetch a queued command and run it.
    fn submit(&self, st: &mut State, tag: u8) {
        let i = (tag as usize) % SCB_COUNT;
        if st.scb_onchip[i] {
            // Written through the register window: the SCB is already here.
            // The layout is not the PROM's, and is not yet decoded -- record
            // it so the next reader has the evidence rather than a guess.
            let scb = st.scb[i];
            let hex: Vec<String> = scb.iter().map(|b| format!("{b:02x}")).collect();
            Self::note(st, format!("tag {tag}: on-chip SCB {}", hex.join(" ")));
            st.scb_onchip[i] = false;
            self.complete(st, tag);
            return;
        }
        let array = Self::scratch_le32(st, sram::SCB_ARRAY);
        if array == 0 {
            Self::note(st, format!("tag {tag} queued with no SCB array configured"));
            return;
        }
        let scb_addr = self.dma_read32(array + tag as u32 * 4);
        if scb_addr == 0 {
            Self::note(st, format!("tag {tag} has no SCB address in the array"));
            return;
        }
        let scb = self.dma_read_bytes(scb_addr, SCB_SIZE);
        let cmd_ptr = u32::from_be_bytes([
            scb[scb::CMD_PTR], scb[scb::CMD_PTR + 1],
            scb[scb::CMD_PTR + 2], scb[scb::CMD_PTR + 3],
        ]);
        // The SCB says how long the CDB is; fall back on the opcode group if
        // it says something impossible.
        let len = scb[scb::CDB_LEN] as usize;
        let len = if (6..=16).contains(&len) {
            len
        } else {
            Self::cdb_len(self.dma_read8(cmd_ptr))
        };
        let cdb = self.dma_read_bytes(cmd_ptr, len);

        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ");
        Self::note(st, format!(
            "tag {tag}: SCB at 0x{scb_addr:08x} control 0x{:02x} cdblen {}",
            scb[scb::CONTROL], len));
        Self::note(st, format!("   CDB {}", hex(&cdb)));
        Self::note(st, format!("   SCB {}", hex(&scb)));
        let sg_ptr = u32::from_be_bytes([
            scb[scb::SG_PTR], scb[scb::SG_PTR + 1],
            scb[scb::SG_PTR + 2], scb[scb::SG_PTR + 3],
        ]);
        let (status, moved, sg, dir) = match Self::data_out_len(&cdb) {
            Some(want) => {
                let (data, sg) = self.gather(sg_ptr, want);
                let n = data.len();
                (self.execute_write(&cdb, &data), n, sg, "out of")
            }
            None => {
                let (data, status) = self.execute(&cdb);
                let (sent, sg) = if data.is_empty() {
                    (0, Vec::new())
                } else {
                    self.scatter(sg_ptr, &data)
                };
                (status, sent, sg, "into")
            }
        };
        let sgtxt: Vec<String> = sg.iter()
            .map(|(a, l)| format!("0x{a:08x}+{l}")).collect();
        // Hand the status back where the driver left room for it.
        self.dma_write8(scb_addr + scb::TARGET_STATUS as u32, status);
        st.total_cmds += 1;
        if dir == "into" { st.total_in += moved as u64 } else { st.total_out += moved as u64 }
        Self::note(st, format!(
            "   -> status {status}, {moved} bytes {dir} [{}]", sgtxt.join(" ")));
        st.executed.push((tag, scb_addr, cdb));

        self.complete(st, tag);
    }

    /// Post a finished tag where the driver looks for it.
    fn complete(&self, st: &mut State, tag: u8) {
        let fifo = Self::scratch_le32(st, sram::QOUTFIFO);
        if fifo != 0 {
            self.dma_write8(fifo + st.qoutpos, tag);
            st.qoutpos = (st.qoutpos + 1) % QOUTFIFO_LEN;
        }
        st.qout.push_back(tag);
        st.intstat |= intstat::CMDCMPLT;
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
                    let (c, i, o) = (st.total_cmds, st.total_in, st.total_out);
                    *st = State {
                        notes,
                        total_cmds: c,
                        total_in: i,
                        total_out: o,
                        ..Default::default()
                    };
                    Self::note(&mut st, "chip reset".into());
                    // The reset bit reads back as an acknowledgement.
                    st.regs[reg::HCNTRL as usize] = hcntrl::CHIPRST;
                    return;
                }
                let was = st.regs[reg::HCNTRL as usize];
                st.regs[reg::HCNTRL as usize] = val;
                // The driver pauses and restarts the sequencer around every
                // shared-state access, thousands of times. Counting them says
                // as much as logging them and leaves room for the commands.
                if was & hcntrl::PAUSE != val & hcntrl::PAUSE {
                    st.pauses += 1;
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
                st.qin.push_back(val);
                self.submit(&mut st, val);
            }
            reg::SCBARRAY_FIRST..=reg::SCBARRAY_LAST => {
                let i = Self::scb_index(&st);
                st.scb[i][(off - reg::SCBARRAY_FIRST) as usize] = val;
                // Remember that this SCB was filled in on the chip. Two
                // drivers use two different submission paths: the PROM's
                // sequencer program fetches SCBs from host memory and is
                // handed only a tag, while NetBSD's writes the whole SCB
                // through this window first. Which one is in use is decided
                // by what the driver actually did, not by guessing.
                st.scb_onchip[i] = true;
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

    fn chip() -> Aic7880 {
        Aic7880::new(std::sync::Arc::new(std::sync::Mutex::new(vec![0u8; 1 << 20])))
    }

    #[test]
    fn a_chip_reset_acknowledges_itself() {
        let c = chip();
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
        let c = chip();
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
        let c = chip();
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
        let c = chip();
        c.write(reg::SCBPTR, 2, 1);
        c.write(reg::SCBARRAY_FIRST, 0x12, 1);
        c.write(reg::QINFIFO, 2, 1);

        assert_eq!(c.read(reg::QINCNT, 1), 1, "the queue reports its depth");
        let q = c.queued();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, 2, "the index queued");
        assert_eq!(q[0].1[0], 0x12, "and the SCB as it was when queued");
    }

    /// The whole submission path, built the way the driver builds it: a SCB
    /// in host memory, its address in an array the chip was told about, and
    /// only a tag handed to the chip. Every step of this was read out of the
    /// driver rather than a datasheet, so it is worth pinning down.
    #[test]
    fn a_queued_tag_fetches_the_command_and_answers_it() {
        let ram = std::sync::Arc::new(std::sync::Mutex::new(vec![0u8; 1 << 20]));
        let c = Aic7880::new(ram.clone());

        // A controller with nothing attached answers nothing, so the test
        // needs a disk as much as the machine does.
        let img = std::env::temp_dir().join("iris-aic7880-test.img");
        std::fs::write(&img, vec![0u8; 64 * BLOCK_SIZE]).expect("test image");
        c.attach_disk(&img).expect("attach");

        let base = crate::ip32::RAM_BASE;
        let (array, scb_addr, cdb_addr, sg_addr, buf, qout) =
            (0x1000u32, 0x2000u32, 0x2100u32, 0x2200u32, 0x3000u32, 0x4000u32);
        {
            let mut m = ram.lock().unwrap();
            let put32 = |m: &mut Vec<u8>, at: u32, v: u32| {
                m[at as usize..at as usize + 4].copy_from_slice(&v.to_be_bytes());
            };
            // array[1] = the SCB's bus address
            put32(&mut m, array + 4, base + scb_addr);
            // SCB: pointers to the scatter-gather list and the CDB
            put32(&mut m, scb_addr + scb::SG_PTR as u32, base + sg_addr);
            put32(&mut m, scb_addr + scb::CMD_PTR as u32, base + cdb_addr);
            // The driver leaves this filled with a value that is not a status.
            m[(scb_addr + scb::TARGET_STATUS as u32) as usize] = 0x40;
            // INQUIRY, 64 bytes
            m[cdb_addr as usize] = 0x12;
            m[cdb_addr as usize + 4] = 0x40;
            // One scatter-gather entry
            put32(&mut m, sg_addr, base + buf);
            put32(&mut m, sg_addr + 4, 64);
        }
        // Tell the chip where the array and the completion FIFO are, the way
        // the driver does: a scratch register at a time, little end first.
        for (i, b) in (base + array).to_le_bytes().iter().enumerate() {
            c.write(sram::SCB_ARRAY + i as u32, *b as u32, 1);
        }
        for (i, b) in (base + qout).to_le_bytes().iter().enumerate() {
            c.write(sram::QOUTFIFO + i as u32, *b as u32, 1);
        }

        c.write(reg::QINFIFO, 1, 1);

        let m = ram.lock().unwrap();
        assert_eq!(m[buf as usize], 0x00, "a direct access device");
        assert_eq!(&m[buf as usize + 8..buf as usize + 11], b"SGI",
                   "the inquiry data must land in the buffer the driver listed");
        assert_eq!(m[(scb_addr + scb::TARGET_STATUS as u32) as usize], 0,
                   "and the status must replace the value the driver left");
        assert_eq!(m[qout as usize], 1, "the tag goes into the host completion FIFO");
        drop(m);
        assert_eq!(c.read(reg::INTSTAT, 1) as u8 & intstat::CMDCMPLT, intstat::CMDCMPLT,
                   "and the driver is told to look");
    }

    /// A write goes the other way through the same scatter-gather list, and
    /// must reach the image rather than growing it.
    #[test]
    fn a_write_reaches_the_disk_and_reads_back() {
        let ram = std::sync::Arc::new(std::sync::Mutex::new(vec![0u8; 1 << 20]));
        let c = Aic7880::new(ram.clone());
        let img = std::env::temp_dir().join("iris-aic7880-write.img");
        std::fs::write(&img, vec![0u8; 64 * BLOCK_SIZE]).expect("test image");
        c.attach_disk(&img).expect("attach");

        let base = crate::ip32::RAM_BASE;
        let (array, scb, cdb, sg, buf, qout) =
            (0x1000u32, 0x2000u32, 0x2100u32, 0x2200u32, 0x3000u32, 0x4000u32);
        let pattern: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
        {
            let mut m = ram.lock().unwrap();
            let put32 = |m: &mut Vec<u8>, at: u32, v: u32| {
                m[at as usize..at as usize + 4].copy_from_slice(&v.to_be_bytes());
            };
            put32(&mut m, array + 4, base + scb);
            put32(&mut m, scb + scb::SG_PTR as u32, base + sg);
            put32(&mut m, scb + scb::CMD_PTR as u32, base + cdb);
            m[(scb + scb::CDB_LEN as u32) as usize] = 10;
            // WRITE(10), one block at LBA 5.
            m[cdb as usize] = 0x2a;
            m[cdb as usize + 5] = 5;
            m[cdb as usize + 8] = 1;
            put32(&mut m, sg, base + buf);
            put32(&mut m, sg + 4, BLOCK_SIZE as u32);
            m[buf as usize..buf as usize + BLOCK_SIZE].copy_from_slice(&pattern);
        }
        for (i, b) in (base + array).to_le_bytes().iter().enumerate() {
            c.write(sram::SCB_ARRAY + i as u32, *b as u32, 1);
        }
        for (i, b) in (base + qout).to_le_bytes().iter().enumerate() {
            c.write(sram::QOUTFIFO + i as u32, *b as u32, 1);
        }
        c.write(reg::QINFIFO, 1, 1);

        assert_eq!(ram.lock().unwrap()[(scb + scb::TARGET_STATUS as u32) as usize], 0,
                   "the write must report success");
        let on_disk = std::fs::read(&img).expect("read back");
        assert_eq!(&on_disk[5 * BLOCK_SIZE..6 * BLOCK_SIZE], &pattern[..],
                   "the block must land at the address the CDB named");
        assert!(on_disk[4 * BLOCK_SIZE..5 * BLOCK_SIZE].iter().all(|b| *b == 0),
                "and must not disturb its neighbour");
        assert_eq!(on_disk.len(), 64 * BLOCK_SIZE, "the image must not grow");
    }

    /// A transfer larger than any plausible fixed bound must still complete.
    /// A 32-entry cap looked reasonable and silently truncated a 33-page
    /// read, reporting success and leaving a hole in the middle of a loaded
    /// program.
    #[test]
    fn a_long_scatter_gather_list_is_followed_to_the_end() {
        let ram = std::sync::Arc::new(std::sync::Mutex::new(vec![0u8; 4 << 20]));
        let c = Aic7880::new(ram.clone());
        let img = std::env::temp_dir().join("iris-aic7880-sg.img");
        let pattern: Vec<u8> = (0..264 * BLOCK_SIZE).map(|i| (i % 253) as u8).collect();
        std::fs::write(&img, &pattern).expect("test image");
        c.attach_disk(&img).expect("attach");

        let base = crate::ip32::RAM_BASE;
        let (array, scb, cdb, sg, buf, qout) =
            (0x1000u32, 0x2000u32, 0x2100u32, 0x4000u32, 0x100000u32, 0x3000u32);
        // 264 blocks into 4 KiB pages: 33 entries, one past the old bound.
        let pages = 264 * BLOCK_SIZE / 4096;
        assert!(pages > 32, "the test must cross the bound it is checking");
        {
            let mut m = ram.lock().unwrap();
            let put32 = |m: &mut Vec<u8>, at: u32, v: u32| {
                m[at as usize..at as usize + 4].copy_from_slice(&v.to_be_bytes());
            };
            put32(&mut m, array + 4, base + scb);
            put32(&mut m, scb + scb::SG_PTR as u32, base + sg);
            put32(&mut m, scb + scb::CMD_PTR as u32, base + cdb);
            m[(scb + scb::CDB_LEN as u32) as usize] = 10;
            // READ(10), 264 blocks from LBA 0.
            m[cdb as usize] = 0x28;
            m[cdb as usize + 7] = (264 >> 8) as u8;
            m[cdb as usize + 8] = (264 & 0xff) as u8;
            for i in 0..pages as u32 {
                put32(&mut m, sg + i * 8, base + buf + i * 4096);
                put32(&mut m, sg + i * 8 + 4, 4096);
            }
        }
        for (i, b) in (base + array).to_le_bytes().iter().enumerate() {
            c.write(sram::SCB_ARRAY + i as u32, *b as u32, 1);
        }
        for (i, b) in (base + qout).to_le_bytes().iter().enumerate() {
            c.write(sram::QOUTFIFO + i as u32, *b as u32, 1);
        }
        c.write(reg::QINFIFO, 1, 1);

        let m = ram.lock().unwrap();
        let got = &m[buf as usize..buf as usize + pattern.len()];
        assert_eq!(got, &pattern[..], "every page of the transfer must arrive");
    }

    /// Writing past the end is an error, not a bigger disk.
    #[test]
    fn a_write_past_the_end_fails_rather_than_growing_the_image() {
        let img = std::env::temp_dir().join("iris-aic7880-grow.img");
        std::fs::write(&img, vec![0u8; 8 * BLOCK_SIZE]).expect("test image");
        let d = ScsiDisk::open(&img).expect("open");
        assert!(!d.write_blocks(7, &vec![0xab; 4 * BLOCK_SIZE]), "refused");
        assert_eq!(std::fs::metadata(&img).unwrap().len(), (8 * BLOCK_SIZE) as u64);
    }

    /// Memory answers both at its own base and in a low alias, and the driver
    /// uses both in one SCB. Resolving only one reads the SCB and then finds
    /// an all-zero command.
    #[test]
    fn dma_resolves_the_low_memory_alias() {
        let ram = std::sync::Arc::new(std::sync::Mutex::new(vec![0u8; 1 << 16]));
        ram.lock().unwrap()[0x40] = 0xa5;
        let c = Aic7880::new(ram);
        assert_eq!(c.dma_read8(0x40), 0xa5, "the low alias");
        assert_eq!(c.dma_read8(crate::ip32::RAM_BASE + 0x40), 0xa5, "and memory's own base");
    }

    #[test]
    fn interrupts_are_cleared_by_writing_their_bits() {
        let c = chip();
        {
            let mut st = c.st.lock().unwrap();
            st.intstat = intstat::CMDCMPLT | intstat::SEQINT;
        }
        c.write(reg::ERROR_CLRINT, intstat::CMDCMPLT as u32, 1);
        assert_eq!(c.read(reg::INTSTAT, 1) as u8, intstat::SEQINT,
                   "clearing one interrupt must not clear the others");
    }
}

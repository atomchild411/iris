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
//!
//! Three contracts are known. SGI's PROM and the IRIX kernel both carry
//! Adaptec's "HIM" and its sequencer program, which hand the chip a tag
//! through `QINFIFO` and take completions from a byte array in host memory;
//! errors come back as pausing interrupts the HIM then sorts out itself.
//! NetBSD and OpenBSD ring a doorbell in scratch RAM instead. Which one is
//! in force is read from scratch RAM at each doorbell, never latched,
//! because one boot can hand the chip from one driver to the next.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Registers, at the offsets the chip presents them. The bridge's byte-lane
/// swap is applied before we get here, so these are device offsets.
pub mod reg {
    pub const SCSISEQ: u32 = 0x00;
    /// The bus's control lines. Bit 1 is REQ.
    pub const SCSISIG: u32 = 0x03;
    pub const SCSIID: u32 = 0x05;
    /// Status that latches until cleared: reads are SSTAT0/SSTAT1, writes
    /// to the same addresses are CLRSINT0/CLRSINT1.
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
    /// The interrupts that stop the sequencer until the host restarts it.
    /// A completion is the one that does not.
    pub const PAUSING: u8 = SEQINT | SCSIINT | BRKADRINT;
    /// What a `SEQINT` is about. Only meaningful while `SEQINT` is set.
    pub const INTCODE: u8 = 0xf0;
}

pub mod sstat1 {
    /// Selection timed out: nobody answered at that id.
    pub const SELTO: u8 = 0x80;
}

/// Bus control lines, as `SCSISIG` reads them.
pub mod scsisig {
    /// The target is asking for a byte. Nothing here ever does, and the HIM
    /// waits for this to drop before it carries on after an error.
    pub const REQ: u8 = 0x02;
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
    /// Bus address of the queue-out array in host memory. Under the HIM its
    /// low byte is also the sequencer's write cursor into that array.
    pub const QOUTFIFO: u32 = 0x56;
}

/// The HIM's side of scratch RAM, shared by SGI's PROM and the IRIX kernel.
pub mod him {
    /// Two placeholder pointers the HIM writes and neither of its sequencer
    /// programs ever reads, little end first. Nothing else puts these
    /// values there, so finding both is how we know the HIM is driving.
    pub const SIGNATURE: [(u32, u32); 2] = [(0x42, 0x0bad_0001), (0x5a, 0x0bad_0003)];
    /// The tag being selected, which is the one a selection timeout is for.
    pub const WAITING_SCB: u32 = 0x3a;
    /// The tag connected, which is the one a check condition is for.
    pub const ACTIVE_SCB: u32 = 0x3b;
    /// The SCSI status a check-condition interrupt hands over.
    pub const STATUS: u32 = 0x3c;
    /// `SEQINT` code: the target ended with a status other than GOOD.
    pub const BAD_STATUS: u8 = 0x50;
    /// Length of the PROM's build of the sequencer program. The kernel's is
    /// 1708 bytes. The PROM boots disks and CD-ROMs here with every error
    /// answered as success, so it keeps those answers, and the selection
    /// timeout and check-condition interrupts go only to other programs.
    /// That holds until a PROM boot has been seen to take them.
    pub const PROM_PROGRAM_LEN: usize = 1696;
}

/// SCSI block size. Everything SGI boots from uses this.
pub const BLOCK_SIZE: usize = 512;

/// A scatter-gather entry: a bus address and a length. The top byte of the
/// length carries flags, so it is masked off.
pub const SG_LEN_MASK: u32 = 0x00ff_ffff;
/// The one flag in that byte we use: this entry is the last.
pub const SG_LAST: u32 = 0x8000_0000;
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
    /// Presented as a CD-ROM rather than a fixed disk.
    ///
    /// SGI's PROM greys out "Local CD-ROM" on its install menu unless
    /// something on the bus says it is one, and the install media is the only
    /// way in without a network.
    pub cdrom: bool,
    /// Bytes per logical block. A disk is 512. A CD-ROM drive starts at 2048,
    /// as real ones do, and takes whatever a MODE SELECT block descriptor
    /// asks for: SGI's PROM switches it to 512 (which is how an SGI CD's
    /// volume header reads with disk arithmetic), while OpenBSD's driver
    /// leaves it at 2048 and cannot read the disc at 512.
    block_len: std::sync::atomic::AtomicUsize,
    /// Size of the image in bytes.
    bytes: u64,
}

impl ScsiDisk {
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        Self::open_as(path, false)
    }

    pub fn open_as(path: &std::path::Path, cdrom: bool) -> std::io::Result<Self> {
        // Read-only media must be opened read-only: a write that silently
        // succeeded on an install CD would be worse than one that failed.
        let file = if cdrom {
            std::fs::OpenOptions::new().read(true).open(path)?
        } else {
            std::fs::OpenOptions::new().read(true).write(true).open(path)?
        };
        let bytes = file.metadata()?.len();
        let blocks = bytes / BLOCK_SIZE as u64;
        let block_len = std::sync::atomic::AtomicUsize::new(if cdrom { 2048 } else { BLOCK_SIZE });
        Ok(Self { file: Mutex::new(file), blocks, cdrom, block_len, bytes })
    }

    /// Bytes per logical block, as the drive is set now.
    pub fn block_len(&self) -> usize {
        self.block_len.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Logical blocks at the current block length.
    pub fn capacity(&self) -> u64 {
        self.bytes / self.block_len() as u64
    }

    /// Back to the power-on block length, as a bus reset leaves a device.
    fn power_on_block_len(&self) {
        let len = if self.cdrom { 2048 } else { BLOCK_SIZE };
        self.block_len.store(len, std::sync::atomic::Ordering::Relaxed);
    }

    /// A MODE SELECT's block descriptor: a CD-ROM takes the new length.
    fn set_block_len(&self, len: usize) -> bool {
        if !self.cdrom || !matches!(len, 512 | 1024 | 2048) {
            return false;
        }
        self.block_len.store(len, std::sync::atomic::Ordering::Relaxed);
        true
    }

    fn write_blocks(&self, lba: u64, data: &[u8]) -> bool {
        use std::io::{Seek, SeekFrom, Write};
        if self.cdrom {
            return false;
        }
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
        let bl = self.block_len();
        let mut buf = vec![0u8; count * bl];
        let mut f = self.file.lock().unwrap();
        if f.seek(SeekFrom::Start(lba * bl as u64)).is_ok() {
            // A short read at the end of the image leaves zeros, which is
            // what reading past the end of a real disk's data would give.
            let _ = f.read(&mut buf);
        }
        buf
    }
}

/// NetBSD's sequencer program uses a different arrangement from the PROM's,
/// and these names come from its own headers rather than from guessing.
///
/// Its host does not write a tag to `QINFIFO` at all. It puts the tag into a
/// 256-entry ring in host memory and then writes the *producer index* to a
/// scratch register; the sequencer consumes from the ring and fetches the SCB
/// from a flat array indexed by tag. Completion goes back through a second
/// ring beside the first.
pub mod nb {
    /// Scratch registers holding the two base addresses and the index.
    pub const HSCB_ADDR: u32 = 0x44;
    pub const SHARED_DATA_ADDR: u32 = 0x48;
    pub const KERNEL_QINPOS: u32 = 0x4c;

    /// `qoutfifo` sits at the shared-data base and `qinfifo` right after it,
    /// 256 bytes each.
    pub const FIFO_LEN: u32 = 256;
    /// Stride of the host's SCB array. The chip only looks at the first 32
    /// bytes; the rest is room for a long CDB.
    pub const HSCB_STRIDE: u32 = 64;

    /// Offsets inside one SCB.
    pub const CDB: u32 = 0;
    /// Status is returned in the same union the CDB arrived in, after two
    /// residual words.
    pub const STATUS: u32 = 8;
    pub const DATAPTR: u32 = 12;
    pub const DATACNT: u32 = 16;
    pub const SGPTR: u32 = 20;
    pub const CONTROL: u32 = 24;
    pub const SCSIID: u32 = 25;
    pub const LUN: u32 = 26;
    pub const CDB_LEN: u32 = 28;

    /// A scatter-gather entry's length carries flags in its top byte.
    pub const LAST_SEG: u32 = 0x8000_0000;
    pub const SG_PTR_MASK: u32 = 0xffff_fff8;
}

/// The host queue-out FIFO is 256 entries, filled with this until used.
pub const QOUTFIFO_LEN: u32 = 256;
pub const QOUTFIFO_EMPTY: u8 = 0xff;

/// Offsets within the 32-byte SCB, as host memory holds it. The HIM builds it
/// with 32-bit stores, so each word's bytes are the chip's in reverse.
pub mod scb {
    /// How many scatter-gather entries follow. Never zero: a command with
    /// no data still lists one empty entry.
    pub const SEG_COUNT: usize = 0;
    /// The length of the CDB, in bytes.
    ///
    /// This was first read as a target/lun byte, on the strength of the
    /// aic7xxx register named `SCB_TCL` sitting at the same offset. It is
    /// not: it holds `0x06` for INQUIRY, TEST UNIT READY, MODE SENSE and
    /// START STOP UNIT — every six-byte command — and `0x0a` for READ(10).
    /// Header names again, observed behaviour again, and the behaviour wins.
    pub const CDB_LEN: usize = 1;
    /// Control flags: disconnect allowed, tagged, and the like. Belongs to
    /// the driver; nothing here writes it.
    pub const CONTROL: usize = 2;
    /// Bus address of the scatter-gather list.
    pub const SG_PTR: usize = 4;
    /// Bus address of the CDB.
    pub const CMD_PTR: usize = 8;
    /// Target in bits 7..4, channel in bit 3, LUN in bits 2..0 -- the same
    /// shape as aic7xxx's `SCB_TCL`.
    ///
    /// Read as `tag << 4` at first, which fitted perfectly while the PROM was
    /// scanning targets 1, 2 and 3 with tags 1, 2 and 3. Two disks at
    /// different ids in one session separate them: asking for `dksc(0,1,8)`
    /// and then `dksc(0,4,8)` produces `0x10` and `0x40` with the same tag.
    pub const TCL: usize = 3;
    /// The target's SCSI status. The host writes zero and the HIM fills it
    /// in itself after a check-condition interrupt; the sequencer never
    /// writes the SCB back. Byte 2 was taken for this once, because the
    /// driver leaves 0x40 there, but that is "disconnect allowed".
    pub const TARGET_STATUS: usize = 15;
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
    /// Tags waiting for the sequencer. It takes them only while running, so
    /// a tag written while paused stays here, where the host can still
    /// count, pop and rotate it.
    qin: VecDeque<u8>,
    qout: VecDeque<u8>,
    intstat: u8,
    /// SCSI status that latches until the host clears it, as SSTAT0 and
    /// SSTAT1 read. Kept apart from `regs` because the same addresses
    /// written are CLRSINT0 and CLRSINT1: storing the write there made a
    /// channel reset's "clear everything" read back as "everything
    /// happened", external bus reset included.
    sstat0: u8,
    sstat1: u8,
    /// SCBs handed to the queue-in FIFO, with their contents as queued. This
    /// is the evidence for what the SCB layout actually is.
    queued: Vec<(u8, [u8; SCB_SIZE])>,
    /// Commands actually fetched and run: tag, SCB address, CDB.
    executed: Vec<(u8, u32, Vec<u8>)>,
    /// Where the next completion goes in the host queue-out FIFO.
    qoutpos: u32,
    /// How far we have consumed NetBSD's queue-in ring.
    qinpos: u8,
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
            sstat0: 0,
            sstat1: 0,
            queued: Vec::new(),
            executed: Vec::new(),
            qoutpos: 0,
            qinpos: 0,
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
    /// Disks by SCSI id. An install needs at least two -- the media to read
    /// and the disk to write -- so one is not enough.
    disks: Mutex<std::collections::BTreeMap<u8, ScsiDisk>>,
    /// Host memory. The controller fetches its work from there rather than
    /// being handed it a register at a time, so without this it can see the
    /// tag of a command and nothing else about it.
    ram: std::sync::Arc<Mutex<crate::dev::crime_mem::O2Memory>>,
    /// The SCSI id the one disk answers at.
    target: u8,
    /// An address range to report DMA writes for, from IRIS_IP32_DMAWATCH.
    /// "Did the loader ever write the entry point" is not answerable from the
    /// command log, because the log says where a transfer was aimed and not
    /// which bytes moved.
    dma_watch: Option<(u32, u32)>,
    dma_hits: Mutex<Vec<(u32, u8)>>,
    /// Sense data for the last command that failed, as (key, ASC, ASCQ),
    /// handed out by the next REQUEST SENSE. A check condition followed by
    /// "no sense" tells a driver nothing went wrong, and it gives up on a
    /// unit it cannot understand rather than skipping one command.
    sense: Mutex<Option<(u8, u8, u8)>>,
    /// Opcodes already reported as unsupported, so each is logged once.
    unsupported: Mutex<std::collections::BTreeSet<u8>>,
    /// Pages of DMA addresses already reported as not memory.
    dma_misses: Mutex<std::collections::BTreeSet<u32>>,
}

impl Aic7880 {
    pub fn new(ram: std::sync::Arc<Mutex<crate::dev::crime_mem::O2Memory>>) -> Self {
        Self {
            st: Mutex::new(State::default()),
            disks: Mutex::new(std::collections::BTreeMap::new()),
            ram,
            dma_watch: std::env::var("IRIS_IP32_DMAWATCH").ok().and_then(|v| {
                let (a, b) = v.split_once(':')?;
                let p = |x: &str| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok();
                Some((p(a)?, p(b)?))
            }),
            dma_hits: Mutex::new(Vec::new()),
            sense: Mutex::new(None),
            unsupported: Mutex::new(std::collections::BTreeSet::new()),
            dma_misses: Mutex::new(std::collections::BTreeSet::new()),
            target: std::env::var("IRIS_IP32_SCSI_ID").ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
        }
    }

    /// Memory answers at its own base and again in a low alias, and the
    /// driver uses both in the same breath: the SCB's own address is given as
    /// `0x410752fc` while the CDB pointer inside it is `0x0107532c`, which is
    /// the same bytes. The memory's own decode covers both.
    /// Read a byte of host memory, given the address the device sees.
    fn dma_read8(&self, addr: u32) -> u8 {
        let r = self.ram.lock().unwrap();
        match r.dma_offset(addr) {
            Some(i) => r[i],
            None => {
                drop(r);
                self.dma_miss(addr, false);
                0
            }
        }
    }

    /// A DMA address that is not memory: report each 4 KB page once (the
    /// first 64), since a driver handing the chip bus addresses we decode
    /// differently loses data silently otherwise.
    fn dma_miss(&self, addr: u32, write: bool) {
        let mut seen = self.dma_misses.lock().unwrap();
        if seen.len() < 64 && seen.insert(addr & !0xfff) {
            eprintln!("aic7880: DMA {} {addr:#010x} is not memory", if write { "write to" } else { "read from" });
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
        if let Some(i) = r.dma_offset(addr) {
            r.write(i, &[val]);
        } else {
            drop(r);
            self.dma_miss(addr, true);
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

    /// A 32-bit word the little-endian way.
    ///
    /// The chip is little-endian and NetBSD's driver writes its SCB fields
    /// accordingly (`ahc_htole32`), while the PROM's sequencer program was
    /// handed big-endian words by its own loader. Reading NetBSD's pointers
    /// the wrong way round turns 0x0005fe58 into 0x58fe0500 -- an address
    /// far outside memory, which lands nowhere and reports success.
    fn dma_read32_le(&self, addr: u32) -> u32 {
        let mut v = 0u32;
        for i in (0..4).rev() {
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

    /// True while the chip is asserting its interrupt line. Only the four
    /// interrupt bits count: the code bits above them say what a `SEQINT`
    /// is about and are not an interrupt of their own.
    pub fn interrupting(&self) -> bool {
        self.st.lock().unwrap().intstat & 0x0f != 0
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

    /// Whether the HIM is driving: its placeholder pointers are where it
    /// leaves them. Read fresh each time, because a NetBSD boot after the
    /// PROM rewrites the same scratch bytes and must fall out of it.
    fn him(st: &State) -> bool {
        him::SIGNATURE.iter().all(|(off, v)| Self::scratch_le32(st, *off) == *v)
    }

    /// Whether errors go back the way the HIM's own sequencer reports them:
    /// as pausing interrupts, with nothing on the queue-out. Everything but
    /// the PROM's build of the program; see `him::PROM_PROGRAM_LEN`.
    fn him_errors(st: &State) -> bool {
        Self::him(st) && st.seqram.len() != him::PROM_PROGRAM_LEN
    }

    /// Whether the sequencer is stopped. The host can stop it, and so does
    /// any interrupt that needs the host to look before it goes on.
    fn paused(st: &State) -> bool {
        st.regs[reg::HCNTRL as usize] & hcntrl::PAUSE != 0 || st.intstat & intstat::PAUSING != 0
    }

    /// Raise an interrupt that stops the sequencer. `HCNTRL` reads back as
    /// paused, as the real one does, since that is what the HIM polls.
    fn raise_pausing(st: &mut State, bits: u8) {
        st.intstat |= bits;
        st.regs[reg::HCNTRL as usize] |= hcntrl::PAUSE;
    }

    /// Whether an address is host memory. NetBSD's doorbell address lives in
    /// bytes the HIM also uses, so a doorbell whose rings are not memory is
    /// somebody else's bytes and not a doorbell.
    fn is_memory(&self, addr: u32) -> bool {
        self.ram.lock().unwrap().dma_offset(addr).is_some()
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

    /// Attach a disk image at a SCSI id.
    pub fn attach_disk_at(&self, target: u8, path: &std::path::Path) -> std::io::Result<u64> {
        self.attach_at(target, path, false)
    }

    pub fn attach_at(&self, target: u8, path: &std::path::Path, cdrom: bool)
        -> std::io::Result<u64>
    {
        let d = ScsiDisk::open_as(path, cdrom)?;
        let blocks = d.blocks;
        self.disks.lock().unwrap().insert(target, d);
        Ok(blocks)
    }


    /// Attach at the default id, which is what a single-disk run wants.
    /// The id a disk attached without one goes on.
    pub fn default_target(&self) -> u8 {
        self.target
    }

    pub fn attach_disk(&self, path: &std::path::Path) -> std::io::Result<u64> {
        self.attach_disk_at(self.target, path)
    }

    /// The ids that have something on them.
    pub fn attached(&self) -> Vec<u8> {
        self.disks.lock().unwrap().keys().copied().collect()
    }

    /// Whether this address is the one disk we have.
    ///
    /// A controller that answers for every target and LUN makes a driver
    /// attach sixty-four copies of the same disk, which is slow, confusing,
    /// and nothing like the machine. Where the target is known we answer only
    /// at our own address; where it is not — the PROM's SCBs do not carry it —
    /// we answer, because refusing would be a guess.
    /// Which disk a command is for, if any. A command whose target we cannot
    /// see -- the PROM's SCBs do not carry one -- goes to the default id,
    /// because refusing would be a guess.
    fn target_of(&self, who: Option<(u8, u8)>) -> Option<u8> {
        match who {
            Some((_, l)) if l != 0 => None,
            Some((t, _)) => Some(t),
            None => Some(self.target),
        }
    }

    /// Run a CDB against the attached disk. Returns the data to send back and
    /// a SCSI status byte.
    fn execute_for(&self, cdb: &[u8], who: Option<(u8, u8)>) -> (Vec<u8>, u8) {
        let present = self
            .target_of(who)
            .map(|t| self.disks.lock().unwrap().contains_key(&t))
            .unwrap_or(false);
        if !present {
            // The way SCSI says "nothing here": an INQUIRY answered with
            // peripheral qualifier 011b and type 1Fh. Anything else is a
            // check condition.
            if cdb[0] == 0x12 {
                let mut d = vec![0u8; 36];
                d[0] = 0x7f;
                d[4] = 31;
                d.truncate((cdb[4] as usize).min(36));
                return (d, 0x00);
            }
            // A target that is there but has no such LUN says so, so the
            // REQUEST SENSE that follows explains the failure.
            if let Some((t, l)) = who {
                if l != 0 && self.disks.lock().unwrap().contains_key(&t) {
                    // ILLEGAL REQUEST, logical unit not supported.
                    *self.sense.lock().unwrap() = Some((0x05, 0x25, 0x00));
                }
            }
            return (Vec::new(), 0x02);
        }
        self.execute_on(cdb, self.target_of(who).unwrap())
    }

    /// Run a CDB against the attached disk. Returns the data to send back and
    /// a SCSI status byte.
    fn execute(&self, cdb: &[u8]) -> (Vec<u8>, u8) {
        self.execute_on(cdb, self.target)
    }

    fn execute_on(&self, cdb: &[u8], target: u8) -> (Vec<u8>, u8) {
        const GOOD: u8 = 0x00;
        const CHECK_CONDITION: u8 = 0x02;
        let disks = self.disks.lock().unwrap();
        let Some(disk) = disks.get(&target) else {
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
            0x00 | 0x1b | 0x16 | 0x17 | 0x2f => (Vec::new(), GOOD),
            // INQUIRY.
            0x12 => {
                let mut d = vec![0u8; 36];
                d[0] = if disk.cdrom { 0x05 } else { 0x00 };
                d[1] = if disk.cdrom { 0x80 } else { 0x00 }; // removable
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
                let last = disk.capacity().saturating_sub(1) as u32;
                let mut d = Vec::with_capacity(8);
                d.extend_from_slice(&last.to_be_bytes());
                d.extend_from_slice(&(disk.block_len() as u32).to_be_bytes());
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
            // "no special modes" without claiming anything untrue. A CD
            // says it is write-protected.
            0x1a => (vec![3, 0, if disk.cdrom { 0x80 } else { 0 }, 0], GOOD),
            // MODE SENSE(10): the same, in the 10-byte header.
            0x5a => {
                let mut d = vec![0u8, 6, 0, if disk.cdrom { 0x80 } else { 0 }, 0, 0, 0, 0];
                d.truncate(be16(7).min(8));
                (d, GOOD)
            }
            // PREVENT ALLOW MEDIUM REMOVAL and SET CD SPEED: nothing to do,
            // but a CD driver sends both on open and must not see them fail.
            0x1e | 0xbb => (Vec::new(), GOOD),
            // READ TOC/PMA/ATIP, format 0: one data track from block 0 and
            // the lead-out, in LBA or MSF as asked (MSF counts 75 frames a
            // second from a 2-second pregap, in 2048-byte frames).
            0x43 if disk.cdrom => {
                let msf = cdb[1] & 0x02 != 0;
                let addr = |lba: u64| -> [u8; 4] {
                    if msf {
                        let f = lba * disk.block_len() as u64 / 2048 + 150;
                        [0, (f / (75 * 60)) as u8, (f / 75 % 60) as u8, (f % 75) as u8]
                    } else {
                        (lba as u32).to_be_bytes()
                    }
                };
                let mut d = vec![0u8, 18, 1, 1];
                d.extend_from_slice(&[0, 0x14, 1, 0]);
                d.extend_from_slice(&addr(0));
                d.extend_from_slice(&[0, 0x14, 0xaa, 0]);
                d.extend_from_slice(&addr(disk.capacity()));
                d.truncate(be16(7));
                (d, GOOD)
            }
            // REQUEST SENSE: the last failure's sense, or none.
            0x03 => {
                let (key, asc, ascq) = self.sense.lock().unwrap().take().unwrap_or((0, 0, 0));
                let mut d = vec![0u8; 18];
                d[0] = 0x70; // current error
                d[2] = key;
                d[7] = 10;
                d[12] = asc;
                d[13] = ascq;
                d.truncate((cdb[4] as usize).max(1).min(18));
                (d, GOOD)
            }
            op => {
                if self.unsupported.lock().unwrap().insert(op) {
                    eprintln!("aic7880: target {target}: unsupported SCSI command {op:#04x} ({:02x?})", cdb);
                }
                // ILLEGAL REQUEST, invalid command operation code.
                *self.sense.lock().unwrap() = Some((0x05, 0x20, 0x00));
                (Vec::new(), CHECK_CONDITION)
            }
        }
    }

    /// Collect `want` bytes out of the buffers the driver listed, for a
    /// command that sends data to the device.
    /// Collect bytes out of an explicit list of buffers.
    fn gather_segs(&self, segs: &[(u32, u32)], want: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(want);
        for (a, l) in segs {
            if out.len() >= want {
                break;
            }
            let n = (*l as usize).min(want - out.len());
            out.extend(self.dma_read_bytes(*a, n));
        }
        out
    }

    /// Place bytes into an explicit list of buffers; returns how many landed.
    fn scatter_segs(&self, segs: &[(u32, u32)], data: &[u8]) -> usize {
        let mut done = 0usize;
        for (a, l) in segs {
            if done >= data.len() {
                break;
            }
            let n = (*l as usize).min(data.len() - done);
            for k in 0..n {
                self.dma_write8(a + k as u32, data[done + k]);
            }
            done += n;
        }
        done
    }

    fn gather(&self, sg_ptr: u32, want: usize) -> (Vec<u8>, Vec<(u32, u32)>) {
        let mut entries = Vec::new();
        let mut out = Vec::with_capacity(want);
        for i in 0..SG_MAX {
            if out.len() >= want {
                break;
            }
            let a = self.dma_read32(sg_ptr + i as u32 * 8);
            let raw = self.dma_read32(sg_ptr + i as u32 * 8 + 4);
            let l = raw & SG_LEN_MASK;
            if a == 0 || l == 0 {
                break;
            }
            entries.push((a, l));
            let n = (l as usize).min(want - out.len());
            out.extend(self.dma_read_bytes(a, n));
            // Whatever follows the last entry is not part of the list.
            if raw & SG_LAST != 0 {
                break;
            }
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
            let raw = self.dma_read32(sg_ptr + i as u32 * 8 + 4);
            let l = raw & SG_LEN_MASK;
            if a == 0 || l == 0 {
                break;
            }
            entries.push((a, l));
            let n = (l as usize).min(data.len() - done);
            for k in 0..n {
                self.dma_write8(a + k as u32, data[done + k]);
            }
            done += n;
            if raw & SG_LAST != 0 {
                break;
            }
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
            // MODE SELECT(6) and (10): the parameter list, whose block
            // descriptor can change the block length.
            0x15 => Some(cdb[4] as usize),
            0x55 => Some(((cdb[7] as usize) << 8) | cdb[8] as usize),
            _ => None,
        }
    }

    /// Run a command that carries data to the disk.
    fn execute_write(&self, cdb: &[u8], data: &[u8]) -> u8 {
        self.execute_write_on(cdb, data, self.target)
    }

    fn execute_write_on(&self, cdb: &[u8], data: &[u8], target: u8) -> u8 {
        const GOOD: u8 = 0x00;
        const CHECK_CONDITION: u8 = 0x02;
        let disks = self.disks.lock().unwrap();
        let Some(disk) = disks.get(&target) else {
            return CHECK_CONDITION;
        };
        if matches!(cdb[0], 0x15 | 0x55) {
            // Header, then any block descriptors: the block length is the
            // descriptor's last three bytes.
            let (hdr, bd_len) = if cdb[0] == 0x15 {
                (4, data.get(3).copied().unwrap_or(0) as usize)
            } else {
                (8, ((data.get(6).copied().unwrap_or(0) as usize) << 8) | data.get(7).copied().unwrap_or(0) as usize)
            };
            if bd_len >= 8 && data.len() >= hdr + 8 {
                let d = &data[hdr..hdr + 8];
                let len = ((d[5] as usize) << 16) | ((d[6] as usize) << 8) | d[7] as usize;
                if disk.cdrom {
                    if disk.set_block_len(len) {
                        eprintln!("aic7880: target {target}: block length {len}");
                    } else {
                        eprintln!("aic7880: target {target}: MODE SELECT asked for block length {len}; kept {}", disk.block_len());
                    }
                }
            }
            return GOOD;
        }
        let lba = match cdb[0] {
            0x0a => (((cdb[1] & 0x1f) as u64) << 16) | ((cdb[2] as u64) << 8) | cdb[3] as u64,
            _ => {
                ((cdb[2] as u64) << 24) | ((cdb[3] as u64) << 16)
                    | ((cdb[4] as u64) << 8) | cdb[5] as u64
            }
        };
        if disk.write_blocks(lba, data) {
            return GOOD;
        }
        // Say why, so the REQUEST SENSE that follows explains the failure
        // instead of reporting that nothing went wrong.
        let sense = if disk.cdrom {
            // DATA PROTECT, write protected.
            (0x07, 0x27, 0x00)
        } else if lba + (data.len() / BLOCK_SIZE) as u64 > disk.blocks {
            // ILLEGAL REQUEST, logical block address out of range.
            (0x05, 0x21, 0x00)
        } else {
            // MEDIUM ERROR, write error: the image itself refused it.
            (0x03, 0x0c, 0x00)
        };
        *self.sense.lock().unwrap() = Some(sense);
        CHECK_CONDITION
    }

    /// Consume everything the host has added to its queue-in ring.
    fn drain_netbsd(&self, st: &mut State, want: u8) {
        let shared = Self::scratch_le32(st, nb::SHARED_DATA_ADDR);
        let hscb_base = Self::scratch_le32(st, nb::HSCB_ADDR);
        if shared == 0 || hscb_base == 0 {
            return;
        }
        // The ring is 256 entries, so it cannot legitimately need more than
        // that many steps to catch up.
        for _ in 0..nb::FIFO_LEN {
            if st.qinpos == want {
                break;
            }
            let tag = self.dma_read8(shared + nb::FIFO_LEN + st.qinpos as u32);
            st.qinpos = st.qinpos.wrapping_add(1);
            self.submit_netbsd(st, tag, hscb_base, shared);
        }
    }

    /// The buffers one command transfers through: the SCB carries the first
    /// segment directly and points at the rest.
    fn netbsd_segments(&self, hscb: u32) -> Vec<(u32, u32)> {
        let mut segs = Vec::new();
        let first_len = self.dma_read32_le(hscb + nb::DATACNT) & SG_LEN_MASK;
        if first_len != 0 {
            segs.push((self.dma_read32_le(hscb + nb::DATAPTR), first_len));
        }
        let mut sg = self.dma_read32_le(hscb + nb::SGPTR) & nb::SG_PTR_MASK;
        for _ in 0..SG_MAX {
            if sg == 0 {
                break;
            }
            let a = self.dma_read32_le(sg);
            let raw = self.dma_read32_le(sg + 4);
            let l = raw & SG_LEN_MASK;
            if a == 0 && l == 0 {
                break;
            }
            segs.push((a, l));
            if raw & nb::LAST_SEG != 0 {
                break;
            }
            sg += 8;
        }
        segs
    }

    /// Run one command the NetBSD way.
    fn submit_netbsd(&self, st: &mut State, tag: u8, hscb_base: u32, shared: u32) {
        let hscb = hscb_base + tag as u32 * nb::HSCB_STRIDE;
        let len = self.dma_read8(hscb + nb::CDB_LEN) as usize;
        // A CDB longer than the twelve bytes that fit inline is kept
        // elsewhere and pointed at from the same field.
        let cdb = if len <= 12 {
            self.dma_read_bytes(hscb + nb::CDB, len.clamp(6, 12))
        } else {
            self.dma_read_bytes(self.dma_read32_le(hscb + nb::CDB), len.min(32))
        };
        let segs = self.netbsd_segments(hscb);
        // SCSIID holds the target in the high nibble and our own id in the
        // low one -- the driver's own card dump prints SCB_SCSIID[0x17] for
        // target 1 on an adapter at id 7.
        let scsiid = self.dma_read8(hscb + nb::SCSIID);
        let lun = self.dma_read8(hscb + nb::LUN) & 0x07;

        let (status, moved) = match Self::data_out_len(&cdb) {
            Some(want) => {
                let data = self.gather_segs(&segs, want);
                let n = data.len();
                let t = self.target_of(Some((scsiid >> 4, lun))).unwrap_or(self.target);
                (self.execute_write_on(&cdb, &data, t), n)
            }
            None => {
                let (data, status) = self.execute_for(&cdb, Some((scsiid >> 4, lun)));
                (status, self.scatter_segs(&segs, &data))
            }
        };

        let hex: Vec<String> = cdb.iter().map(|b| format!("{b:02x}")).collect();
        let segtxt: Vec<String> = segs.iter()
            .map(|(a, l)| format!("0x{a:08x}+{l}")).collect();
        Self::note(st, format!(
            "nb tag {tag} id 0x{scsiid:02x} lun {lun}: CDB {} -> status {status}, {moved} bytes into [{}]",
            hex.join(" "), segtxt.join(" ")));

        // Status goes back in the SCB, and the residual words beside it are
        // cleared so the driver does not go looking for one.
        for i in 0..8 {
            self.dma_write8(hscb + i, 0);
        }
        self.dma_write8(hscb + nb::STATUS, status);
        for i in 0..4 {
            self.dma_write8(hscb + nb::SGPTR + i, 0);
        }

        st.total_cmds += 1;
        st.total_in += moved as u64;

        // And the tag goes back through the other ring.
        self.dma_write8(shared + st.qoutpos, tag);
        st.qoutpos = (st.qoutpos + 1) % nb::FIFO_LEN;
        st.intstat |= intstat::CMDCMPLT;
    }

    /// Fetch a queued command and run it.
    fn submit(&self, st: &mut State, tag: u8) {
        let i = (tag as usize) % SCB_COUNT;
        // The register window is used by both drivers, but only to *clear*
        // SCBs during initialisation -- a window write is not evidence that
        // the command lives on the chip. Report a window-filled SCB that
        // carries anything, so the day one does it is not a silent surprise,
        // and otherwise take the host-memory path both drivers actually use.
        if st.scb_onchip[i] {
            st.scb_onchip[i] = false;
            let scb = st.scb[i];
            if scb.iter().take(SCB_SIZE - 1).any(|b| *b != 0) {
                let hex: Vec<String> = scb.iter().map(|b| format!("{b:02x}")).collect();
                Self::note(st, format!("tag {tag}: on-chip SCB carries {}", hex.join(" ")));
            }
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
            "tag {tag}: SCB at 0x{scb_addr:08x} control 0x{:02x} target {} lun {} cdblen {}",
            scb[scb::CONTROL], scb[scb::TCL] >> 4, scb[scb::TCL] & 0x07, len));
        Self::note(st, format!("   CDB {}", hex(&cdb)));
        Self::note(st, format!("   SCB {}", hex(&scb)));
        let sg_ptr = u32::from_be_bytes([
            scb[scb::SG_PTR], scb[scb::SG_PTR + 1],
            scb[scb::SG_PTR + 2], scb[scb::SG_PTR + 3],
        ]);
        let tcl = scb[scb::TCL];
        let target = tcl >> 4;
        let who = Some((target, tcl & 0x07));
        let errors = Self::him_errors(st);

        // Nobody at that id, or our own: the selection times out. The HIM
        // finds the tag in the waiting-SCB byte and gives up on the id at
        // once, where an INQUIRY answered "no device" with GOOD status made
        // it look for a device that never comes.
        let own = st.regs[reg::SCSIID as usize] & 0x0f;
        if errors && (target == own || !self.disks.lock().unwrap().contains_key(&target)) {
            st.regs[him::WAITING_SCB as usize] = tag;
            st.sstat1 |= sstat1::SELTO;
            Self::raise_pausing(st, intstat::SCSIINT);
            Self::note(st, format!("   -> selection timeout at id {target}"));
            return;
        }
        let (status, moved, sg, dir) = match Self::data_out_len(&cdb) {
            Some(want) => {
                let (data, sg) = self.gather(sg_ptr, want);
                let n = data.len();
                let t = self.target_of(who).unwrap_or(self.target);
                (self.execute_write_on(&cdb, &data, t), n, sg, "out of")
            }
            None => {
                let (data, status) = self.execute_for(&cdb, who);
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
        // Nothing is written back to the SCB. GOOD status means the zeros
        // the host left in it stand, and anything else is reported below.
        st.total_cmds += 1;
        if dir == "into" { st.total_in += moved as u64 } else { st.total_out += moved as u64 }
        Self::note(st, format!(
            "   -> status {status}, {moved} bytes {dir} [{}]", sgtxt.join(" ")));
        st.executed.push((tag, scb_addr, cdb));

        // Any status but GOOD stops the sequencer with the status in
        // scratch RAM for the HIM to collect. It rewrites the same SCB as a
        // REQUEST SENSE and queues the tag again at the head, and that one
        // completes normally with the sense `self.sense` is holding.
        if errors && status != 0x00 {
            st.regs[him::ACTIVE_SCB as usize] = tag;
            st.regs[him::STATUS as usize] = status;
            st.regs[reg::SCBPTR as usize] = tag;
            st.intstat = (st.intstat & !intstat::INTCODE) | him::BAD_STATUS;
            Self::raise_pausing(st, intstat::SEQINT);
            return;
        }

        if Self::him(st) {
            self.complete_him(st, tag, target);
        } else {
            self.complete(st, tag);
        }
    }

    /// Post a finished tag the way the HIM's sequencer does: two single
    /// bytes into the queue-out array, with the cursor in scratch RAM.
    ///
    /// The HIM reads exactly one byte for each completion, at its own
    /// cursor (starting at 4, moving down by 4) with the low two bits
    /// flipped, because the array is in the view of memory that keeps
    /// 32-bit words whole and so turns single bytes around. The sequencer
    /// keeps its cursor four behind: it marks the slot the host will look
    /// at next as empty and puts the tag one slot ahead of that, which is
    /// where the host is looking now. Writing tags at 0, 1, 2, ... instead
    /// put every one where the kernel never looked, and each command timed
    /// out after ten seconds. The PROM scans the whole array, so this
    /// serves it too.
    fn complete_him(&self, st: &mut State, tag: u8, target: u8) {
        let ptr = Self::scratch_le32(st, sram::QOUTFIFO);
        let base = ptr & !0xff;
        let p = ptr & 0xff;
        // The low alias maps address zero to memory, so an array that was
        // never configured must not be written through it.
        if base != 0 {
            self.dma_write8(base + (p ^ 3), QOUTFIFO_EMPTY);
            self.dma_write8(base + (((p + 4) & 0xff) ^ 3), tag);
        }
        // Add 4 and then 0xf8, with no carry into the next byte: the
        // cursor wraps inside the array.
        st.regs[sram::QOUTFIFO as usize] = (p.wrapping_sub(4) & 0xff) as u8;
        st.regs[him::ACTIVE_SCB as usize] = 0xff;
        // The target is no longer busy. The busy table is byte 31 of the
        // on-chip SCB with the target's number.
        st.scb[target as usize % SCB_COUNT][SCB_SIZE - 1] = 0xff;
        st.intstat |= intstat::CMDCMPLT;
    }

    /// Post a finished tag where a driver without the HIM looks for it.
    fn complete(&self, st: &mut State, tag: u8) {
        let fifo = Self::scratch_le32(st, sram::QOUTFIFO);
        if fifo != 0 {
            self.dma_write8(fifo + st.qoutpos, tag);
            st.qoutpos = (st.qoutpos + 1) % QOUTFIFO_LEN;
        }
        st.qout.push_back(tag);
        st.intstat |= intstat::CMDCMPLT;
    }

    /// Run queued tags until the queue is empty or the sequencer stops.
    /// A command that ends in a pausing interrupt stops it, and the rest
    /// wait until the host has dealt with that one and restarts it.
    fn run_queue(&self, st: &mut State) {
        while !Self::paused(st) {
            let Some(tag) = st.qin.pop_front() else { break };
            self.submit(st, tag);
        }
    }

    fn scb_index(st: &State) -> usize {
        (st.regs[reg::SCBPTR as usize] as usize) % SCB_COUNT
    }

    fn read8(&self, off: u32) -> u8 {
        let mut st = self.st.lock().unwrap();
        match off {
            reg::INTSTAT => st.intstat,
            reg::SSTAT0 => st.sstat0,
            reg::SSTAT1 => st.sstat1,
            // No target ever asks for a byte, and the HIM waits for REQ to
            // drop after an error, so it must never read as set.
            reg::SCSISIG => st.regs[off as usize] & !scsisig::REQ,
            // A real FIFO: reading takes the head. Only the low four bits
            // exist with no external SCB RAM, and an empty one reads zero.
            reg::QINFIFO => st.qin.pop_front().unwrap_or(0) & 0x0f,
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
        // SCSIRSTO: the controller drives a SCSI bus reset, and every device
        // returns to its power-on state. What that is for a CD-ROM depends on
        // whose drive it is: an SGI drive comes up in 512-byte blocks, which
        // SGI's own driver (the HIM, in the PROM and in IRIX) relies on --
        // IRIX resets the bus when it attaches and never sends MODE SELECT
        // again, then reads 512-byte sectors. The BSDs' driver expects the
        // common 2048 after its reset (the PROM had left 512 behind).
        if off == reg::SCSISEQ && val & 0x01 != 0 {
            let sgi = Self::him(&self.st.lock().unwrap());
            for d in self.disks.lock().unwrap().values() {
                if sgi && d.cdrom {
                    d.set_block_len(512);
                } else {
                    d.power_on_block_len();
                }
            }
        }
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
                // An interrupt that stopped the sequencer keeps it stopped
                // until the host has cleared it; only then does a restart
                // take. The HIM checks for exactly this and retries.
                let val = if st.intstat & intstat::PAUSING != 0 { val | hcntrl::PAUSE } else { val };
                st.regs[reg::HCNTRL as usize] = val;
                // The driver pauses and restarts the sequencer around every
                // shared-state access, thousands of times. Counting them says
                // as much as logging them and leaves room for the commands.
                if was & hcntrl::PAUSE != val & hcntrl::PAUSE {
                    st.pauses += 1;
                }
                if val & hcntrl::PAUSE == 0 {
                    // The HIM's sequencer goes back to its idle loop, where
                    // nothing is being selected and nothing is connected.
                    if Self::him(&st) {
                        st.regs[him::WAITING_SCB as usize] = 0xff;
                        st.regs[him::ACTIVE_SCB as usize] = 0xff;
                    }
                    // And picks up whatever was queued while it was stopped.
                    self.run_queue(&mut st);
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
            nb::KERNEL_QINPOS => {
                st.regs[off as usize] = val;
                // The HIM's next-SCB table covers this byte, and filling it
                // at init used to ring the doorbell with its placeholder
                // pointer standing in for NetBSD's rings: 127 commands from
                // nowhere, and completions nobody had asked for. A doorbell
                // is only one when the HIM is not driving and both rings
                // are in memory.
                let rings = [nb::HSCB_ADDR, nb::SHARED_DATA_ADDR]
                    .iter()
                    .all(|r| self.is_memory(Self::scratch_le32(&st, *r)));
                if !Self::him(&st) && rings {
                    self.drain_netbsd(&mut st, val);
                }
            }
            reg::ERROR_CLRINT => {
                // Write-one-to-clear, sharing an address with the sequencer
                // error register that reads back. Clearing SEQINT clears
                // its code with it.
                let clr = val & 0x0f;
                st.intstat &= !clr;
                if clr & intstat::SEQINT != 0 {
                    st.intstat &= !intstat::INTCODE;
                }
            }
            // CLRSINT0 and CLRSINT1: clear what is written, store nothing.
            reg::SSTAT0 => st.sstat0 &= !val,
            reg::SSTAT1 => st.sstat1 &= !val,
            reg::QINFIFO => {
                let tag = val & 0x0f;
                let scb = st.scb[tag as usize];
                st.queued.push((val, scb));
                // Sixteen deep; a write to a full FIFO is lost, as on the
                // chip.
                if st.qin.len() < SCB_COUNT {
                    st.qin.push_back(tag);
                }
                self.run_queue(&mut st);
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
        Aic7880::new(std::sync::Arc::new(std::sync::Mutex::new(crate::dev::crime_mem::O2Memory::single(1 << 20))))
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
    /// layout is read off. The sequencer is paused so the tag stays queued
    /// long enough to be counted.
    #[test]
    fn queueing_an_scb_records_it() {
        let c = chip();
        c.write(reg::HCNTRL, hcntrl::PAUSE as u32, 1);
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
        let ram = std::sync::Arc::new(std::sync::Mutex::new(crate::dev::crime_mem::O2Memory::single(1 << 20)));
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
            let put32 = |m: &mut [u8], at: u32, v: u32| {
                m[at as usize..at as usize + 4].copy_from_slice(&v.to_be_bytes());
            };
            // array[1] = the SCB's bus address
            put32(&mut m, array + 4, base + scb_addr);
            // SCB: pointers to the scatter-gather list and the CDB
            put32(&mut m, scb_addr + scb::SG_PTR as u32, base + sg_addr);
            put32(&mut m, scb_addr + scb::CMD_PTR as u32, base + cdb_addr);
            // The driver's control flags: disconnect allowed.
            m[(scb_addr + scb::CONTROL as u32) as usize] = 0x40;
            // Address the id the disk was attached at.
            m[(scb_addr + scb::TCL as u32) as usize] = 1 << 4;
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
        assert_eq!(m[(scb_addr + scb::CONTROL as u32) as usize], 0x40,
                   "and the SCB is the driver's: nothing is written back into it");
        assert_eq!(m[qout as usize], 1, "the tag goes into the host completion FIFO");
        drop(m);
        assert_eq!(c.read(reg::INTSTAT, 1) as u8 & intstat::CMDCMPLT, intstat::CMDCMPLT,
                   "and the driver is told to look");
    }

    /// A write goes the other way through the same scatter-gather list, and
    /// must reach the image rather than growing it.
    #[test]
    fn a_write_reaches_the_disk_and_reads_back() {
        let ram = std::sync::Arc::new(std::sync::Mutex::new(crate::dev::crime_mem::O2Memory::single(1 << 20)));
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
            let put32 = |m: &mut [u8], at: u32, v: u32| {
                m[at as usize..at as usize + 4].copy_from_slice(&v.to_be_bytes());
            };
            put32(&mut m, array + 4, base + scb);
            put32(&mut m, scb + scb::SG_PTR as u32, base + sg);
            put32(&mut m, scb + scb::CMD_PTR as u32, base + cdb);
            m[(scb + scb::CDB_LEN as u32) as usize] = 10;
            m[(scb + scb::TCL as u32) as usize] = 1 << 4;
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
        sign(&c);
        c.write(reg::QINFIFO, 1, 1);

        assert_eq!(c.read(reg::INTSTAT, 1) as u8, intstat::CMDCMPLT,
                   "the write must report success, which is a plain completion");
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
        let ram = std::sync::Arc::new(std::sync::Mutex::new(crate::dev::crime_mem::O2Memory::single(4 << 20)));
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
            let put32 = |m: &mut [u8], at: u32, v: u32| {
                m[at as usize..at as usize + 4].copy_from_slice(&v.to_be_bytes());
            };
            put32(&mut m, array + 4, base + scb);
            put32(&mut m, scb + scb::SG_PTR as u32, base + sg);
            put32(&mut m, scb + scb::CMD_PTR as u32, base + cdb);
            m[(scb + scb::CDB_LEN as u32) as usize] = 10;
            m[(scb + scb::TCL as u32) as usize] = 1 << 4;
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
        let ram = std::sync::Arc::new(std::sync::Mutex::new(crate::dev::crime_mem::O2Memory::single(1 << 16)));
        ram.lock().unwrap().write(0x40, &[0xa5]);
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

    type Ram = std::sync::Arc<std::sync::Mutex<crate::dev::crime_mem::O2Memory>>;

    /// Where the HIM rig keeps things in host memory.
    const ARRAY: u32 = 0x1000;
    const QOUT: u32 = 0x4000;
    fn scb_at(tag: u8) -> u32 { 0x2000 + tag as u32 * 0x40 }
    fn cdb_at(tag: u8) -> u32 { 0x5000 + tag as u32 * 0x20 }
    fn sg_at(tag: u8) -> u32 { 0x6000 + tag as u32 * 0x10 }
    fn buf_at(tag: u8) -> u32 { 0x8000 + tag as u32 * 0x200 }

    fn put32(ram: &Ram, at: u32, v: u32) {
        ram.lock().unwrap()[at as usize..at as usize + 4].copy_from_slice(&v.to_be_bytes());
    }

    /// The HIM's signature, as it writes it: a byte at a time, little end
    /// first.
    fn sign(c: &Aic7880) {
        for (off, v) in him::SIGNATURE {
            for (i, b) in v.to_le_bytes().iter().enumerate() {
                c.write(off + i as u32, *b as u32, 1);
            }
        }
    }

    /// A controller set up the way the kernel's HIM leaves it after init:
    /// both arrays configured, the signature in place, the next-SCB table
    /// full of 0x7f, the queue-out array full of 0xff, and its own id 0, as
    /// on an O2. One disk, at id 1.
    fn him_rig(name: &str) -> (Aic7880, Ram) {
        let ram: Ram = std::sync::Arc::new(std::sync::Mutex::new(crate::dev::crime_mem::O2Memory::single(1 << 20)));
        let c = Aic7880::new(ram.clone());
        let img = std::env::temp_dir().join(format!("iris-aic7880-{name}.img"));
        std::fs::write(&img, vec![0u8; 64 * BLOCK_SIZE]).expect("test image");
        c.attach_disk_at(1, &img).expect("attach");
        let base = crate::ip32::RAM_BASE;
        for (off, v) in [(sram::SCB_ARRAY, base + ARRAY), (sram::QOUTFIFO, base + QOUT)] {
            for (i, b) in v.to_le_bytes().iter().enumerate() {
                c.write(off + i as u32, *b as u32, 1);
            }
        }
        c.write(him::WAITING_SCB, 0xff, 1);
        c.write(him::ACTIVE_SCB, 0xff, 1);
        sign(&c);
        for off in 0x46..=0x55 {
            c.write(off, 0x7f, 1);
        }
        c.write(reg::SCSIID, 0, 1);
        ram.lock().unwrap()[QOUT as usize..QOUT as usize + 256].fill(QOUTFIFO_EMPTY);
        (c, ram)
    }

    /// Build a command for `tag` in host memory, the way the HIM does: the
    /// SCB's address in the array, one scatter-gather entry marked last.
    fn put_cmd(ram: &Ram, tag: u8, target: u8, cdb: &[u8], len: u32) {
        let base = crate::ip32::RAM_BASE;
        let scb = scb_at(tag);
        put32(ram, ARRAY + tag as u32 * 4, base + scb);
        {
            let mut m = ram.lock().unwrap();
            m[scb as usize..scb as usize + 32].fill(0);
            m[(scb + scb::SEG_COUNT as u32) as usize] = 1;
            m[(scb + scb::CDB_LEN as u32) as usize] = cdb.len() as u8;
            m[(scb + scb::CONTROL as u32) as usize] = 0x40;
            m[(scb + scb::TCL as u32) as usize] = target << 4;
            m[cdb_at(tag) as usize..cdb_at(tag) as usize + cdb.len()].copy_from_slice(cdb);
        }
        put32(ram, scb + scb::SG_PTR as u32, base + sg_at(tag));
        // The CDB is in the low view, as the HIM points at it.
        put32(ram, scb + scb::CMD_PTR as u32, cdb_at(tag));
        put32(ram, sg_at(tag), buf_at(tag));
        put32(ram, sg_at(tag) + 4, len | SG_LAST);
    }

    fn qout(ram: &Ram) -> Vec<u8> {
        ram.lock().unwrap()[QOUT as usize..QOUT as usize + 256].to_vec()
    }

    const TUR: [u8; 6] = [0x00, 0, 0, 0, 0, 0];

    /// The HIM reads one byte per completion, at its cursor (4, then down by
    /// 4) with the low bits flipped. Tags written anywhere else are never
    /// found, and every command times out.
    #[test]
    fn him_completions_land_where_the_kernel_looks() {
        let (c, ram) = him_rig("him-qout");
        put_cmd(&ram, 0, 1, &TUR, 0);
        put_cmd(&ram, 1, 1, &TUR, 0);
        c.write(reg::QINFIFO, 0, 1);
        c.write(reg::QINFIFO, 1, 1);

        let q = qout(&ram);
        assert_eq!(q[7], 0, "the first completion is at the HIM's cursor 4, flipped");
        assert_eq!(q[3], 1, "the second at cursor 0, flipped");
        assert_eq!(q[0xff], QOUTFIFO_EMPTY, "the slot after that is marked empty");
        assert!(q.iter().enumerate().all(|(i, b)| i == 3 || i == 7 || *b == QOUTFIFO_EMPTY),
                "and nothing else is touched");
        assert_eq!(c.read(sram::QOUTFIFO, 1), 0xf8, "the cursor moved down by 4 twice, wrapping");
        assert_eq!(c.read(reg::INTSTAT, 1) as u8, intstat::CMDCMPLT);
        assert_eq!(c.read(reg::QINCNT, 1), 0, "both were taken");
        assert_eq!(ram.lock().unwrap()[(scb_at(0) + scb::CONTROL as u32) as usize], 0x40,
                   "and the SCB's control flags are left alone");
    }

    /// The HIM fills its next-SCB table with 0x7f, and one of those bytes
    /// is NetBSD's doorbell. It must not ring.
    #[test]
    fn the_him_filling_its_tables_rings_no_doorbell() {
        let c = chip();
        sign(&c);
        for off in 0x46..=0x55 {
            c.write(off, 0x7f, 1);
        }
        assert_eq!(c.read(reg::INTSTAT, 1), 0, "nothing completed");
        assert!(c.executed().is_empty());
        assert!(!c.notes().iter().any(|n| n.starts_with("nb tag")), "nothing ran");
        assert!(c.dma_misses.lock().unwrap().is_empty(), "and nothing was fetched");
    }

    /// NetBSD's doorbell, with its rings in memory and no HIM, still runs
    /// a command.
    #[test]
    fn the_netbsd_doorbell_still_runs_commands() {
        let ram: Ram = std::sync::Arc::new(std::sync::Mutex::new(crate::dev::crime_mem::O2Memory::single(1 << 20)));
        let c = Aic7880::new(ram.clone());
        let img = std::env::temp_dir().join("iris-aic7880-nb.img");
        std::fs::write(&img, vec![0u8; 64 * BLOCK_SIZE]).expect("test image");
        c.attach_disk_at(1, &img).expect("attach");
        let (hscbs, shared) = (0x2000u32, 0x4000u32);
        let tag = 3u8;
        {
            let mut m = ram.lock().unwrap();
            m[(shared + nb::FIFO_LEN) as usize] = tag;
            let h = (hscbs + tag as u32 * nb::HSCB_STRIDE) as usize;
            m[h + nb::CDB_LEN as usize] = 6; // TEST UNIT READY
            m[h + nb::SCSIID as usize] = 0x17;
            m[h + nb::STATUS as usize] = 0xaa;
        }
        for (off, v) in [(nb::HSCB_ADDR, hscbs), (nb::SHARED_DATA_ADDR, shared)] {
            for (i, b) in v.to_le_bytes().iter().enumerate() {
                c.write(off + i as u32, *b as u32, 1);
            }
        }
        c.write(nb::KERNEL_QINPOS, 1, 1);
        let m = ram.lock().unwrap();
        assert_eq!(m[shared as usize], tag, "the tag comes back on the queue-out ring");
        assert_eq!(m[(hscbs + tag as u32 * nb::HSCB_STRIDE + nb::STATUS) as usize], 0, "with GOOD status");
        drop(m);
        assert_eq!(c.read(reg::INTSTAT, 1) as u8, intstat::CMDCMPLT);
    }

    /// While paused, the queue-in FIFO holds what it is given, and the host
    /// can pop and push to reorder it: that is how the HIM puts a REQUEST
    /// SENSE at the head. Restarting runs them in the new order.
    #[test]
    fn a_paused_queue_holds_and_rotates() {
        let (c, ram) = him_rig("him-rotate");
        put_cmd(&ram, 0, 1, &TUR, 0);
        put_cmd(&ram, 1, 1, &TUR, 0);
        c.write(reg::HCNTRL, hcntrl::PAUSE as u32, 1);
        c.write(reg::QINFIFO, 0, 1);
        c.write(reg::QINFIFO, 1, 1);
        assert_eq!(c.read(reg::QINCNT, 1), 2, "nothing runs while paused");
        assert!(c.executed().is_empty());

        let head = c.read(reg::QINFIFO, 1);
        assert_eq!(head, 0, "reading takes the head");
        assert_eq!(c.read(reg::QINCNT, 1), 1);
        c.write(reg::QINFIFO, head, 1);
        assert_eq!(c.read(reg::QINCNT, 1), 2, "and writing puts it at the back");

        c.write(reg::HCNTRL, hcntrl::INTEN as u32, 1);
        let order: Vec<u8> = c.executed().iter().map(|e| e.0).collect();
        assert_eq!(order, vec![1, 0], "restarting runs them in the new order");
        assert_eq!(c.read(reg::QINCNT, 1), 0);
    }

    /// Nobody at an id: the selection times out, the way the HIM expects,
    /// and nothing goes to the queue-out.
    #[test]
    fn an_empty_id_times_out_its_selection() {
        let (c, ram) = him_rig("him-selto");
        let inquiry = [0x12, 0, 0, 0, 36, 0];
        put_cmd(&ram, 2, 3, &inquiry, 36);
        c.write(reg::QINFIFO, 2, 1);

        assert_eq!(c.read(reg::INTSTAT, 1), intstat::SCSIINT as u32);
        assert_ne!(c.read(reg::SSTAT1, 1) as u8 & sstat1::SELTO, 0, "SSTAT1 says selection timeout");
        assert_eq!(c.read(him::WAITING_SCB, 1), 2, "for the tag being selected");
        assert_ne!(c.read(reg::HCNTRL, 1) as u8 & hcntrl::PAUSE, 0, "and the sequencer has stopped");
        assert!(c.interrupting());
        assert!(qout(&ram).iter().all(|b| *b == QOUTFIFO_EMPTY), "nothing completed");
        assert!(ram.lock().unwrap()[buf_at(2) as usize..buf_at(2) as usize + 36].iter().all(|b| *b == 0),
                "and no data moved");

        // A restart before the interrupt is cleared does not take.
        c.write(reg::HCNTRL, hcntrl::INTEN as u32, 1);
        assert_ne!(c.read(reg::HCNTRL, 1) as u8 & hcntrl::PAUSE, 0);

        c.write(reg::SSTAT1, 0x88, 1);
        c.write(reg::ERROR_CLRINT, intstat::SCSIINT as u32, 1);
        c.write(reg::HCNTRL, hcntrl::INTEN as u32, 1);
        assert_eq!(c.read(reg::INTSTAT, 1), 0);
        assert_eq!(c.read(reg::SSTAT1, 1), 0);
        assert_eq!(c.read(reg::HCNTRL, 1) as u8 & hcntrl::PAUSE, 0, "and it runs again");
        assert!(!c.interrupting());
    }

    /// A failed command stops the sequencer with its status in scratch RAM.
    /// The HIM turns the same SCB into a REQUEST SENSE and queues it at the
    /// head; that one completes normally, carrying the reason.
    #[test]
    fn a_check_condition_goes_through_the_hims_auto_sense() {
        let (c, ram) = him_rig("him-sense");
        let tag = 5u8;
        put_cmd(&ram, tag, 1, &[0xc0, 0, 0, 0, 0, 0], 0);
        c.write(reg::QINFIFO, tag as u32, 1);

        assert_eq!(c.read(reg::INTSTAT, 1), 0x51, "SEQINT with the bad-status code");
        assert_eq!(c.read(him::ACTIVE_SCB, 1), tag as u32, "for the tag connected");
        assert_eq!(c.read(him::STATUS, 1), 0x02, "with CHECK CONDITION handed over");
        assert_ne!(c.read(reg::HCNTRL, 1) as u8 & hcntrl::PAUSE, 0);
        assert!(qout(&ram).iter().all(|b| *b == QOUTFIFO_EMPTY), "nothing completed");

        // The HIM rewrites the SCB as REQUEST SENSE, pointed at its own
        // sense list just after the SCB, and queues the tag again.
        let sense_list = scb_at(tag) + 32;
        let sense_buf = 0x9000u32;
        put32(&ram, sense_list, sense_buf);
        put32(&ram, sense_list + 4, 18 | SG_LAST);
        put32(&ram, scb_at(tag) + scb::SG_PTR as u32, crate::ip32::RAM_BASE + sense_list);
        ram.lock().unwrap()[cdb_at(tag) as usize..cdb_at(tag) as usize + 6]
            .copy_from_slice(&[0x03, 0, 0, 0, 18, 0]);
        c.write(reg::QINFIFO, tag as u32, 1);
        assert_eq!(c.read(reg::QINCNT, 1), 1, "held while the sequencer is stopped");

        c.write(reg::ERROR_CLRINT, intstat::SEQINT as u32, 1);
        assert_eq!(c.read(reg::INTSTAT, 1), 0, "clearing SEQINT clears its code too");
        c.write(reg::HCNTRL, hcntrl::INTEN as u32, 1);

        let m = ram.lock().unwrap();
        assert_eq!(m[sense_buf as usize + 2], 0x05, "ILLEGAL REQUEST");
        assert_eq!(m[sense_buf as usize + 12], 0x20, "invalid command operation code");
        drop(m);
        assert_eq!(qout(&ram)[7], tag, "and the tag completes through the queue-out");
        assert_eq!(c.read(reg::INTSTAT, 1) as u8, intstat::CMDCMPLT);
        assert_eq!(c.read(reg::QINCNT, 1), 0);
    }

    /// The PROM's build of the program keeps the answers it boots with
    /// today: an empty id is "no device" with GOOD status, and completes.
    #[test]
    fn the_proms_program_keeps_its_old_answers() {
        let (c, ram) = him_rig("him-prom");
        c.write(reg::SEQCTL, seqctl::LOADRAM as u32, 1);
        for _ in 0..him::PROM_PROGRAM_LEN {
            c.write(reg::SEQRAM, 0, 1);
        }
        c.write(reg::SEQCTL, 0, 1);
        put_cmd(&ram, 1, 3, &[0x12, 0, 0, 0, 36, 0], 36);
        c.write(reg::QINFIFO, 1, 1);
        assert_eq!(c.read(reg::INTSTAT, 1) as u8, intstat::CMDCMPLT);
        assert_eq!(ram.lock().unwrap()[buf_at(1) as usize], 0x7f, "nothing here");
        assert_eq!(qout(&ram)[7], 1, "completed where the HIM looks");
    }

    /// CLRSINT0 and CLRSINT1 share addresses with SSTAT0 and SSTAT1. A
    /// channel reset writes 0xff to both, and reading that back said an
    /// external bus reset had happened.
    #[test]
    fn clearing_scsi_status_leaves_it_clear() {
        let c = chip();
        c.write(reg::SSTAT1, 0xff, 1);
        assert_eq!(c.read(reg::SSTAT1, 1), 0);
        c.write(reg::SSTAT0, 0xff, 1);
        assert_eq!(c.read(reg::SSTAT0, 1), 0);
        {
            let mut st = c.st.lock().unwrap();
            st.sstat0 = 0xff;
            st.sstat1 = 0xff;
        }
        c.write(reg::SSTAT1, 0xff, 1);
        assert_eq!(c.read(reg::SSTAT1, 1), 0, "a clear clears");
        c.write(reg::SSTAT0, 0xff, 1);
        assert_eq!(c.read(reg::SSTAT0, 1), 0);

        c.write(reg::SCSISIG, 0xff, 1);
        assert_eq!(c.read(reg::SCSISIG, 1) as u8 & scsisig::REQ, 0, "REQ never reads as set");
    }

    /// Failed writes say why, so the REQUEST SENSE after them is not "no
    /// sense".
    #[test]
    fn failed_writes_carry_sense() {
        let c = chip();
        let img = std::env::temp_dir().join("iris-aic7880-wsense.img");
        std::fs::write(&img, vec![0u8; 8 * BLOCK_SIZE]).expect("test image");
        c.attach_at(1, &img, false).expect("attach");
        c.attach_at(4, &img, true).expect("attach CD");
        let write10 = |lba: u8| [0x2a, 0, 0, 0, 0, lba, 0, 0, 1, 0];
        let block = vec![0u8; BLOCK_SIZE];

        assert_eq!(c.execute_write_on(&write10(0), &block, 4), 0x02);
        assert_eq!(*c.sense.lock().unwrap(), Some((0x07, 0x27, 0x00)), "a CD is write protected");
        assert_eq!(c.execute_write_on(&write10(8), &block, 1), 0x02);
        assert_eq!(*c.sense.lock().unwrap(), Some((0x05, 0x21, 0x00)), "past the end is out of range");
    }
}

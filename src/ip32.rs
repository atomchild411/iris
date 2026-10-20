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

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

// The O2's devices live one to a file under `crate::dev`; their names stay
// reachable here for the bus and the machine.
pub use crate::dev::crime::*;
pub use crate::dev::ds1687::*;
pub use crate::dev::mace_isa::*;
pub use crate::dev::mace_pci::*;
pub use crate::dev::mace_ps2::*;
pub use crate::dev::mace_sdma::*;
pub use crate::dev::mace_ust::*;
pub use crate::dev::ns16550::*;
pub use crate::dev::onewire::*;


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

/// Whether the per-register access counters (MACE's, GBE's, the rendering
/// engine's) run: `IRIS_IP32_HOT`. They are for finding a driver polling a
/// bit nothing sets, and each access took a lock and a map update, every
/// status poll of a GL scene included.
pub fn hot_counters() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("IRIS_IP32_HOT").is_some())
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
    reads: std::sync::atomic::AtomicU64,
    writes: std::sync::atomic::AtomicU64,
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
            reads: std::sync::atomic::AtomicU64::new(0),
            writes: std::sync::atomic::AtomicU64::new(0),
            hot: Mutex::new(std::collections::BTreeMap::new()),
            cells: Mutex::new(std::collections::BTreeMap::new()),
            who: Mutex::new(std::collections::BTreeMap::new()),
            pc: Mutex::new(None),
        }
    }
    pub fn counts(&self) -> (u64, u64) {
        (self.reads.load(Ordering::Relaxed), self.writes.load(Ordering::Relaxed))
    }
    /// Offsets by access count, busiest first: `(offset, reads, writes)`.
    pub fn hottest(&self) -> Vec<(u32, u64, u64)> {
        let mut v: Vec<(u32, u64, u64)> =
            self.hot.lock().unwrap().iter().map(|(&o, &(r, w))| (o, r, w)).collect();
        v.sort_by_key(|&(_, r, w)| std::cmp::Reverse(r + w));
        v
    }
    fn r(&self) {
        self.reads.fetch_add(1, Ordering::Relaxed);
    }
    fn w(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
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
        if !hot_counters() {
            return;
        }
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
    pub crime: std::sync::Arc<Crime>,
    /// MACE's ISA interrupt status and mask, cascading into CRIME.
    pub isa: std::sync::Arc<MaceIsa>,
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
    pub onewire: OneWireId,
    pub com0: Com16550,
    pub com1: Com16550,
    /// The serial ports' DMA rings, which IRIX's console runs on.
    pub sdma: MaceSerialDma,
    /// The audio interface and its AD1843 (dev/mace_audio.rs).
    pub audio: crate::dev::mace_audio::MaceAudio,
    /// The Ethernet. Its frames go nowhere until `start_nat` (IRIS_IP32_NET).
    pub mec: std::sync::Arc<crate::dev::mec::Mec>,
    pub pci_view: PciNativeView,
    /// Accesses that hit nothing, first 64 kept, as `(addr, is_write, pc)`.
    unmapped: Mutex<Vec<(u32, bool, u32)>>,
    /// The ppmem window's base and the RAM bitmap the memory space keeps
    /// (one bit per 64 MB region it has fully mapped), once the machine has
    /// wired them: aligned RAM reads then skip the RAM lock. See `read_bytes`.
    fast_ram: std::sync::OnceLock<(usize, usize)>,
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
    /// `watching` says whether there is one, so the RAM path checks a flag
    /// rather than taking a lock.
    watch: Mutex<Option<(u32, u32)>>,
    watching: AtomicBool,
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
        let crime = std::sync::Arc::new(Crime::new(ust.clone()));
        let isa = std::sync::Arc::new(MaceIsa::new(crime.clone()));
        let bus = Self {
            ram: ram.clone(),
            prom: Mutex::new(prom),
            prom_writes: Mutex::new(Vec::new()),
            flash_file: opts.flash_file.clone(),
            crime,
            isa,
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
            // A locally-administered address; nothing depends on the value yet.
            onewire: OneWireId::from_mac([0x08, 0x00, 0x69, 0x12, 0x34, 0x56]),
            com0: Com16550::named("com0"),
            com1: Com16550::named("com1"),
            sdma: MaceSerialDma::new(),
            audio: crate::dev::mace_audio::MaceAudio::new(),
            mec: std::sync::Arc::new(crate::dev::mec::Mec::new(ram.clone())),
            pci_view: PciNativeView::new(),
            unmapped: Mutex::new(Vec::new()),
            fast_ram: std::sync::OnceLock::new(),
            pc: pc.clone(),
            fw_writes: Mutex::new(0),
            sizemem: Mutex::new(Vec::new()),
            watch: Mutex::new(std::env::var("IRIS_IP32_WATCHMEM").ok().and_then(|v| {
                let (a, b) = v.split_once(':').unwrap_or((&v, &v));
                let p = |x: &str| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok();
                Some((p(a)?, p(b)? + 8))
            })),
            watching: AtomicBool::new(std::env::var("IRIS_IP32_WATCHMEM").is_ok()),
            watch_log: Mutex::new(Vec::new()),
            watch_cap: std::env::var("IRIS_IP32_WATCHMEM_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(256),
            watch_writes_only: std::env::var("IRIS_IP32_WATCHMEM_WRITES").is_ok(),
            scsi: Mutex::new(Vec::new()),
        };
        // An experiment for now: the PROM probes slots 1..3 for a vendor ID
        // and finds nothing, so it has no disk and cannot boot. Putting the
        // controller a real O2 has there is how we learn how much of it the
        // PROM insists on.
        bus.connect_isa_devices();
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

    /// Where CRIME's output goes: the CPU's IP2, for a machine whose CPU
    /// runs on its own thread. Without it (the harness) nothing is pushed.
    pub fn connect_cpu_irq(&self, line: crate::irq::IrqLine) {
        self.crime.connect_output(line);
    }

    /// Let aligned RAM reads go straight to the ppmem window: its base and
    /// the bitmap the memory space publishes (the CPU's, as `ppmem_bitmap_ptr`).
    pub fn set_fast_ram(&self, window: *mut u8, bitmap: *mut u64) {
        if !window.is_null() && !bitmap.is_null() {
            let _ = self.fast_ram.set((window as usize, bitmap as usize));
        }
    }

    fn is_ram_addr(addr: u32) -> bool {
        addr < LOW_ALIAS_SIZE || addr >= RAM_BASE
    }

    #[inline]
    fn before_read(&self, addr: u32) {
        if self.crime.has_output() && self.in_crime(addr) {
            self.refresh_irq();
        }
    }

    /// After a device write, bring interrupts up to date. Not after the
    /// rendering engine's or GBE's registers: neither drives a device line
    /// (GBE's vertical-timing interrupts come from the beam, which the
    /// service thread watches), and a GL scene writes the engine millions
    /// of times a second, each refresh taking the interrupt lock the
    /// service thread holds too.
    #[inline]
    fn after_write(&self, addr: u32) {
        if self.crime.has_output() && !Self::is_ram_addr(addr) && !self.in_crime_re(addr) && !self.in_gbe(addr) {
            self.refresh_irq();
        }
    }

    /// Device lines into CRIME, which hands its output on if it changed.
    /// Returns whether CRIME is interrupting the CPU (IP2).
    pub fn refresh_irq(&self) -> bool {
        self.update_lines();
        self.crime.update_output()
    }

    /// The SCSI controllers, the Ethernet, com0 and the PS/2 ports, into
    /// their CRIME inputs.
    fn update_lines(&self) {
        // GBE's vertical-timing interrupts: the beam crossed a VT_INTRn line.
        let crossed = self.gbe.vt_crossings();
        for n in 0..4 {
            if crossed & (1 << n) != 0 {
                crate::irq::InterruptController::set_line(&*self.crime, 16 + n, true);
            }
        }
        for (scsi, line) in self.scsi.lock().unwrap().iter() {
            crate::irq::InterruptController::set_line(&*self.crime, *line, scsi.interrupting());
        }
        crate::irq::InterruptController::set_line(&*self.crime, IP32_MEC_CRIME_INT, self.mec.interrupting());
        self.com0.update_irq();
        self.sdma.update_irq();
        self.ps2.update_irq();
        self.audio.update_irq();
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
        self.com0.update_irq();
        self.sdma.update_irq();
        BUS_OK
    }

    /// Bring every device interrupt up to date in CRIME: the SCSI
    /// controllers, the Ethernet (and its frames from the network), com0 and
    /// the PS/2 ports. Returns whether CRIME is interrupting the CPU (IP2).
    /// The harness calls it every 256 instructions, the machine from its
    /// service thread.
    pub fn service(&self) -> bool {
        self.mec.poll();
        // Whatever the consoles have typed, into the UARTs.
        self.com0.pump();
        self.com1.pump();
        self.audio.poll(&crate::dev::mace_audio::AudioIo {
            ram: &self.ram,
            ring_base: (self.mace.cell(MACE_ISA_RING_BASE) & sdma::RING_BASE_MASK) as u32,
            ust: self.ust.counter() as u32,
        });
        if self.sdma.any_owned() {
            self.sdma.poll(&self.sdma_io());
        }
        self.refresh_irq()
    }

    /// After a write to MACE's ISA status or mask: a status write clears the
    /// serial DMA engine's pair-request latches it wrote as zero, and a DMA
    /// port's bits are read through the new mask at once.
    fn isa_wrote(&self, addr: u32) {
        if addr & !7 == MACE_ISA_INT_STATUS {
            self.sdma.status_written(self.isa.status_written());
        }
        if self.sdma.any_owned() {
            self.sdma.update_irq();
        }
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
    }

    /// The ISA peripherals' interrupts into MACE's ISA controller: the PS/2
    /// ports, audio, the console UART (while driven by PIO) and serial DMA.
    fn connect_isa_devices(&self) {
        self.ps2.connect_irq(self.isa.clone());
        self.audio.connect_irq(self.isa.clone());
        self.com0.connect_irq(crate::irq::IrqLine::new(self.isa.clone(), ISA_SERIAL0_PIO));
        self.sdma.connect_irq(self.isa.clone());
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
        self.ps2.connect_irq(self.isa.clone());
        self.crime.set_revision(CRIME_REV_GBE);
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
        if !self.watching.load(Ordering::Relaxed) {
            return;
        }
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
        self.watching.store(range.is_some(), Ordering::Relaxed);
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
        self.ust.onewire_busy();
        self.onewire.master_drive(self.ust.guest_us(), low);
    }

    fn nic_read(&self) -> u8 {
        let mut v = self.mace.cell(MACE_ISA_FLASH_NIC_REG) as u8;
        self.ust.onewire_busy();
        if self.onewire.line_level(self.ust.guest_us()) {
            v |= nic_bit::DATA;
        } else {
            v &= !nic_bit::DATA;
        }
        v
    }

    fn in_ust(&self, addr: u32) -> bool {
        addr >= MACE_UST_MSC && addr < MACE_UST_MSC + MACE_UST_MSC_SIZE
    }

    /// Which audio channel's MSC/UST pair a UST-block address names: the
    /// three at 0x20, 0x28 and 0x30 (input, output #1, output #2).
    fn audio_msc_slot(addr: u32) -> Option<usize> {
        let off = addr.wrapping_sub(MACE_UST_MSC);
        (0x20..0x38).contains(&off).then(|| ((off - 0x20) / 8) as usize)
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
        // Mapped RAM, read straight from the window: IRIX's idle loop and
        // uncached kernel loads came here thousands of times a second, each
        // taking the RAM lock and assembling the value a byte at a time.
        // An aligned word is a host u32 in the window's layout (see
        // O2Memory::get_be).
        if let Some(&(win, bm)) = self.fast_ram.get() {
            let bits = unsafe { std::ptr::read_volatile(bm as *const u64) };
            if bits & (1u64 << (addr >> crate::ppmem::BITMAP_SHIFT)) != 0 && addr as usize % n == 0 {
                let p = win + addr as usize;
                let v = unsafe {
                    match n {
                        1 => *((p ^ 3) as *const u8) as u64,
                        2 => ((p ^ 2) as *const u16).read_unaligned() as u64,
                        4 => (p as *const u32).read_unaligned() as u64,
                        _ => ((p as *const u32).read_unaligned() as u64) << 32 | ((p + 4) as *const u32).read_unaligned() as u64,
                    }
                };
                self.watch_hit(addr, false, v, n);
                return Some(v);
            }
        }
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
        if self.in_mec(addr) {
            return BusDevice::read8(&*self.mec, addr);
        }
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
            self.nic_trace.record(self.ust.now(), self.ust.guest_us(), false, v);
            return BusRead8::ok(v);
        }
        if let Some(c) = self.com_for(addr) {
            return c.read8(addr);
        }
        if self.in_pci_mem(addr) {
            return BusRead8::ok(self.pci_mem_read_w(addr, 1) as u8);
        }
        if self.in_ps2(addr) {
            return BusDevice::read8(&self.ps2, addr);
        }
        if self.in_rtc(addr) {
            return self.rtc.read8(addr);
        }
        if crate::dev::mace_audio::MaceAudio::contains(addr) {
            return BusDevice::read8(&self.audio, addr);
        }
        if self.in_sdma(addr) {
            let v = self.sdma_read(addr);
            return BusRead8::ok(v as u8);
        }
        if MaceIsa::contains(addr) {
            self.mace.r();
            self.mace.hit(addr, false);
            // The DMA ports' bits are read as they stand now, not as of the
            // last service tick.
            if self.sdma.any_owned() {
                self.sdma.update_irq();
            }
            return BusDevice::read8(&*self.isa, addr);
        }
        if self.in_mace(addr) {
            return self.mace.read8(addr);
        }
        bus_read!(self, addr, 1, BusRead8::ok, |v: u64| v as u8)
    }

    fn read16_raw(&self, addr: u32) -> BusRead16 {
        if self.in_mec(addr) {
            return BusDevice::read16(&*self.mec, addr);
        }
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
            return BusDevice::read16(&self.ps2, addr);
        }
        if self.in_rtc(addr) {
            return self.rtc.read16(addr);
        }
        if crate::dev::mace_audio::MaceAudio::contains(addr) {
            return BusDevice::read16(&self.audio, addr);
        }
        if self.in_sdma(addr) {
            let v = self.sdma_read(addr);
            return BusRead16::ok(v as u16);
        }
        if MaceIsa::contains(addr) {
            self.mace.r();
            self.mace.hit(addr, false);
            // The DMA ports' bits are read as they stand now, not as of the
            // last service tick.
            if self.sdma.any_owned() {
                self.sdma.update_irq();
            }
            return BusDevice::read16(&*self.isa, addr);
        }
        if self.in_mace(addr) {
            return self.mace.read16(addr);
        }
        bus_read!(self, addr, 2, BusRead16::ok, |v: u64| v as u16)
    }

    fn read32_raw(&self, addr: u32) -> BusRead32 {
        if self.in_mec(addr) {
            return BusDevice::read32(&*self.mec, addr);
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
        if let Some(n) = Self::audio_msc_slot(addr) {
            let v = self.audio.msc_ust(n);
            return BusRead32::ok(if addr & 4 == 0 { (v >> 32) as u32 } else { v as u32 });
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
            self.nic_trace.record(self.ust.now(), self.ust.guest_us(), false, v);
            return BusRead32::ok(v as u32);
        }
        if let Some(c) = self.com_for(addr) {
            return c.read32(addr);
        }
        if self.in_pci_mem(addr) {
            return BusRead32::ok(self.pci_mem_read(addr) as u32);
        }
        if self.in_ps2(addr) {
            return BusDevice::read32(&self.ps2, addr);
        }
        if self.in_rtc(addr) {
            return self.rtc.read32(addr);
        }
        if crate::dev::mace_audio::MaceAudio::contains(addr) {
            return BusDevice::read32(&self.audio, addr);
        }
        if self.in_sdma(addr) {
            // The registers are 64-bit; a 32-bit access is the half it names.
            let v = self.sdma_read(addr);
            return BusRead32::ok(if addr & 4 == 0 { (v >> 32) as u32 } else { v as u32 });
        }
        if MaceIsa::contains(addr) {
            self.mace.r();
            self.mace.hit(addr, false);
            // The DMA ports' bits are read as they stand now, not as of the
            // last service tick.
            if self.sdma.any_owned() {
                self.sdma.update_irq();
            }
            return BusDevice::read32(&*self.isa, addr);
        }
        if self.in_mace(addr) {
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
        if let Some(n) = Self::audio_msc_slot(addr) {
            return BusRead64::ok(self.audio.msc_ust(n));
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
            self.nic_trace.record(self.ust.now(), self.ust.guest_us(), false, v);
            return BusRead64::ok(v as u64);
        }
        if self.in_mec(addr) {
            return BusDevice::read64(&*self.mec, addr);
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
            return BusDevice::read64(&self.ps2, addr);
        }
        if self.in_rtc(addr) {
            return self.rtc.read64(addr);
        }
        if crate::dev::mace_audio::MaceAudio::contains(addr) {
            return BusDevice::read64(&self.audio, addr);
        }
        if self.in_sdma(addr) {
            let v = self.sdma_read(addr);
            return BusRead64::ok(v);
        }
        if MaceIsa::contains(addr) {
            self.mace.r();
            self.mace.hit(addr, false);
            // The DMA ports' bits are read as they stand now, not as of the
            // last service tick.
            if self.sdma.any_owned() {
                self.sdma.update_irq();
            }
            return BusDevice::read64(&*self.isa, addr);
        }
        if self.in_mace(addr) {
            return self.mace.read64(addr);
        }
        let r = bus_read!(self, addr, 8, BusRead64::ok, |v: u64| v);
        self.note_sizemem(addr, false, r.data);
        r
    }

    fn write8_raw(&self, addr: u32, val: u8) -> u32 {
        if self.in_mec(addr) {
            return BusDevice::write8(&*self.mec, addr, val);
        }
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
            self.nic_trace.record(self.ust.now(), self.ust.guest_us(), true, val);
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
            return BusDevice::write8(&self.ps2, addr, val);
        }
        if self.in_rtc(addr) {
            return self.rtc.write8(addr, val);
        }
        if crate::dev::mace_audio::MaceAudio::contains(addr) {
            return BusDevice::write8(&self.audio, addr, val);
        }
        if self.in_sdma(addr) {
            // A narrow store is the low end of the register.
            return self.sdma_write(addr, val as u64);
        }
        if MaceIsa::contains(addr) {
            self.mace.w();
            self.mace.hit(addr, true);
            let r = BusDevice::write8(&*self.isa, addr, val);
            self.isa_wrote(addr);
            return r;
        }
        if self.in_mace(addr) {
            let r = self.mace.write8(addr, val);
            self.mace_wrote(addr);
            return r;
        }
        if self.write_bytes(addr, 1, val as u64) { BUS_OK } else { self.miss(addr, true); BUS_ERR }
    }

    fn write16_raw(&self, addr: u32, val: u16) -> u32 {
        if self.in_mec(addr) {
            return BusDevice::write16(&*self.mec, addr, val);
        }
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
            return BusDevice::write16(&self.ps2, addr, val);
        }
        if self.in_rtc(addr) {
            return self.rtc.write16(addr, val);
        }
        if crate::dev::mace_audio::MaceAudio::contains(addr) {
            return BusDevice::write16(&self.audio, addr, val);
        }
        if self.in_sdma(addr) {
            // A narrow store is the low end of the register.
            return self.sdma_write(addr, val as u64);
        }
        if MaceIsa::contains(addr) {
            self.mace.w();
            self.mace.hit(addr, true);
            let r = BusDevice::write16(&*self.isa, addr, val);
            self.isa_wrote(addr);
            return r;
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
            return BusDevice::write32(&*self.mec, addr, val);
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
        if let Some(n) = Self::audio_msc_slot(addr) {
            let v = self.audio.msc_ust(n);
            let v = if addr & 4 == 0 { (v & 0xffff_ffff) | (val as u64) << 32 } else { (v & !0xffff_ffff) | val as u64 };
            self.audio.set_msc_ust(n, v);
            return BUS_OK;
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
            self.nic_trace.record(self.ust.now(), self.ust.guest_us(), true, val as u8);
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
            return BusDevice::write32(&self.ps2, addr, val);
        }
        if self.in_rtc(addr) {
            return self.rtc.write32(addr, val);
        }
        if crate::dev::mace_audio::MaceAudio::contains(addr) {
            return BusDevice::write32(&self.audio, addr, val);
        }
        if self.in_sdma(addr) {
            // A narrow store is the low end of the register.
            return self.sdma_write(addr, val as u64);
        }
        if MaceIsa::contains(addr) {
            self.mace.w();
            self.mace.hit(addr, true);
            let r = BusDevice::write32(&*self.isa, addr, val);
            self.isa_wrote(addr);
            return r;
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
            return BusDevice::write64(&*self.mec, addr, val);
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
        if let Some(n) = Self::audio_msc_slot(addr) {
            self.audio.set_msc_ust(n, val);
            return BUS_OK;
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
            self.nic_trace.record(self.ust.now(), self.ust.guest_us(), true, val as u8);
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
            return BusDevice::write64(&self.ps2, addr, val);
        }
        if self.in_rtc(addr) {
            return self.rtc.write64(addr, val);
        }
        if crate::dev::mace_audio::MaceAudio::contains(addr) {
            return BusDevice::write64(&self.audio, addr, val);
        }
        if self.in_sdma(addr) {
            return self.sdma_write(addr, val);
        }
        if MaceIsa::contains(addr) {
            self.mace.w();
            self.mace.hit(addr, true);
            let r = BusDevice::write64(&*self.isa, addr, val);
            self.isa_wrote(addr);
            return r;
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
/// The O2's devices answer the monitor through the bus that holds them,
/// as the IOC and HPC3 pass their children's commands through on the
/// other machines.
impl Ip32Bus {
    fn devices(&self) -> Vec<&dyn crate::traits::Device> {
        vec![
            &*self.crime, &self.crime_re, &self.gbe, &self.macepci, &*self.ust, &self.rtc, &self.ps2,
            &self.onewire, &self.com0, &self.com1, &self.sdma, &self.audio, &*self.mec,
        ]
    }
}

impl crate::traits::Device for Ip32Bus {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool { true }
    fn get_clock(&self) -> u64 { 0 }

    fn register_commands(&self) -> Vec<(String, String)> {
        self.devices().iter().flat_map(|d| d.register_commands()).collect()
    }

    fn execute_command(&self, cmd: &str, args: &[&str], writer: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        for d in self.devices() {
            if d.register_commands().iter().any(|(c, _)| c == cmd) {
                return d.execute_command(cmd, args, writer);
            }
        }
        Err("Command not found".to_string())
    }
}

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
mod tests;

#[cfg(test)]
mod bringup;

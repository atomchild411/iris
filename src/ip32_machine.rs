//! The O2 (IP32) as a machine: iris's CPU thread and window around the IP32
//! devices that `ip32.rs` brought up in its test harness.
//!
//! Selected with `[machine] profile = "o2_ip32"` and configured by `[o2]`
//! (see [`crate::config::O2Section`]). What the harness did between
//! instructions is split two ways here:
//!
//! - a service thread keeps the devices' interrupts current in CRIME and
//!   CRIME's in the CPU's IP2, runs the MACE clock from the host clock, and
//!   ends the run when the guest switches the machine off;
//! - a batch hook on the CPU thread switches tcache with where the guest is
//!   running: the cache holds data until POST has finished (the PROM keeps
//!   its stack in it while it tests memory), and again whenever the guest is
//!   back inside the PROM, which sets the caches up the same way to power
//!   off.
//!
//! The screen is GBE scanned out of RAM by [`O2Display`], presented through
//! the same renderer the other machines use; keys from the window go to the
//! MACE PS/2 keyboard.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::config::{CpuModel, MachineConfig};
use crate::cpu::mips_core::CyclesPtr;
use crate::cpu::mips_exec::{CpuDevice, MipsCpu, MipsCpuConfig, MipsExecutor};
use crate::cpu::mips_cache_v2::{MipsCache, R5000Cache};
use crate::ip32_handover::HandoverCache;
use crate::cpu::mips_tlb::MipsTlb;
use crate::dev::crime_mem::O2Memory;
use crate::dev::ng1::rex3::Renderer;
use crate::disp::Rex3Screen;
use crate::hptimer::TimerManager;
use crate::ip32::{Ip32Bus, Ip32Disk, Ip32Options, FIRMWARE_LOAD_VA, FIRMWARE_SPAN};
use crate::traits::{BusDevice, Device, Signal};

const PROM_VA: std::ops::Range<u32> = 0xbfc0_0000..0xbfc8_0000;

/// Whether `pc` is inside the PROM or its firmware copied into RAM.
fn in_firmware(pc: u32) -> bool {
    PROM_VA.contains(&pc) || (FIRMWARE_LOAD_VA..FIRMWARE_LOAD_VA + FIRMWARE_SPAN).contains(&pc)
}

pub struct O2Machine {
    pub bus: Arc<Ip32Bus>,
    cpu: Arc<dyn CpuDevice>,
    timer_manager: Arc<TimerManager>,
    display: Arc<O2Display>,
    running: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl O2Machine {
    pub fn new(cfg: &MachineConfig) -> Result<Self, String> {
        let o2 = &cfg.o2;
        let prom = std::fs::read(&o2.prom).map_err(|e| format!("O2 PROM {}: {e}", o2.prom))?;
        let kits = match &o2.dimms {
            Some(k) => k.clone(),
            None => crate::dev::crime_mem::kits_for(o2.ram_mb)?,
        };
        let mem = O2Memory::from_kits(&kits)?;
        if let Some(d) = o2.disks.iter().find(|d| d.bus > 1) {
            return Err(format!("O2 disk {}: bus {} (an O2 has controllers 0 and 1)", d.path, d.bus));
        }
        let opts = Ip32Options {
            scsi: true,
            disks: o2
                .disks
                .iter()
                .map(|d| Ip32Disk { id: Some(d.id), bus: d.bus, path: d.path.clone().into(), cdrom: d.cdrom })
                .collect(),
            flash_file: o2.flash.clone().map(Into::into),
        };
        let mut bus = Ip32Bus::with_options(mem, prom, opts);
        // MACE audio plays through the host unless audio is off.
        if !cfg.no_audio {
            bus.audio.enable_output(cfg.audio.clone());
        }
        if o2.graphics {
            bus.enable_graphics_with_mouse(o2.mouse);
        }
        let bus = Arc::new(bus);
        eprintln!(
            "iris: O2 with {} MB ({}), {}",
            kits.iter().sum::<u32>(),
            kits.iter().map(|k| format!("{k}")).collect::<Vec<_>>().join("+"),
            if o2.graphics { "graphics console" } else { "serial console" }
        );
        if let Some(port) = o2.serial_port {
            match bus.com0.serve_tcp(port) {
                Ok(p) => eprintln!("iris: O2 serial console on 127.0.0.1:{p}"),
                Err(e) => eprintln!("iris: O2 serial console port {port}: {e}"),
            }
        }
        if let Some(path) = &o2.serial_log {
            if let Err(e) = bus.com0.log_to(std::path::Path::new(path)) {
                eprintln!("iris: O2 serial log {path}: {e}");
            }
        }
        if o2.net {
            let mut net = crate::net::GatewayConfig::default();
            net.port_forwards = o2.forwards.clone();
            eprintln!("iris: O2 Ethernet on NAT: gateway {}, guest {} (DHCP)", net.gateway_ip, net.client_ip);
            bus.mec.start_nat(net);
        }

        let timer_manager = Arc::new(TimerManager::new());
        let sysad: Arc<dyn BusDevice> = bus.clone();
        let arcs_fpu = o2.arcs_fpu;
        let cpu: Arc<dyn CpuDevice> = match cfg.machine.cpu {
            // The R10000 runs the full cache model through the PROM and
            // IP28's tagless shadow model once the kernel is in charge: the
            // JIT then reaches RAM through the ppmem window with no tag probe,
            // LRU or dirty bit on every load and store (memory-bound code ran
            // 2.5-4x slower than on IP28 with the full model).
            CpuModel::R10000 => Self::build_cpu::<HandoverCache>(sysad, &timer_manager, arcs_fpu, o2.cpu_mhz, |c, kernel| {
                if kernel && !c.on_shadow() {
                    c.hand_over();
                    true
                } else if !kernel && c.on_shadow() {
                    c.hand_back();
                    true
                } else {
                    false
                }
            }),
            CpuModel::R5000 => Self::build_cpu::<R5000Cache>(sysad, &timer_manager, arcs_fpu, o2.cpu_mhz, |_, _| false),
            CpuModel::R4400 => return Err("an O2 has an R5000 or an R10000 ([machine] cpu)".into()),
        };

        #[cfg(feature = "jitv2")]
        {
            let cpu_device: Arc<dyn Device> = cpu.clone().as_device();
            let jit = cpu.jitv2();
            let mut g = jit.lock();
            g.compile_queue.set_thread_count(cfg.jitv2.threads.max(1));
            g.compile_queue.set_cpu(Arc::downgrade(&cpu_device));
            g.compile_queue.set_owner(Arc::downgrade(&jit));
            let stats = g.stats.clone();
            g.compile_queue.start(bus.clone() as Arc<dyn BusDevice>, stats);
            // The compile pool keeps only a weak handle on the CPU.
            std::mem::forget(cpu_device);
        }

        // ppmem: the CPU's cache and the JIT reach RAM through the memory's
        // window, publishing the mapped bitmap into the CPU's own fields, as
        // the Indigo2 does. Remaps happen on the CPU thread (a CRIME bank
        // control write), which is the window's contract.
        {
            use crate::ppmem::MappedMemory;
            let ram = bus.ram.lock().unwrap();
            let sp = ram.space();
            unsafe {
                sp.set_bitmap_sink(cpu.ppmem_bitmap_ptr());
                bus.set_fast_ram(sp.window_base(), cpu.ppmem_bitmap_ptr());
                cpu.set_tcache_window(sp.window_base());
                let bitmap = cpu.tcache_bitmap_ptr();
                if !bitmap.is_null() {
                    sp.set_bitmap_sink2(bitmap);
                }
                #[cfg(feature = "jitv2")]
                cpu.set_tcache_gen_window(sp.gen_window_base());
            }
        }

        // CRIME's output is the CPU's IP2, pushed the moment it changes.
        bus.ust.use_host_clock();
        {
            let dev = cpu.clone().as_device();
            bus.set_irq_sink(Box::new(move |level| dev.signal(Signal::Interrupt(2, level))));
        }

        bus.gbe.use_cpu_clock(cpu.cycles_ptr(), o2.cpu_mhz);
        // CRIME's rendering engine can draw on its own thread
        // (IRIS_O2_RE_ASYNC=1), but by default it stays on the CPU's: the
        // real engine checks for conflicts between its queued work and the
        // CPU's memory accesses (DrawMode enNoConflict), which is not
        // modelled, and under the JIT the CPU outran the queue and
        // overwrote pixmap data the desktop's icon transfers had yet to read
        // (the camera and mic icons lost their outlines).
        if std::env::var("IRIS_O2_RE_ASYNC").is_ok() {
            bus.crime_re.start_async(bus.clone(), |b: &Ip32Bus| &b.crime_re);
        }
        let running = Arc::new(AtomicBool::new(false));
        let display = Arc::new(O2Display::new(bus.clone(), cpu.cycles_ptr(), cpu.fasttick_count(), running.clone()));
        if let Some(port) = o2.control_port {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => {
                    eprintln!("iris: O2 control on 127.0.0.1:{port} (shot, type, key, mouse, click, status, pc, regs, peek, scsi, retrace)");
                    let bus = bus.clone();
                    std::thread::Builder::new().name("O2-Control".into()).spawn(move || control_server(l, bus)).unwrap();
                }
                Err(e) => eprintln!("iris: O2 control port {port}: {e}"),
            }
        }
        Ok(Self { bus, cpu, timer_manager, display, running, threads: Vec::new() })
    }

    /// `kernel_mode(cache, in_kernel)` switches the cache model on leaving the
    /// firmware for the kernel (true) and on a PROM starting over (false),
    /// returning whether it changed, so the JIT's memory pointers are
    /// re-published.
    fn build_cpu<C>(sysad: Arc<dyn BusDevice>, tm: &Arc<TimerManager>, arcs_fpu: bool, mhz: u32, kernel_mode: fn(&C, bool) -> bool) -> Arc<dyn CpuDevice>
    where
        C: crate::cpu::mips_cache_v2::CpuModel + From<Arc<dyn BusDevice>> + 'static,
        MipsCpu<MipsTlb, C>: CpuDevice,
    {
        let cfg = MipsCpuConfig::for_model::<C>();
        let tlb = MipsTlb::new(cfg.tlb_entries);
        let mut exec: MipsExecutor<MipsTlb, C> = MipsExecutor::new(sysad, tlb, &cfg);
        exec.core.set_timer_manager(tm.clone());
        // Count runs at half the pipeline clock on both CPUs.
        exec.core.set_count_hz(mhz.max(1) as u64 * 500_000);
        // The PROM runs with the cache holding its data (see the module doc).
        exec.cache.set_tcache_live(false);
        let mut post_done = false;
        let mut inside = true;
        let mut cu1_saved = 0u32;
        exec.batch_hook = Some(Box::new(move |exec: &mut MipsExecutor<MipsTlb, C>| {
            let pc = exec.core.pc as u32;
            LAST_PC.store(pc, Ordering::Relaxed);
            BATCHES.fetch_add(1, Ordering::Relaxed);
            for (slot, v) in LAST_REGS.iter().zip(exec.core.gpr.iter()) {
                slot.store(*v, Ordering::Relaxed);
            }
            LAST_REGS[32].store(exec.core.cp0_status as u64, Ordering::Relaxed);
            LAST_REGS[33].store(exec.core.cp0_cause as u64, Ordering::Relaxed);
            LAST_REGS[34].store(exec.core.cp0_epc, Ordering::Relaxed);
            LAST_REGS[35].store(exec.core.cp0_badvaddr, Ordering::Relaxed);
            // The timer, for the input stall: Count as last materialized,
            // Compare, the pending-interrupt word and the Compare-match count.
            LAST_REGS[36].store(exec.core.cp0_count, Ordering::Relaxed);
            LAST_REGS[37].store(exec.core.cp0_compare, Ordering::Relaxed);
            LAST_REGS[38].store(exec.core.hot.interrupts.load(Ordering::Relaxed), Ordering::Relaxed);
            LAST_REGS[39].store(exec.core.fasttick_count.load(Ordering::Relaxed), Ordering::Relaxed);
            // Instructions retired, for an idle-MIPS reading over the control port.
            LAST_REGS[40].store(exec.core.hot.cycles, Ordering::Relaxed);
            LAST_REGS[41].store(exec.core.cp0_config as u64, Ordering::Relaxed);
            if !post_done {
                // POST is over once the firmware it loaded into RAM runs.
                if (FIRMWARE_LOAD_VA..FIRMWARE_LOAD_VA + FIRMWARE_SPAN).contains(&pc) {
                    post_done = true;
                } else {
                    return;
                }
            }
            // The boot PROM running again (a reboot): the full model, so POST
            // has a cache that holds its stack.
            if PROM_VA.contains(&pc) && kernel_mode(&exec.cache, false) {
                eprintln!("iris: O2 cache: full model again (PC {pc:#010x}, ra {:#010x})", exec.core.gpr[31] as u32);
                post_done = false;
                inside = true;
                exec.cache.set_tcache_live(false);
                #[cfg(feature = "jitv2")]
                exec.install_jit_mem_ptrs();
                return;
            }
            let fw = in_firmware(pc);
            if fw != inside {
                inside = fw;
                exec.cache.set_tcache_live(!fw);
                if !fw && kernel_mode(&exec.cache, true) {
                    eprintln!("iris: O2 cache: shadow model from PC {pc:#010x}");
                    #[cfg(feature = "jitv2")]
                    exec.install_jit_mem_ptrs();
                }
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
            }
        }));
        Arc::new(MipsCpu::new(exec))
    }

    pub fn start(&mut self) {
        self.running.store(true, Ordering::SeqCst);
        let d = self.display.clone();
        self.threads.push(std::thread::Builder::new().name("O2-Display".into()).spawn(move || d.display_loop()).unwrap());
        let bus = self.bus.clone();
        let cpu = self.cpu.clone();
        let running = self.running.clone();
        self.threads.push(
            std::thread::Builder::new()
                .name("O2-Service".into())
                .spawn(move || Self::service_loop(bus, cpu, running))
                .unwrap(),
        );
        self.cpu.clone().as_device().start();
    }

    /// Device interrupts the guest did not cause by an access (network
    /// frames, keys), typing scripts, and the guest's power switch.
    fn service_loop(bus: Arc<Ip32Bus>, cpu: Arc<dyn CpuDevice>, running: Arc<AtomicBool>) {
        let dev = cpu.clone().as_device();
        // IRIS_IP32_KEYS, as in the harness: segments split by `|`, each typed
        // once the guest sits at a prompt (or 30 s after the last one).
        let mut script: std::collections::VecDeque<std::collections::VecDeque<char>> = std::env::var("IRIS_IP32_KEYS")
            .unwrap_or_default()
            .replace("\\r", "\r")
            .replace("\\e", "\x1b")
            .replace("\\b", "\x08")
            .split('|')
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().collect())
            .collect();
        let mut typing = false;
        let mut last_seg: Option<Instant> = None;
        let mut next_key = Instant::now();
        // The PROM's environment lives in its flash: save it once the guest
        // has written there (setenv) and the writing has stopped.
        let mut flash_seen = bus.flash_writes();
        let mut flash_check = Instant::now();
        while running.load(Ordering::Relaxed) {
            if flash_check.elapsed() >= Duration::from_secs(2) {
                flash_check = Instant::now();
                let n = bus.flash_writes();
                if n != flash_seen {
                    flash_seen = n;
                    bus.save_flash();
                }
            }
            if let Some(kb) = bus.ps2.devices() {
                if !typing && !script.is_empty() && kb.scanning()
                    && (bus.ps2.idle_polls() >= 20_000 || last_seg.is_some_and(|t| t.elapsed() > Duration::from_secs(30)))
                {
                    typing = true;
                }
                if typing && Instant::now() >= next_key {
                    let seg = script.front_mut().unwrap();
                    if let Some(ch) = seg.pop_front() {
                        kb.type_char(ch);
                        next_key = Instant::now() + Duration::from_millis(30);
                    }
                    if seg.is_empty() {
                        script.pop_front();
                        typing = false;
                        last_seg = Some(Instant::now());
                        bus.ps2.reset_idle();
                        eprintln!("iris: O2 typed a key segment");
                    }
                }
            }
            // Frames from the network, and any line that changed without a
            // register access (a disk finishing, a key from the window).
            bus.service();
            if bus.rtc.powered_off() {
                eprintln!("iris: the O2 switched itself off");
                dev.stop();
                bus.save_flash();
                running.store(false, Ordering::SeqCst);
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    pub fn stop(&mut self) {
        self.cpu.clone().as_device().stop();
        self.bus.save_flash();
        self.running.store(false, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }

    pub fn get_ps2(&self) -> Arc<crate::dev::ps2::Ps2Controller> {
        match self.bus.ps2.devices() {
            Some(d) => d.clone(),
            // Serial console: a keyboard nothing reads.
            None => Arc::new(crate::dev::ps2::Ps2Controller::new(None)),
        }
    }

    pub fn get_display(&self) -> Arc<dyn crate::gfx_display::GfxDisplay> {
        self.display.clone()
    }

    pub fn get_timer_manager(&self) -> Arc<TimerManager> {
        self.timer_manager.clone()
    }
}

/// GBE's picture, scanned out of RAM about sixty times a second and handed
/// to whatever renderer the front-end installs.
pub struct O2Display {
    bus: Arc<Ip32Bus>,
    renderer: Mutex<Option<Box<dyn Renderer>>>,
    screen: Mutex<Rex3Screen>,
    screenshot_pending: AtomicBool,
    cycles: CyclesPtr,
    /// The CPU's Compare-match count: IRIX's clock tick on the O2, which has
    /// no 8254. The status bar's Hz.
    ticks: Arc<std::sync::atomic::AtomicU64>,
    running: Arc<AtomicBool>,
}

impl O2Display {
    fn new(bus: Arc<Ip32Bus>, cycles: CyclesPtr, ticks: Arc<std::sync::atomic::AtomicU64>, running: Arc<AtomicBool>) -> Self {
        Self {
            bus,
            renderer: Mutex::new(None),
            screen: Mutex::new(Rex3Screen::new()),
            screenshot_pending: AtomicBool::new(false),
            cycles,
            ticks,
            running,
        }
    }

    fn display_loop(&self) {
        let frame_time = Duration::from_micros(16_667);
        let mut status_bar = crate::disp::StatusBar::new();
        let mut overlay = crate::debug_overlay::DebugOverlay::new();
        let mut sbtex = crate::disp::StatusBarTexture::new();
        let mut rgba = vec![0u8; 2048 * 2048 * 4];
        let mut size = (0usize, 0usize);
        let mut shots = 0u32;
        // IRIS_IP32_PNG: the screen as a PNG every few seconds while it
        // changes, for watching a run without its window.
        let png = std::env::var("IRIS_IP32_PNG").ok();
        let mut png_at = Instant::now();
        let mut png_hash = 0u64;
        while self.running.load(Ordering::Relaxed) {
            let start = Instant::now();
            let shown = self.bus.render_screen(&mut rgba);
            if let (Some(p), Some((w, h))) = (png.as_ref(), shown) {
                if png_at.elapsed() >= Duration::from_secs(5) {
                    png_at = Instant::now();
                    let n = (w * h * 4) as usize;
                    let hash = rgba[..n].iter().step_by(61).fold(0u64, |a, &b| a.wrapping_mul(31).wrapping_add(b as u64));
                    if hash != png_hash {
                        png_hash = hash;
                        let _ = crate::ip32_screen::save_png(p, &rgba[..n], w, h);
                    }
                }
            }
            // Off: keep the last size, black.
            let (w, h) = shown.map(|(w, h)| (w as usize, h as usize)).unwrap_or(if size.0 == 0 { (1280, 1024) } else { size });
            {
                let mut screen = self.screen.lock();
                let stride = crate::disp::FB_STRIDE;
                for y in 0..h {
                    for x in 0..w {
                        let v = if shown.is_some() {
                            let o = (y * w + x) * 4;
                            u32::from_le_bytes([rgba[o], rgba[o + 1], rgba[o + 2], 0xff])
                        } else {
                            0xff00_0000
                        };
                        screen.fb_rgb[y * stride + x] = v;
                        screen.rgba[y * stride + x] = v;
                    }
                }
                screen.prebuilt = true;
                screen.width = w;
                screen.height = h;
                screen.status_bar_only = false;
                let take_shot = self.screenshot_pending.swap(false, Ordering::Relaxed);
                let stats = crate::disp::BarStats {
                    now: start,
                    hb: 0,
                    cycles: self.cycles.get(),
                    fasttick: self.ticks.load(Ordering::Relaxed),
                    decoded_delta: 0,
                    l1i_hits: 0,
                    l1i_fetches: 0,
                    uncached: 0,
                    count_hz: 0,
                    gfifo_pending: 0,
                };
                if let Some(r) = self.renderer.lock().as_mut() {
                    if (w, h) != size {
                        r.resize(w, h);
                    }
                    r.present(&mut screen, &mut overlay, &mut status_bar, &mut sbtex, &stats, take_shot, None, None);
                }
                size = (w, h);
                if take_shot {
                    let pixels = screen.rgba.clone();
                    let path = format!("screenshot_{shots:04}.png");
                    shots += 1;
                    std::thread::spawn(move || match crate::disp::save_screenshot(&path, &pixels, w, h) {
                        Ok(()) => println!("iris: screenshot saved to {path}"),
                        Err(e) => println!("iris: screenshot failed: {e}"),
                    });
                }
            }
            let spent = start.elapsed();
            if spent < frame_time {
                std::thread::sleep(frame_time - spent);
            }
        }
        if let Some(r) = self.renderer.lock().as_mut() {
            r.stop();
        }
    }
}

impl crate::gfx_display::GfxDisplay for O2Display {
    fn renderer_slot(&self) -> &Mutex<Option<Box<dyn Renderer>>> {
        &self.renderer
    }
    fn screen(&self) -> &Mutex<Rex3Screen> {
        &self.screen
    }
    fn request_screenshot(&self) {
        self.screenshot_pending.store(true, Ordering::Relaxed);
    }
    fn cycles(&self) -> CyclesPtr {
        self.cycles.clone()
    }
}

/// The control port: one command per line, one reply line each (`ok ...` or
/// `err ...`). For scripts and for an assistant driving the machine while a
/// person watches its window.
///
/// - `shot PATH`: the screen as a PNG.
/// - `type TEXT`: type it on the PS/2 keyboard (`\r` Enter, `\e` Escape,
///   `\b` Backspace, `\t` Tab).
/// - `key NAME`, `keydown NAME`, `keyup NAME`: a key by its winit name
///   (`Enter`, `KeyA`, `F1`, `ShiftLeft`, `ArrowUp`...).
/// - `mouse DX DY [BUTTONS]`: move by DX, DY (screen pixels, y down) with
///   BUTTONS held (1 left, 2 right, 4 middle).
/// - `click [BUTTON]`: press and release (default 1, left).
/// - `status`: what the display is showing, and the GBE registers behind it.
/// Where the CPU was at the end of its last batch, and how many batches it
/// has run: enough to tell a guest spinning in one place from one that is
/// still moving, without stopping it.
pub(crate) static LAST_PC: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static BATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The general registers at the same moment, then Status, Cause, EPC and
/// BadVAddr: what a polling loop is polling.
static LAST_REGS: [std::sync::atomic::AtomicU64; 42] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 42];

fn control_server(listener: std::net::TcpListener, bus: Arc<Ip32Bus>) {
    use std::io::{BufRead, BufReader, Write};
    for conn in listener.incoming().flatten() {
        let bus = bus.clone();
        let _ = std::thread::Builder::new().name("O2-Control-Client".into()).spawn(move || {
            let mut out = match conn.try_clone() {
                Ok(o) => o,
                Err(_) => return,
            };
            let mut rgba = vec![0u8; 2048 * 2048 * 4];
            for line in BufReader::new(conn).lines().map_while(Result::ok) {
                let (cmd, arg) = line.trim_end().split_once(' ').unwrap_or((line.trim_end(), ""));
                let reply = control_command(&bus, cmd, arg, &mut rgba);
                if writeln!(out, "{reply}").is_err() {
                    break;
                }
            }
        });
    }
}

fn control_command(bus: &Ip32Bus, cmd: &str, arg: &str, rgba: &mut [u8]) -> String {
    let kb = bus.ps2.devices();
    match cmd {
        "shot" => match bus.render_screen(rgba) {
            Some((w, h)) => match crate::ip32_screen::save_png(arg, &rgba[..(w * h * 4) as usize], w, h) {
                Ok(()) => format!("ok {w}x{h}"),
                Err(e) => format!("err {e}"),
            },
            None => "err display off".into(),
        },
        "pc" => format!(
            "ok pc 0x{:08x} after {} batches",
            LAST_PC.load(Ordering::Relaxed),
            BATCHES.load(Ordering::Relaxed)
        ),
        // `crimelog PATH`: the guest's recent CRIME interrupt-register
        // accesses, oldest first.
        "nictrace" => {
            let mut t = bus.nic_trace.text();
            for l in bus.onewire.protocol_log() {
                t.push_str(&format!("# {l}\n"));
            }
            match std::fs::write(arg.trim(), t) {
                Ok(()) => "ok".into(),
                Err(e) => format!("err {e}"),
            }
        }
        "crimelog" => match std::fs::write(arg.trim(), bus.crime.irqlog_text()) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err {e}"),
        },
        "regs" => {
            let r: Vec<u64> = LAST_REGS.iter().map(|v| v.load(Ordering::Relaxed)).collect();
            let mut s = format!("ok pc 0x{:08x}", LAST_PC.load(Ordering::Relaxed));
            for (i, v) in r[..32].iter().enumerate() {
                s.push_str(&format!(" r{i}=0x{v:x}"));
            }
            s.push_str(&format!(" sr=0x{:x} cause=0x{:x} epc=0x{:x} badva=0x{:x}", r[32], r[33], r[34], r[35]));
            s.push_str(&format!(" count=0x{:x} compare=0x{:x} irq=0x{:x} ticks={} cycles={} config=0x{:x}", r[36], r[37], r[38], r[39], r[40], r[41]));
            s
        }
        // `peek ADDR LEN PATH`: LEN bytes of the physical address space from
        // ADDR (hex), read as the CPU would, written to PATH as raw big-endian
        // bytes.
        "peek" => {
            let mut it = arg.split_whitespace();
            let addr = it.next().and_then(|a| u32::from_str_radix(a.trim_start_matches("0x"), 16).ok());
            let len = it.next().and_then(|l| l.parse::<u32>().ok());
            match (addr, len, it.next()) {
                (Some(a), Some(n), Some(path)) => {
                    let mut out = Vec::with_capacity(n as usize);
                    for i in (0..n.min(1 << 20)).step_by(4) {
                        out.extend_from_slice(&bus.read32(a.wrapping_add(i)).data.to_be_bytes());
                    }
                    match std::fs::write(path, out) {
                        Ok(()) => "ok".into(),
                        Err(e) => format!("err {e}"),
                    }
                }
                _ => "err usage: peek ADDR LEN PATH".into(),
            }
        }
        // `watch LO HI` (hex, bus addresses: RAM from 0x40000000) arms the
        // memory watch with an empty log; `watch off` stops it.
        "watch" => {
            let v: Vec<u32> = arg.split_whitespace().filter_map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()).collect();
            if arg.trim() == "fb" {
                bus.set_watch(Some((0, 0)));
                return "ok watching the displayed frame buffer".into();
            }
            match v[..] {
                [lo, hi] if lo < hi => {
                    bus.set_watch(Some((lo, hi)));
                    format!("ok watching 0x{lo:08x}..0x{hi:08x}")
                }
                _ => {
                    bus.set_watch(None);
                    "ok watch off".into()
                }
            }
        }
        // `ps2log PATH`: the PS/2 ports' recent traffic, `1>f4` a byte to
        // port 1's device, `1<fa` one from it.
        "ps2log" => match std::fs::write(arg, bus.ps2.traffic_timed()) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err {e}"),
        },
        "watchdump" => match std::fs::write(arg, bus.watch_text()) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err {e}"),
        },
        "retrace" => match std::fs::write(arg, bus.crime_re.trace_text()) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err {e}"),
        },
        "scsi" => {
            let mut text = String::new();
            for (n, (c, _)) in bus.scsi.lock().unwrap().iter().enumerate() {
                for line in c.notes() {
                    text.push_str(&format!("{n}: {line}\n"));
                }
            }
            match std::fs::write(arg, text) {
                Ok(()) => "ok".into(),
                Err(e) => format!("err {e}"),
            }
        }
        "status" => {
            let shown = bus.render_screen(rgba);
            format!("ok display {shown:?}; {}", bus.gbe.summary())
        }
        _ if kb.is_none() => "err no keyboard or mouse (graphics = false)".into(),
        "type" => {
            let kb = kb.unwrap();
            let text = arg.replace("\\r", "\r").replace("\\e", "\x1b").replace("\\b", "\x08").replace("\\t", "\t");
            let mut n = 0;
            for c in text.chars() {
                if c == '\t' {
                    kb.push_kb(winit::keyboard::KeyCode::Tab, true);
                    kb.push_kb(winit::keyboard::KeyCode::Tab, false);
                    n += 1;
                } else if kb.type_char(c) {
                    n += 1;
                }
            }
            format!("ok {n}")
        }
        "key" | "keydown" | "keyup" => match crate::ip32_screen::keycode(arg.trim()) {
            Some(k) => {
                let kb = kb.unwrap();
                if cmd != "keyup" {
                    kb.push_kb(k, true);
                }
                if cmd != "keydown" {
                    kb.push_kb(k, false);
                }
                "ok".into()
            }
            None => format!("err no key {arg:?}"),
        },
        "mouse" => {
            let v: Vec<i32> = arg.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() < 2 {
                return "err mouse DX DY [BUTTONS]".into();
            }
            let buttons = v.get(2).copied().unwrap_or(0) as u8;
            let kb = kb.unwrap();
            // A PS/2 packet carries at most 255 either way: send big moves in steps.
            let (mut dx, mut dy) = (v[0], v[1]);
            loop {
                let (sx, sy) = (dx.clamp(-200, 200), dy.clamp(-200, 200));
                kb.push_mouse_input(buttons, sx, sy, 0);
                dx -= sx;
                dy -= sy;
                if dx == 0 && dy == 0 {
                    break;
                }
            }
            "ok".into()
        }
        "click" => {
            let b: u8 = arg.trim().parse().unwrap_or(1);
            let kb = kb.unwrap();
            kb.push_mouse_input(b, 0, 0, 0);
            kb.push_mouse_input(0, 0, 0, 0);
            "ok".into()
        }
        _ => format!("err unknown command {cmd:?}"),
    }
}

/// Run an O2 from `cfg` until the window closes or the guest switches it off.
pub fn run(cfg: MachineConfig, scale: u32) {
    let headless = cfg.headless;
    let scroll = cfg.mouse_scroll_pixels_per_line;
    let lock_aspect = cfg.lock_aspect_ratio;
    let mut m = match O2Machine::new(&cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("iris: {e}");
            std::process::exit(1);
        }
    };
    m.start();
    if headless {
        eprintln!("iris: O2 running headless");
        std::thread::park();
    } else {
        use winit::event_loop::EventLoop;
        let event_loop = EventLoop::new().unwrap();
        // The window wants the Indy's SCSI controller for its CD menu; the
        // O2's are on PCI, so it gets one with nothing on it.
        let scsi = Arc::new(crate::dev::wd33c93a::Wd33c93a::new(None, None, Arc::new(std::sync::atomic::AtomicU64::new(0))));
        let ui = crate::ui::Ui::new(m.get_ps2(), m.get_display(), scsi, m.get_timer_manager(), &event_loop, scale, scroll, lock_aspect);
        ui.run(event_loop);
    }
    m.stop();
}

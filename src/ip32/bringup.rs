//! Driving the real CPU against the map above, to find out how far the PROM
//! gets and what it touches on the way.
//!
//! These need a PROM image, which is SGI's and is not in the repo. Point
//! `IRIS_IP32_PROM` at one (or drop it at `ip32/ip32prom.rev4.18.bin` beside
//! the checkout) and they run; otherwise they skip, so CI stays green.

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
    // (telnet or nc), alongside any script; IRIS_IP32_CONSOLE=file logs it.
    {
        let mut backend = crate::dev::z85c30::null_serial_backend();
        if let Some(port) = std::env::var("IRIS_IP32_SERIAL").ok().and_then(|v| v.parse::<u16>().ok()) {
            match crate::dev::z85c30::tcp_serial_backend(port) {
                Some((b, port)) => {
                    eprintln!("ip32: com0 console on 127.0.0.1:{port} (telnet 127.0.0.1 {port})");
                    backend = b;
                }
                None => eprintln!("ip32: com0 console on port {port}: unavailable"),
            }
        }
        if let Ok(path) = std::env::var("IRIS_IP32_CONSOLE") {
            let _ = std::fs::File::create(&path);
            match crate::dev::z85c30::TeeBackend::new(backend.clone(), &path) {
                Ok(t) => backend = std::sync::Arc::new(t),
                Err(e) => eprintln!("ip32: console log {path}: {e}"),
            }
        }
        bus.com0.set_backend(backend);
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
            bus.ps2.update_irq();
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
        if let Some((name, f, touched)) = bus.macepci.slot(slot) {
            let t: Vec<String> = touched.iter()
                .map(|(r, w)| format!("{:02x}{}", r, if *w { "W" } else { "R" }))
                .collect();
            eprintln!("ip32: PCI slot {slot} ({name}) config touched: {}", t.join(" "));
            let c = f.config();
            for i in 0..6 {
                let b = c.bar_base(i);
                if b != 0 {
                    eprintln!("   BAR{i} programmed to 0x{b:08x}");
                }
            }
            eprintln!("   command {:#06x}", c.command());
        }
    }
    for scsi in bus.scsi.lock().unwrap().iter() {
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

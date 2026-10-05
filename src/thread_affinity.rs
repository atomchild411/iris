//! Optional host thread affinity for emulator worker threads.

use std::sync::OnceLock;

use crate::config::PerfConfig;

static PERF: OnceLock<PerfConfig> = OnceLock::new();

/// Called once from `Machine::new` with the loaded `[perf]` section.
pub fn init(perf: PerfConfig) {
    let _ = PERF.set(perf);
}

#[derive(Clone, Copy, Debug)]
pub enum PerfRole {
    MipsCpu,
    Rex3Processor,
    Rex3Refresh,
}

/// Pin the current thread when `[perf] thread_affinity` is enabled.
pub fn pin_current(role: PerfRole) {
    let Some(perf) = PERF.get() else { return; };
    if !perf.thread_affinity {
        return;
    }
    let core = match role {
        PerfRole::MipsCpu => perf.cpu_core,
        PerfRole::Rex3Processor => perf.rex3_core,
        PerfRole::Rex3Refresh => perf.refresh_core,
    };
    let Some(core) = core else { return; };
    if apply_core(core) {
        eprintln!("iris: pinned {:?} thread to core {}", role, core);
    }
}

#[cfg(windows)]
fn apply_core(core: u32) -> bool {
    let mask = 1usize << core.min(63);
    unsafe {
        windows_sys::Win32::System::Threading::SetThreadAffinityMask(
            windows_sys::Win32::System::Threading::GetCurrentThread(),
            mask,
        ) != 0
    }
}

#[cfg(target_os = "linux")]
fn apply_core(core: u32) -> bool {
    use std::mem::MaybeUninit;
    let mut set: libc::cpu_set_t = unsafe { MaybeUninit::zeroed().assume_init() };
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core as usize, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) == 0
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn apply_core(_core: u32) -> bool {
    false
}

#[cfg(not(any(windows, unix)))]
fn apply_core(_core: u32) -> bool {
    false
}

/// One-line summary for `perf snapshot`.
pub fn status_line() -> String {
    let Some(perf) = PERF.get() else {
        return "thread_affinity: (not initialized)".to_string();
    };
    if !perf.thread_affinity {
        return "thread_affinity: off".to_string();
    }
    format!(
        "thread_affinity: on  cpu={:?} rex3={:?} refresh={:?}",
        perf.cpu_core, perf.rex3_core, perf.refresh_core
    )
}

/// Exploration: `IRIS_QOS="cpu=interactive,compile=utility"` sets the macOS
/// QoS class of the MIPS CPU thread (`cpu`) and of each jitv2 compile worker
/// (`compile`), from inside the thread. Classes: `interactive`, `initiated`,
/// `default`, `utility`, `background`. QoS is what decides performance versus
/// efficiency cores on Apple Silicon (`THREAD_AFFINITY_POLICY` is ignored
/// there). A no-op elsewhere, and when the variable doesn't name the role.
pub fn apply_qos(role: &str) {
    #[cfg(target_os = "macos")]
    {
        use libc::qos_class_t::*;
        let Ok(spec) = std::env::var("IRIS_QOS") else { return };
        let Some(class) = spec.split(',').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k.trim() == role).then(|| v.trim().to_string())
        }) else { return };
        let qos = match class.as_str() {
            "interactive" => QOS_CLASS_USER_INTERACTIVE,
            "initiated" => QOS_CLASS_USER_INITIATED,
            "default" => QOS_CLASS_DEFAULT,
            "utility" => QOS_CLASS_UTILITY,
            "background" => QOS_CLASS_BACKGROUND,
            other => {
                eprintln!("iris: IRIS_QOS: unknown class {other:?} for {role}");
                return;
            }
        };
        let before = current_qos();
        let rc = unsafe { libc::pthread_set_qos_class_self_np(qos, 0) };
        eprintln!("iris: QoS {role}: {before} -> {} (rc {rc})", current_qos());
    }
    #[cfg(not(target_os = "macos"))]
    let _ = role;
}

#[cfg(target_os = "macos")]
fn current_qos() -> String {
    let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
    let mut prio: libc::c_int = 0;
    unsafe { libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut prio) };
    format!("{class:?}/{prio}")
}

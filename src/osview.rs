//! A live performance panel for the emulator window, in the style of IRIX's
//! `gr_osview`: each bar has a header naming it and its bands, every band name
//! drawn in its band's colour, and the bar below it is split into those bands.
//! Absolute bars scale themselves and show the scale at the right; the MIPS
//! bar is a strip chart, so it shows the last minute instead of one moment.
//!
//! Opened from the STATS button on the status bar or with RCtrl+O
//! (`ui.rs`), drawn into the debug overlay's buffer (`debug_overlay.rs`), so it
//! works on every display (Newport, GR2, IMPACT) without touching them.
//!
//! The numbers come from counters other parts of the emulator publish anyway
//! (`crate::cpu::jit_feedback::JIT_FEEDBACK`, the status bar's MIPS figure,
//! the persistent code cache's counters); this module samples them twice a
//! second and keeps a minute of history.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static PANEL_OPEN: AtomicBool = AtomicBool::new(false);
/// MIPS x10, published by the status bar each time it recomputes it.
pub static MIPS_X10: AtomicU32 = AtomicU32::new(0);
/// Guest instructions retired (`hot.cycles`), published by the status bar
/// every frame.
pub static INSTRS: AtomicU64 = AtomicU64::new(0);
/// Instructions the interpreter executed (`MipsExecutor::step_int`). Written
/// only by the CPU thread, so a plain load and store, no atomic
/// read-modify-write on the interpreter's path.
static INTERPRETED: AtomicU64 = AtomicU64::new(0);

/// `MipsCore::flops`, registered by `MipsCpu::new` (`set_flops_source`).
static FLOPS_SRC: std::sync::atomic::AtomicPtr<u64> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Where the CPU counts its floating-point operations (valid for the life of
/// the process, as `cycles_ptr` is).
pub fn set_flops_source(p: *const u64) {
    FLOPS_SRC.store(p as *mut u64, Relaxed);
}

fn flops_now() -> u64 {
    let p = FLOPS_SRC.load(Relaxed);
    // Written by the CPU thread only; a torn or stale read is just one
    // sample's noise.
    if p.is_null() { 0 } else { unsafe { std::ptr::read_volatile(p) } }
}

/// Floating-point operations in one MIPS instruction: 1 for add, sub, mul,
/// div, sqrt, recip and rsqrt (COP1, single or double), 2 for the
/// multiply-add family (MADD, MSUB, NMADD, NMSUB; COP1X), 0 for everything
/// else (moves, compares, conversions, loads and stores).
#[inline(always)]
pub fn flops_of(raw: u32) -> u32 {
    match raw >> 26 {
        0x11 => {
            let fmt = (raw >> 21) & 0x1F;
            let funct = raw & 0x3F;
            ((fmt == 16 || fmt == 17) && matches!(funct, 0..=4 | 21 | 22)) as u32
        }
        0x13 => (matches!((raw & 0x3F) >> 3, 4..=7) as u32) * 2,
        _ => 0,
    }
}

/// One interpreted instruction (from `step_int`).
#[inline(always)]
pub fn count_interpreted() {
    INTERPRETED.store(INTERPRETED.load(Relaxed).wrapping_add(1), Relaxed);
}

pub fn panel_open() -> bool {
    PANEL_OPEN.load(Relaxed)
}

pub fn toggle_panel() {
    PANEL_OPEN.fetch_xor(true, Relaxed);
}

const SAMPLE_EVERY: Duration = Duration::from_millis(500);
/// A minute of samples.
const HISTORY: usize = 120;

#[derive(Clone, Default)]
struct Counters {
    compiles: u64,
    compile_ns: u64,
    busy_ns: u64,
    cache_lookups: u64,
    cache_hits: u64,
    cache_hit_ns: u64,
    cache_miss_ns: u64,
    flushes: u32,
    instrs: u64,
    interpreted: u64,
    worker_busy_ns: Vec<u64>,
    worker_compiles: Vec<u64>,
    flops: u64,
}

impl Counters {
    fn now() -> Self {
        #[allow(unused_mut)]
        let mut c = Counters {
            instrs: INSTRS.load(Relaxed),
            interpreted: INTERPRETED.load(Relaxed),
            flops: flops_now(),
            ..Default::default()
        };
        #[cfg(feature = "jitv2")]
        {
            let f = &crate::cpu::jit_feedback::JIT_FEEDBACK;
            c.compiles = f.compiles.load(Relaxed);
            c.compile_ns = f.compile_ns.load(Relaxed);
            c.busy_ns = f.busy_ns.load(Relaxed);
            c.flushes = f.flush_events.load(Relaxed);
            let (lookups, hits, hit_ns, miss_ns) = crate::cpu::jitv2::pcache::lookup_counts();
            c.cache_lookups = lookups;
            c.cache_hits = hits;
            c.cache_hit_ns = hit_ns;
            c.cache_miss_ns = miss_ns;
            let n = (f.compile_threads.load(Relaxed) as usize).min(crate::cpu::jit_feedback::MAX_WORKERS);
            c.worker_busy_ns = f.worker_busy_ns[..n].iter().map(|a| a.load(Relaxed)).collect();
            c.worker_compiles = f.worker_compiles[..n].iter().map(|a| a.load(Relaxed)).collect();
        }
        c
    }
}

#[derive(Clone, Default)]
struct Sample {
    mips: f32,
    /// Compile threads busy (0..threads), averaged over the interval.
    busy: f32,
    threads: u32,
    queue: f32,
    arena: f32,
    compiles_s: f32,
    loads_s: f32,
    ms_per_compile: f32,
    hit_frac: f32,
    lookups_s: f32,
    /// Average time of this interval's lookups that hit, and that missed (µs).
    hit_us: f32,
    miss_us: f32,
    flushes: u32,
    /// Share of the interval's instructions that ran compiled.
    compiled_frac: f32,
    /// Per compile worker: busy share of the interval, compiles per second.
    worker_busy: Vec<f32>,
    worker_cps: Vec<f32>,
    mflops: f32,
}

struct State {
    last: Option<(Instant, Counters)>,
    history: VecDeque<Sample>,
    mips_max: f32,
}

static STATE: Mutex<State> = Mutex::new(State { last: None, history: VecDeque::new(), mips_max: 0.0 });

fn sample(st: &mut State) {
    let now = Instant::now();
    if st.last.as_ref().is_some_and(|(t, _)| now.duration_since(*t) < SAMPLE_EVERY) {
        return;
    }
    let c = Counters::now();
    let Some((t0, c0)) = st.last.replace((now, c.clone())) else { return };
    let dt = now.duration_since(t0).as_secs_f32().max(1e-3);
    let compiles = c.compiles.saturating_sub(c0.compiles) as f32;
    let lookups = c.cache_lookups.saturating_sub(c0.cache_lookups) as f32;
    let hits = c.cache_hits.saturating_sub(c0.cache_hits) as f32;
    #[allow(unused_mut)]
    let mut s = Sample {
        mips: MIPS_X10.load(Relaxed) as f32 / 10.0,
        busy: c.busy_ns.saturating_sub(c0.busy_ns) as f32 / 1e9 / dt,
        compiles_s: compiles / dt,
        loads_s: hits / dt,
        ms_per_compile: if compiles > 0.0 { c.compile_ns.saturating_sub(c0.compile_ns) as f32 / 1e6 / compiles } else { 0.0 },
        hit_frac: if lookups > 0.0 { hits / lookups } else { 0.0 },
        lookups_s: lookups / dt,
        hit_us: if hits > 0.0 { c.cache_hit_ns.saturating_sub(c0.cache_hit_ns) as f32 / 1e3 / hits } else { 0.0 },
        miss_us: if lookups > hits {
            c.cache_miss_ns.saturating_sub(c0.cache_miss_ns) as f32 / 1e3 / (lookups - hits)
        } else { 0.0 },
        flushes: c.flushes,
        compiled_frac: {
            let all = c.instrs.saturating_sub(c0.instrs) as f32;
            let int = c.interpreted.saturating_sub(c0.interpreted) as f32;
            if all > 0.0 { (1.0 - int / all).clamp(0.0, 1.0) } else { 0.0 }
        },
        mflops: c.flops.saturating_sub(c0.flops) as f32 / 1e6 / dt,
        worker_busy: c.worker_busy_ns.iter().enumerate()
            .map(|(i, &b)| (b.saturating_sub(c0.worker_busy_ns.get(i).copied().unwrap_or(b)) as f32 / 1e9 / dt).min(1.0))
            .collect(),
        worker_cps: c.worker_compiles.iter().enumerate()
            .map(|(i, &n)| n.saturating_sub(c0.worker_compiles.get(i).copied().unwrap_or(n)) as f32 / dt)
            .collect(),
        ..Default::default()
    };
    #[cfg(feature = "jitv2")]
    {
        let f = &crate::cpu::jit_feedback::JIT_FEEDBACK;
        s.threads = f.compile_threads.load(Relaxed);
        s.queue = f.queue_fill.load(Relaxed) as f32 / 255.0;
        s.arena = f.arena_fill.load(Relaxed) as f32 / 255.0;
    }
    st.mips_max = st.mips_max.max(s.mips);
    st.history.push_back(s);
    while st.history.len() > HISTORY {
        st.history.pop_front();
    }
}

// ---- drawing ------------------------------------------------------------------

/// Colours as the overlay buffer stores them (0xAABBGGRR).
const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    0xFF00_0000 | (b << 16) | (g << 8) | r
}
const BG: u32 = rgb(0, 0, 0);
const FRAME: u32 = rgb(0x80, 0x80, 0x80);
const TITLE: u32 = rgb(0xFF, 0xFF, 0xFF);
const DIM: u32 = rgb(0x50, 0x50, 0x50);
const GREEN: u32 = rgb(0x00, 0xE0, 0x00);
const YELLOW: u32 = rgb(0xFF, 0xFF, 0x00);
const RED: u32 = rgb(0xFF, 0x30, 0x30);
const CYAN: u32 = rgb(0x00, 0xE0, 0xE0);
const MAGENTA: u32 = rgb(0xFF, 0x40, 0xFF);
const BLUE: u32 = rgb(0x50, 0x80, 0xFF);
const ORANGE: u32 = rgb(0xFF, 0xA0, 0x00);

const PANEL_W: usize = 520;
const MARGIN: usize = 8;
const LINE: usize = 16;
const BAR_H: usize = 14;
const STRIP_H: usize = 48;
/// The cache's hit/miss strip chart, and the MFLOPS one.
const CACHE_H: usize = 40;
const FLOPS_H: usize = 40;
/// One compile worker's row: a thin bar beside its number and figures.
const ROW_H: usize = 16;
const ROW_BAR_H: usize = 10;

struct Canvas<'a> {
    buf: &'a mut [u32],
    stride: usize,
    w: usize,
    h: usize,
    font: &'a [u8],
}

impl Canvas<'_> {
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        for py in y..(y + h).min(self.h) {
            let row = py * self.stride;
            for px in x..(x + w).min(self.w) {
                if let Some(p) = self.buf.get_mut(row + px) {
                    *p = c;
                }
            }
        }
    }

    /// Text with the VGA 8x16 font; returns the x after the last glyph.
    fn text(&mut self, x: usize, y: usize, s: &str, c: u32) -> usize {
        let mut tx = x;
        for ch in s.chars() {
            let g = (ch as usize & 0xFF) * 16;
            for row in 0..16 {
                let bits = self.font.get(g + row).copied().unwrap_or(0);
                for col in 0..8 {
                    if bits >> (7 - col) & 1 != 0 && tx + col < self.w && y + row < self.h {
                        if let Some(p) = self.buf.get_mut((y + row) * self.stride + tx + col) {
                            *p = c;
                        }
                    }
                }
            }
            tx += 8;
        }
        tx
    }

    /// A bar header: title, then each band's name in its colour, and an
    /// optional right-aligned note (scale, max, average).
    fn header(&mut self, x: usize, y: usize, title: &str, bands: &[(&str, u32)], note: &str) {
        let mut tx = self.text(x, y, title, TITLE) + 8;
        for (name, c) in bands {
            tx = self.text(tx, y, name, *c) + 8;
        }
        let nx = (x + PANEL_W - 2 * MARGIN).saturating_sub(note.len() * 8);
        self.text(nx.max(tx), y, note, TITLE);
    }

    /// A sectioned bar: bands as fractions of the whole, the rest left empty;
    /// a frame with the heavy lower border gr_osview draws.
    fn bar(&mut self, x: usize, y: usize, w: usize, bands: &[(f32, u32)]) {
        self.fill(x, y, w, BAR_H, FRAME);
        self.fill(x + 1, y + 1, w - 2, BAR_H - 2, BG);
        self.fill(x, y + BAR_H, w, 2, FRAME);
        let inner = (w - 2) as f32;
        let mut bx = x + 1;
        for &(f, c) in bands {
            let bw = (f.clamp(0.0, 1.0) * inner).round() as usize;
            let bw = bw.min(x + w - 1 - bx);
            self.fill(bx, y + 1, bw, BAR_H - 2, c);
            bx += bw;
        }
    }

    /// A strip chart with several series overlaid on one scale: each is
    /// filled at `ALPHA` over whatever is already there, so overlaps show
    /// both, and topped with a solid line so its value stays readable.
    fn strip_overlay(&mut self, x: usize, y: usize, w: usize, h: usize, series: &[(&[f32], u32)], scale: f32) {
        self.fill(x, y, w, h, FRAME);
        self.fill(x + 1, y + 1, w - 2, h - 2, BG);
        self.fill(x, y + h, w, 2, FRAME);
        let cols = (w - 2) / 4;
        let inner_h = (h - 2) as f32;
        // Every fill first, then every line, so no series' line is buried
        // under another's fill.
        for lines in [false, true] {
            for &(values, c) in series {
                for (i, v) in values.iter().rev().take(cols).enumerate() {
                    let vh = ((v / scale.max(1e-6)).clamp(0.0, 1.0) * inner_h).round() as usize;
                    if vh == 0 {
                        continue;
                    }
                    let cx = x + w - 1 - (i + 1) * 4;
                    let top = y + 1 + (h - 2 - vh);
                    if lines {
                        self.fill(cx, top, 4, 1, c);
                        continue;
                    }
                    for py in top + 1..y + h - 1 {
                        for px in cx..cx + 4 {
                            if let Some(p) = self.buf.get_mut(py * self.stride + px) {
                                *p = blend(*p, c, ALPHA);
                            }
                        }
                    }
                }
            }
        }
    }

    /// A thin bar for a per-worker row: frame, bands, no heavy border.
    fn row_bar(&mut self, x: usize, y: usize, w: usize, bands: &[(f32, u32)]) {
        let y = y + (ROW_H - ROW_BAR_H) / 2 - 1;
        self.fill(x, y, w, ROW_BAR_H, FRAME);
        self.fill(x + 1, y + 1, w - 2, ROW_BAR_H - 2, BG);
        let inner = (w - 2) as f32;
        let mut bx = x + 1;
        for &(f, c) in bands {
            let bw = ((f.clamp(0.0, 1.0) * inner).round() as usize).min(x + w - 1 - bx);
            self.fill(bx, y + 1, bw, ROW_BAR_H - 2, c);
            bx += bw;
        }
    }

    /// A strip chart of `values` (oldest first) against `scale`, newest at
    /// the right edge.
    fn strip(&mut self, x: usize, y: usize, w: usize, h: usize, values: &[f32], scale: f32, c: u32) {
        self.fill(x, y, w, h, FRAME);
        self.fill(x + 1, y + 1, w - 2, h - 2, BG);
        self.fill(x, y + h, w, 2, FRAME);
        let cols = (w - 2) / 4;
        let inner_h = (h - 2) as f32;
        for (i, v) in values.iter().rev().take(cols).enumerate() {
            let vh = ((v / scale.max(1e-6)).clamp(0.0, 1.0) * inner_h).round() as usize;
            let cx = x + w - 1 - (i + 1) * 4;
            self.fill(cx, y + 1 + (h - 2 - vh), 4, vh, c);
        }
    }
}

/// Overlaid series' fill opacity (0..=255).
const ALPHA: u32 = 190;

/// `src` over `dst` at `alpha`/255, channel by channel (0xAABBGGRR, opaque).
fn blend(dst: u32, src: u32, alpha: u32) -> u32 {
    let mix = |shift: u32| {
        let d = (dst >> shift) & 0xFF;
        let s = (src >> shift) & 0xFF;
        ((s * alpha + d * (255 - alpha)) / 255) << shift
    };
    0xFF00_0000 | mix(16) | mix(8) | mix(0)
}

/// Microseconds for a header: one decimal under 10 (a lookup served from
/// memory takes well under one), none above.
fn us(v: f32) -> String {
    if v < 10.0 { format!("{v:.1}") } else { format!("{v:.0}") }
}

/// A round number at or above `v` for an auto-scaled bar: 1, 2 or 5 times a
/// power of ten.
fn nice_scale(v: f32) -> f32 {
    let v = v.max(1.0);
    let p = 10f32.powf(v.log10().floor());
    for m in [1.0, 2.0, 5.0, 10.0] {
        if m * p >= v {
            return m * p;
        }
    }
    10.0 * p
}

/// The latest sample as text (`osview` on the monitor), sampling now if due.
pub fn report() -> String {
    let mut st = STATE.lock().unwrap();
    sample(&mut st);
    let Some(s) = st.history.back() else { return "osview: no sample yet (try again in a second)".to_string() };
    format!(
        "MIPS {:.1}  MFLOPS {:.2}  compiled {:.1}%  queue {:.0}%  code area {:.0}%  flushes {}\n\
         compile threads {:.2} of {} busy  compiles/s {:.0}  cache lookups/s {:.0} hits {:.0}%  hit {}us miss {}us",
        s.mips, s.mflops, s.compiled_frac * 100.0, s.queue * 100.0, s.arena * 100.0, s.flushes,
        s.busy, s.threads, s.compiles_s, s.lookups_s, s.hit_frac * 100.0, us(s.hit_us), us(s.miss_us),
    )
}

/// Draw the panel into `buf` (the debug overlay's 0xAABBGGRR buffer, row
/// stride `stride`) for a `width` x `height` display.
pub fn draw(buf: &mut [u32], stride: usize, width: usize, height: usize, font: &[u8]) {
    let mut st = STATE.lock().unwrap();
    sample(&mut st);
    let hist: Vec<Sample> = st.history.iter().cloned().collect();
    let mips_max = st.mips_max;
    drop(st);

    let mut cv = Canvas { buf, stride, w: width, h: height, font };
    let x0 = width.saturating_sub(PANEL_W + MARGIN);
    let y0 = MARGIN;
    let inner_w = PANEL_W - 2 * MARGIN;
    let workers = hist.last().map_or(0, |s| s.worker_busy.len());
    // the MIPS strip, four bars with headers, the worker header and a row per
    // worker, and the cache strip
    let panel_h = 2 * MARGIN + (LINE + 2 + STRIP_H + 10) + (LINE + 2 + FLOPS_H + 10) + 4 * (LINE + 2 + BAR_H + 10)
        + (LINE + 2) + workers * ROW_H + 10 + (LINE + 2 + CACHE_H + 2);
    cv.fill(x0, y0, PANEL_W, panel_h, BG);
    let x = x0 + MARGIN;
    let mut y = y0 + MARGIN;

    let last = hist.last().cloned().unwrap_or_default();
    // gr_osview averages each bar over the last samples so it moves smoothly.
    let avg = |f: &dyn Fn(&Sample) -> f32| -> f32 {
        let n = hist.len().min(4);
        if n == 0 { 0.0 } else { hist.iter().rev().take(n).map(f).sum::<f32>() / n as f32 }
    };

    // MIPS: a strip chart over the last minute.
    let mips: Vec<f32> = hist.iter().map(|s| s.mips).collect();
    let mips_avg = if mips.is_empty() { 0.0 } else { mips.iter().sum::<f32>() / mips.len() as f32 };
    let scale = nice_scale(mips.iter().copied().fold(0.0, f32::max));
    cv.header(x, y, "MIPS", &[("guest", GREEN)],
        &format!("{:.1}  max {:.1}  avg {:.1}  scale {}", last.mips, mips_max, mips_avg, scale));
    y += LINE + 2;
    cv.strip(x, y, inner_w, STRIP_H, &mips, scale, GREEN);
    y += STRIP_H + 10;

    // MFLOPS: floating-point operations (multiply-add counted as two).
    let mflops: Vec<f32> = hist.iter().map(|s| s.mflops).collect();
    let fl_avg = if mflops.is_empty() { 0.0 } else { mflops.iter().sum::<f32>() / mflops.len() as f32 };
    let fl_max = mflops.iter().copied().fold(0.0, f32::max);
    let scale = nice_scale(fl_max);
    cv.header(x, y, "MFLOPS", &[("guest", CYAN)],
        &format!("{:.1}  max {:.1}  avg {:.1}  scale {}", last.mflops, fl_max, fl_avg, scale));
    y += LINE + 2;
    cv.strip(x, y, inner_w, FLOPS_H, &mflops, scale, CYAN);
    y += FLOPS_H + 10;

    // How the guest's instructions ran: compiled code or the interpreter.
    let compiled = avg(&|s| s.compiled_frac);
    cv.header(x, y, "instructions", &[("compiled", BLUE), ("interpreted", ORANGE)],
        &format!("{:.0}% compiled", compiled * 100.0));
    y += LINE + 2;
    cv.bar(x, y, inner_w, &[(compiled, BLUE), (1.0 - compiled, ORANGE)]);
    y += BAR_H + 10;

    // The compile queue, shared by every worker.
    let queue = avg(&|s| s.queue);
    cv.header(x, y, "jit queue", &[("waiting", CYAN)], &format!("{:.0}%", queue * 100.0));
    y += LINE + 2;
    cv.bar(x, y, inner_w, &[(queue, CYAN)]);
    y += BAR_H + 10;

    // The workers draining it: the pool's total, then a row per worker
    // (busy share of the interval, compiles per second).
    let threads = last.threads.max(1) as f32;
    let busy = avg(&|s| s.busy);
    let cps: f32 = avg(&|s| s.worker_cps.iter().sum());
    cv.header(x, y, "jit threads", &[("busy", YELLOW), ("idle", DIM)],
        &format!("{:.1} of {} busy  {:.0} compiles/s", busy.min(threads), last.threads, cps));
    y += LINE + 2;
    for i in 0..workers {
        let wb = avg(&|s| s.worker_busy.get(i).copied().unwrap_or(0.0));
        let wc = avg(&|s| s.worker_cps.get(i).copied().unwrap_or(0.0));
        cv.text(x, y - 1, &format!("{i:>2}"), TITLE);
        let note = format!("{:>3.0}% {:>4.0}/s", wb * 100.0, wc);
        let bw = inner_w - 24 - note.len() * 8 - 8;
        cv.row_bar(x + 24, y, bw, &[(wb, YELLOW), (1.0 - wb, DIM)]);
        cv.text(x + 24 + bw + 8, y - 1, &note, TITLE);
        y += ROW_H;
    }
    y += 10;

    let arena = last.arena;
    cv.header(x, y, "code area", &[("used", MAGENTA)],
        &format!("{:.0}%  flushes {}", arena * 100.0, last.flushes));
    y += LINE + 2;
    cv.bar(x, y, inner_w, &[(arena, MAGENTA)]);
    y += BAR_H + 10;

    // Pages compiled against pages loaded from the persistent cache, per
    // second, auto-scaled.
    let comp = avg(&|s| s.compiles_s);
    let loads = avg(&|s| s.loads_s);
    let peak = hist.iter().map(|s| s.compiles_s + s.loads_s).fold(0.0, f32::max);
    let scale = nice_scale(peak);
    cv.header(x, y, "pages/s", &[("compiled", RED), ("from cache", GREEN)],
        &format!("{:.0} ms/compile  scale {}", avg(&|s| s.ms_per_compile), scale));
    y += LINE + 2;
    cv.bar(x, y, inner_w, &[(comp / scale, RED), (loads / scale, GREEN)]);
    y += BAR_H + 10;

    // The persistent code cache: hits and misses among this interval's
    // lookups.
    let lookups = avg(&|s| s.lookups_s);
    let hit = if lookups > 0.0 { avg(&|s| s.hit_frac) } else { 0.0 };
    // Lookup times, averaged over the samples that had any: a hit lists the
    // page's directory and reads and checks its variant files; a miss is the
    // listing alone, or variants that don't cover the entries.
    let avg_nz = |f: &dyn Fn(&Sample) -> f32| -> f32 {
        let v: Vec<f32> = hist.iter().rev().take(4).map(f).filter(|&x| x > 0.0).collect();
        if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 }
    };
    let (hit_us, miss_us) = (avg_nz(&|s| s.hit_us), avg_nz(&|s| s.miss_us));
    // Hits and misses per second over the last minute, overlaid.
    let hits: Vec<f32> = hist.iter().map(|s| s.loads_s).collect();
    let misses: Vec<f32> = hist.iter().map(|s| (s.lookups_s - s.loads_s).max(0.0)).collect();
    let scale = nice_scale(hits.iter().chain(misses.iter()).copied().fold(0.0, f32::max));
    let note = if lookups > 0.0 {
        format!("{:.0}% of {:.0}/s  hit {}us miss {}us  scale {}", hit * 100.0, lookups, us(hit_us), us(miss_us), scale)
    } else {
        format!("idle  scale {}", scale)
    };
    cv.header(x, y, "jit cache", &[("hit", GREEN), ("miss", RED)], &note);
    y += LINE + 2;
    cv.strip_overlay(x, y, inner_w, CACHE_H, &[(&hits, GREEN), (&misses, RED)], scale);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_floating_point_operations() {
        let cop1 = |fmt: u32, funct: u32| (0x11 << 26) | (fmt << 21) | (2 << 16) | (4 << 11) | (6 << 6) | funct;
        assert_eq!(flops_of(cop1(17, 0)), 1); // add.d
        assert_eq!(flops_of(cop1(16, 2)), 1); // mul.s
        assert_eq!(flops_of(cop1(17, 4)), 1); // sqrt.d
        assert_eq!(flops_of(cop1(17, 6)), 0); // mov.d
        assert_eq!(flops_of(cop1(17, 0x32)), 0); // c.eq.d
        assert_eq!(flops_of(cop1(20, 0x21)), 0); // cvt.d.w
        assert_eq!(flops_of((0x13 << 26) | 0x21), 2); // madd.d
        assert_eq!(flops_of((0x13 << 26) | 0x39), 2); // nmsub.d
        assert_eq!(flops_of((0x13 << 26) | 0x01), 0); // ldxc1
        assert_eq!(flops_of(0x8FBF_0010), 0); // lw ra
    }

    /// Draws the panel with a minute of made-up history; with
    /// `OSVIEW_PNG=<path>` it also writes the result, to look at the layout.
    #[test]
    fn draws_a_panel() {
        {
            let mut st = STATE.lock().unwrap();
            st.history.clear();
            for i in 0..HISTORY {
                let t = i as f32 / HISTORY as f32;
                st.history.push_back(Sample {
                    mips: 40.0 + 20.0 * (t * 9.0).sin().abs(),
                    busy: 2.5 * (1.0 - t),
                    threads: 4,
                    queue: 0.6 * (1.0 - t),
                    arena: 0.2 + 0.7 * t,
                    compiles_s: 180.0 * (1.0 - t),
                    loads_s: 120.0 * t,
                    ms_per_compile: 12.4,
                    hit_frac: 0.4 + 0.5 * t,
                    lookups_s: 250.0,
                    hit_us: 11.0,
                    miss_us: 4.0,
                    flushes: 2,
                    compiled_frac: 0.3 + 0.65 * t,
                    worker_busy: vec![0.9 * (1.0 - t), 0.7 * (1.0 - t), 0.4, 0.1],
                    worker_cps: vec![40.0 * (1.0 - t), 30.0 * (1.0 - t), 18.0, 4.0],
                    mflops: 12.0 + 30.0 * (t * 5.0).sin().abs(),
                });
            }
            st.mips_max = 60.0;
            st.last = Some((Instant::now(), Counters::now()));
        }
        PANEL_OPEN.store(true, Relaxed);
        let (w, h) = (1280usize, 1024usize);
        let mut buf = vec![0u32; 2048 * h];
        draw(&mut buf, 2048, w, h, &crate::vga_font::VGA_8X16);
        assert!(buf.iter().any(|&p| p == GREEN), "the MIPS strip was drawn");
        if let Ok(path) = std::env::var("OSVIEW_PNG") {
            crate::disp::save_screenshot(&path, &buf, w, h).unwrap();
        }
    }
}

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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static PANEL_OPEN: AtomicBool = AtomicBool::new(false);
/// MIPS x10, published by the status bar each time it recomputes it.
pub static MIPS_X10: AtomicU32 = AtomicU32::new(0);

pub fn panel_open() -> bool {
    PANEL_OPEN.load(Relaxed)
}

pub fn toggle_panel() {
    PANEL_OPEN.fetch_xor(true, Relaxed);
}

const SAMPLE_EVERY: Duration = Duration::from_millis(500);
/// A minute of samples.
const HISTORY: usize = 120;

#[derive(Clone, Copy, Default)]
struct Counters {
    compiles: u64,
    compile_ns: u64,
    busy_ns: u64,
    cache_lookups: u64,
    cache_hits: u64,
    flushes: u32,
}

impl Counters {
    fn now() -> Self {
        #[allow(unused_mut)]
        let mut c = Counters::default();
        #[cfg(feature = "jitv2")]
        {
            let f = &crate::cpu::jit_feedback::JIT_FEEDBACK;
            c.compiles = f.compiles.load(Relaxed);
            c.compile_ns = f.compile_ns.load(Relaxed);
            c.busy_ns = f.busy_ns.load(Relaxed);
            c.flushes = f.flush_events.load(Relaxed);
            let (lookups, hits) = crate::cpu::jitv2::pcache::lookup_counts();
            c.cache_lookups = lookups;
            c.cache_hits = hits;
        }
        c
    }
}

#[derive(Clone, Copy, Default)]
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
    flushes: u32,
}

struct State {
    last: Option<(Instant, Counters)>,
    history: VecDeque<Sample>,
    mips_max: f32,
}

static STATE: Mutex<State> = Mutex::new(State { last: None, history: VecDeque::new(), mips_max: 0.0 });

fn sample(st: &mut State) {
    let now = Instant::now();
    if st.last.is_some_and(|(t, _)| now.duration_since(t) < SAMPLE_EVERY) {
        return;
    }
    let c = Counters::now();
    let Some((t0, c0)) = st.last.replace((now, c)) else { return };
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
        flushes: c.flushes,
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

const PANEL_W: usize = 520;
const MARGIN: usize = 8;
const LINE: usize = 16;
const BAR_H: usize = 14;
const STRIP_H: usize = 48;

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

/// Draw the panel into `buf` (the debug overlay's 0xAABBGGRR buffer, row
/// stride `stride`) for a `width` x `height` display.
pub fn draw(buf: &mut [u32], stride: usize, width: usize, height: usize, font: &[u8]) {
    let mut st = STATE.lock().unwrap();
    sample(&mut st);
    let hist: Vec<Sample> = st.history.iter().copied().collect();
    let mips_max = st.mips_max;
    drop(st);

    let mut cv = Canvas { buf, stride, w: width, h: height, font };
    let x0 = width.saturating_sub(PANEL_W + MARGIN);
    let y0 = MARGIN;
    let inner_w = PANEL_W - 2 * MARGIN;
    // the MIPS strip and five bars, each with its header
    let panel_h = 2 * MARGIN + (LINE + 2 + STRIP_H + 10) + 5 * (LINE + 2 + BAR_H + 10) - 8;
    cv.fill(x0, y0, PANEL_W, panel_h, BG);
    let x = x0 + MARGIN;
    let mut y = y0 + MARGIN;

    let last = hist.last().copied().unwrap_or_default();
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

    // Compile threads: busy and idle, as a share of the pool.
    let threads = last.threads.max(1) as f32;
    let busy = avg(&|s| s.busy) / threads;
    cv.header(x, y, "jit threads", &[("busy", YELLOW), ("idle", DIM)],
        &format!("{:.1} of {} busy", busy * threads, last.threads));
    y += LINE + 2;
    cv.bar(x, y, inner_w, &[(busy, YELLOW), (1.0 - busy, DIM)]);
    y += BAR_H + 10;

    // The compile queue and the code area (a flush empties it at 100%).
    let queue = avg(&|s| s.queue);
    cv.header(x, y, "jit queue", &[("waiting", CYAN)], &format!("{:.0}%", queue * 100.0));
    y += LINE + 2;
    cv.bar(x, y, inner_w, &[(queue, CYAN)]);
    y += BAR_H + 10;

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
    let note = if lookups > 0.0 { format!("{:.0}% of {:.0}/s", hit * 100.0, lookups) } else { "idle".to_string() };
    cv.header(x, y, "jit cache", &[("hit", GREEN), ("miss", RED)], &note);
    y += LINE + 2;
    let miss = if lookups > 0.0 { 1.0 - hit } else { 0.0 };
    cv.bar(x, y, inner_w, &[(hit, GREEN), (miss, RED)]);
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    flushes: 2,
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

//! JIT v2 entry-set cost: what does compiling a page with more entry points
//! cost? (Exploration for AOT: static analysis of an IRIX ELF file predicts a
//! page's entries well, but with about five times as many as a run uses.)
//!
//! Reads every blob of a persistent cache directory (`pcache`: page words,
//! runtime entry set, FR mode) and a JSON map of static entry sets per page
//! (`{"<page hash hex>": [word offsets]}`, keyed like the cache's page
//! directories), then walks and compiles each page the way `comp.rs` does,
//! three times: with its runtime entries, with the static set, and with their
//! union (what AOT plus union-on-miss would converge to). Reports covered
//! entries, walked instructions, code bytes and compile time, in total and per
//! page (`--csv <file>`).
//!
//! Usage: jitv2_entrycost <cache dir> <static.json> [--csv out.csv] [--limit N]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use iris::cpu::jitv2::analyzer::{instrs_linear, Analyzer};
use iris::cpu::jitv2::codegen::Codegen;
use iris::cpu::jitv2::comp::max_instrs_per_compile;
use iris::cpu::jitv2::{PhysicalCodePage, BITMAP_WORDS, ENTRIES_PER_PAGE};

/// `pcache`'s blob header: magic 8, format 4, FR 4, fingerprint 16, page
/// hash 16, entries and used bitmaps, instruction count, alignment, code
/// length.
const ENTRIES_AT: usize = 8 + 4 + 4 + 16 + 16;
const HEADER_LEN: usize = ENTRIES_AT + BITMAP_WORDS * 8 * 2 + 4 + 4 + 4;
/// A fresh `Codegen` every this many compiles, so its arena never fills.
const COMPILES_PER_CODEGEN: usize = 300;

#[derive(Default, Clone, Copy)]
struct Res {
    covered: usize,
    instrs: usize,
    bytes: u64,
    time: Duration,
    ok: bool,
}

#[derive(Default)]
struct Sum {
    pages: usize,
    covered: usize,
    instrs: usize,
    bytes: u64,
    time: Duration,
    failed: usize,
}

impl Sum {
    fn add(&mut self, r: &Res) {
        self.pages += 1;
        if !r.ok {
            self.failed += 1;
            return;
        }
        self.covered += r.covered;
        self.instrs += r.instrs;
        self.bytes += r.bytes;
        self.time += r.time;
    }
}

struct Compiler {
    analyzer: Analyzer,
    codegen: Codegen,
    compiles: usize,
}

impl Compiler {
    fn new() -> Self {
        Self { analyzer: Analyzer::with_isa(true), codegen: Codegen::new(), compiles: 0 }
    }

    fn measure(&mut self, words: &[u32; ENTRIES_PER_PAGE], entries: &[u16], fr1: bool) -> Res {
        if self.compiles >= COMPILES_PER_CODEGEN {
            self.codegen = Codegen::new();
            self.compiles = 0;
        }
        let instrs = {
            let w = self.analyzer.walk_multi_entry(words, entries, 0x2000_0000, max_instrs_per_compile());
            instrs_linear(w).count()
        };
        let covered = self.analyzer.covered().len();
        if covered == 0 {
            return Res::default();
        }
        let has_fpu = self.analyzer.has_fpu();
        for attempt in 0..2 {
            let mut owned = self.analyzer.instrs_snapshot();
            let counter = AtomicU64::new(0);
            let mut page = Box::new(PhysicalCodePage::new(0, &counter as *const AtomicU64));
            let t = Instant::now();
            let id = self.codegen.compile_region_uncommitted(&mut owned, fr1, true, has_fpu, &mut *page);
            let time = t.elapsed();
            self.compiles += 1;
            if id.is_some() {
                return Res { covered, instrs, bytes: self.codegen.last_code_size() as u64, time, ok: true };
            }
            if attempt == 0 && self.codegen.last_compile_ran_out_of_memory() {
                self.codegen = Codegen::new();
                self.compiles = 0;
                continue;
            }
            break;
        }
        Res { covered, instrs, ..Default::default() }
    }
}

fn blobs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            blobs(&p, out);
        } else if p.extension().is_some_and(|x| x == "jc" || x == "jh") {
            out.push(p);
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: jitv2_entrycost <cache dir> <static.json> [--csv out.csv] [--limit N]");
        std::process::exit(2);
    }
    let csv = args.iter().position(|a| a == "--csv").map(|i| args[i + 1].clone());
    let limit = args.iter().position(|a| a == "--limit").map(|i| args[i + 1].parse::<usize>().unwrap());
    let statics: HashMap<String, Vec<u16>> =
        serde_json::from_str(&std::fs::read_to_string(&args[2]).expect("read static json")).expect("parse static json");

    // (page hash, FR) -> words and the union of the runtime entries of its variants
    let mut pages: BTreeMap<(String, bool), (Box<[u32; ENTRIES_PER_PAGE]>, [u64; BITMAP_WORDS])> = BTreeMap::new();
    let mut files = Vec::new();
    blobs(Path::new(&args[1]), &mut files);
    for f in &files {
        let Ok(buf) = std::fs::read(f) else { continue };
        if buf.len() < HEADER_LEN + ENTRIES_PER_PAGE * 4 || &buf[..8] != b"IRISJC\0\0" {
            continue;
        }
        let fr1 = u32::from_le_bytes(buf[12..16].try_into().unwrap()) == 1;
        let mut entries = [0u64; BITMAP_WORDS];
        for (i, e) in entries.iter_mut().enumerate() {
            *e = u64::from_le_bytes(buf[ENTRIES_AT + 8 * i..ENTRIES_AT + 8 * i + 8].try_into().unwrap());
        }
        let mut words = Box::new([0u32; ENTRIES_PER_PAGE]);
        for (i, w) in words.iter_mut().enumerate() {
            *w = u32::from_le_bytes(buf[HEADER_LEN + 4 * i..HEADER_LEN + 4 * i + 4].try_into().unwrap());
        }
        let ph = hex(&iris::cpu::jitv2::pcache::page_hash(&words));
        let slot = pages.entry((ph, fr1)).or_insert_with(|| (words, [0u64; BITMAP_WORDS]));
        for (a, b) in slot.1.iter_mut().zip(entries) {
            *a |= b;
        }
    }

    let offsets = |bm: &[u64; BITMAP_WORDS]| -> Vec<u16> {
        (0..ENTRIES_PER_PAGE as u16).filter(|&o| bm[o as usize >> 6] >> (o % 64) & 1 != 0).collect()
    };
    let mut c = Compiler::new();
    let (mut s_rt, mut s_st, mut s_un) = (Sum::default(), Sum::default(), Sum::default());
    let mut rows = String::from("page,fr1,rt_entries,st_entries,rt_covered,st_covered,un_covered,rt_instrs,st_instrs,un_instrs,rt_bytes,st_bytes,un_bytes,rt_us,st_us,un_us\n");
    let mut skipped = 0;
    let mut n = 0;
    for ((ph, fr1), (words, rt_bm)) in &pages {
        let Some(st) = statics.get(ph) else { skipped += 1; continue };
        if limit.is_some_and(|l| n >= l) {
            break;
        }
        n += 1;
        let rt = offsets(rt_bm);
        let mut un = rt.clone();
        un.extend(st.iter().copied());
        un.sort_unstable();
        un.dedup();
        let r = c.measure(words, &rt, *fr1);
        let s = c.measure(words, st, *fr1);
        let u = c.measure(words, &un, *fr1);
        s_rt.add(&r);
        s_st.add(&s);
        s_un.add(&u);
        rows.push_str(&format!(
            "{ph},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            *fr1 as u8, rt.len(), st.len(), r.covered, s.covered, u.covered, r.instrs, s.instrs, u.instrs,
            r.bytes, s.bytes, u.bytes, r.time.as_micros(), s.time.as_micros(), u.time.as_micros(),
        ));
        if n % 250 == 0 {
            eprintln!("{n} pages");
        }
    }
    if let Some(path) = csv {
        std::fs::write(&path, rows).expect("write csv");
    }

    println!("{n} pages compiled three ways ({skipped} without a static set skipped)\n");
    println!("{:<22} {:>9} {:>10} {:>11} {:>12} {:>9} {:>7}", "entry set", "entries", "instrs", "code KB", "bytes/instr", "compile s", "failed");
    for (name, s) in [("runtime", &s_rt), ("static", &s_st), ("static + runtime", &s_un)] {
        println!(
            "{:<22} {:>9} {:>10} {:>11.0} {:>12.1} {:>9.1} {:>7}",
            name, s.covered, s.instrs, s.bytes as f64 / 1024.0,
            s.bytes as f64 / s.instrs.max(1) as f64, s.time.as_secs_f64(), s.failed,
        );
    }
    println!(
        "\nstatic vs runtime: code {:.2}x, instructions {:.2}x, compile time {:.2}x",
        s_st.bytes as f64 / s_rt.bytes.max(1) as f64,
        s_st.instrs as f64 / s_rt.instrs.max(1) as f64,
        s_st.time.as_secs_f64() / s_rt.time.as_secs_f64().max(1e-9),
    );
}

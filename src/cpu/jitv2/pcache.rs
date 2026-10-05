//! Persistent code cache: compiled pages kept on disk across runs, keyed by
//! what they were compiled from. Design, measurements and verification plan:
//! `docs/jitv2-persistent-cache.md`.
//!
//! Opt-in with `[jitv2] cache = true` in `iris.toml` (or the GUI's jitv2
//! section), which `Jitv2Config::apply_env` turns into the `IRIS_JIT_CACHE`
//! env var this module actually reads — that var (and `IRIS_JIT_CACHE_DIR`,
//! `[jitv2] cache_dir`'s counterpart, which moves the cache; default: the
//! platform's user cache directory, `iris/jitv2` inside it; see
//! `default_base`) still work directly too, same override rule as the rest
//! of `[debug]`/`[jitv2]`.
//!
//! Layout, under the base directory:
//!
//! ```text
//! <build-id>/<fingerprint>/<page-hash>-<fr>/<entries-hash>.jc
//! ```
//!
//! - `build-id` is a BLAKE3 hash of the running executable, so a rebuild never
//!   sees another build's code.
//! - `fingerprint` hashes every runtime switch that shapes emitted code
//!   (`Codegen::cache_fingerprint`); a run with `j2 alu off` has its own space.
//! - Each page directory holds that page's variants, one per entry set. A blob
//!   serves any request whose entries are a subset of its own, and a new
//!   variant that covers an older one replaces it.
//!
//! There is no index: a lookup lists one small directory, which is the only
//! filesystem work on a page that has never been cached. Several emulators
//! can share a cache, since every write is a temp file plus a rename.
//!
//! Bounds (`[jitv2] cache_max_mb`, default 1024, and `cache_keep_builds`,
//! default 3; `IRIS_JIT_CACHE_MAX_MB` / `IRIS_JIT_CACHE_KEEP_BUILDS`): a blob
//! is written on probation (`.jc`) and becomes protected (`.jh`) the first
//! time it serves a lookup; every hit also sets its mtime. Over the cap, a
//! collection pass deletes down to 80% of it: build directories no other
//! process is using, oldest first, then probation blobs, then protected ones,
//! least recently used first (`collect`). So a session that compiles many
//! pages once (a package build in the guest) cannot push out the pages every
//! boot reuses.
//!
//! A hit is always verified against the full 4 KB of page words stored in the
//! blob, so no hash collision can serve code for other bytes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::cpu::jitv2::{BITMAP_WORDS, ENTRIES_PER_PAGE};

pub type Fingerprint = [u8; 16];
pub type PageHash = [u8; 16];
pub type Entries = [u64; BITMAP_WORDS];

const MAGIC: [u8; 8] = *b"IRISJC\0\0";
/// Bump when the file layout changes. (A codegen change needs no bump: it
/// changes the executable, and with it the build id.)
const FORMAT: u32 = 1;
/// `[jitv2] cache_max_mb`'s default: the whole cache directory, all builds.
pub const DEFAULT_MAX_MB: u64 = 1024;
/// `[jitv2] cache_keep_builds`'s default.
pub const DEFAULT_KEEP_BUILDS: usize = 3;
/// `[jitv2] cache_ram_mb`'s default: the in-memory copy of recently used blobs.
pub const DEFAULT_RAM_MB: u64 = 256;
/// The startup preload fills the in-memory copy to this share of its cap.
const PRELOAD_PERCENT: usize = 75;
/// A collection pass deletes down to this share of the cap, so the next
/// write doesn't start another one.
const TARGET_PERCENT: u64 = 80;
/// The writer starts a pass after writing this share of the cap.
const COLLECT_EVERY_PERCENT: u64 = 5;
/// A running process touches its build's `.used` this often ...
const HEARTBEAT: Duration = Duration::from_secs(5 * 60);
/// ... and a build touched this recently is in use: never deleted whole.
const IN_USE: Duration = Duration::from_secs(15 * 60);
/// A temp file this old was left by a writer that died.
const STALE_TMP: Duration = Duration::from_secs(60 * 60);
/// A blob not yet reused, and one that has served a lookup.
const PROBATION: &str = "jc";
const PROTECTED: &str = "jh";
/// A page keeps at most this many variants; a lookup reads at most this many.
const MAX_VARIANTS: usize = 16;
const REPORT_EVERY: u64 = 500;

const HEADER_LEN: usize = 8 + 4 + 4 + 16 + 16 + BITMAP_WORDS * 8 * 2 + 4 + 4 + 4;
const WORDS_LEN: usize = ENTRIES_PER_PAGE * 4;
const SUM_LEN: usize = 16;

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = matches!(std::env::var("IRIS_JIT_CACHE").as_deref(), Ok("1") | Ok("on"));
        if on {
            match root() {
                Some(dir) => eprintln!("jitcache: on, {}", dir.display()),
                None => eprintln!("jitcache: requested but no cache directory could be set up; off"),
            }
        }
        on && root().is_some()
    })
}

/// The user cache directory's `iris/jitv2`: `~/Library/Caches` on macOS,
/// `%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or `~/.cache` elsewhere.
fn default_base() -> Option<PathBuf> {
    let cache = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else {
        match std::env::var_os("XDG_CACHE_HOME") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
        }
    };
    Some(cache.join("iris").join("jitv2"))
}

struct Limits {
    max_bytes: u64,
    keep_builds: usize,
    /// The in-memory copy's cap (`[jitv2] cache_ram_mb`); 0 turns it off.
    ram_bytes: usize,
    /// Read recently used blobs into memory at startup (`cache_preload`).
    preload: bool,
}

/// The bounds, from `IRIS_JIT_CACHE_MAX_MB` / `IRIS_JIT_CACHE_KEEP_BUILDS`
/// (`[jitv2] cache_max_mb` / `cache_keep_builds`).
fn limits() -> &'static Limits {
    static L: OnceLock<Limits> = OnceLock::new();
    L.get_or_init(|| {
        let var = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok());
        Limits {
            max_bytes: var("IRIS_JIT_CACHE_MAX_MB").unwrap_or(DEFAULT_MAX_MB).max(1) << 20,
            keep_builds: var("IRIS_JIT_CACHE_KEEP_BUILDS").map_or(DEFAULT_KEEP_BUILDS, |n| n as usize).max(1),
            ram_bytes: (var("IRIS_JIT_CACHE_RAM_MB").unwrap_or(DEFAULT_RAM_MB) as usize) << 20,
            preload: !matches!(std::env::var("IRIS_JIT_CACHE_PRELOAD").as_deref(), Ok("0") | Ok("off") | Ok("false")),
        }
    })
}

struct Dirs {
    base: PathBuf,
    /// `<base>/<build-id>`.
    build: PathBuf,
}

/// The base and this build's directory, created on first use; `None` if they
/// can't be. The first call also starts the collector thread: one pass now,
/// then a heartbeat on `.used` so other processes see this build in use.
fn dirs() -> Option<&'static Dirs> {
    static DIRS: OnceLock<Option<Dirs>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let base = match std::env::var_os("IRIS_JIT_CACHE_DIR") {
            Some(d) => PathBuf::from(d),
            None => default_base()?,
        };
        let exe = std::fs::read(std::env::current_exe().ok()?).ok()?;
        let id = hex(&blake3::hash(&exe).as_bytes()[..16]);
        let build = base.join(id);
        std::fs::create_dir_all(&build).ok()?;
        touch_used(&build);
        let (b, d) = (base.clone(), build.clone());
        let _ = std::thread::Builder::new().name("jitcache-gc".into()).spawn(move || {
            let l = limits();
            collect(&b, &d, l.max_bytes, l.keep_builds, SystemTime::now(), true);
            let files = scan_build(&d, SystemTime::now());
            RAM.lock().unwrap().set_index(&files, &d);
            if l.preload && l.ram_bytes > 0 {
                preload(files, l.ram_bytes * PRELOAD_PERCENT / 100);
            }
            loop {
                std::thread::sleep(HEARTBEAT);
                touch_used(&d);
            }
        });
        Some(Dirs { base, build })
    }).as_ref()
}

/// `<base>/<build-id>`.
fn root() -> Option<&'static PathBuf> {
    dirs().map(|d| &d.build)
}

/// Mark a build as in use now (its `.used` file's mtime).
fn touch_used(build: &Path) {
    if let Ok(f) = std::fs::File::create(build.join(".used")) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn page_hash(words: &[u32; ENTRIES_PER_PAGE]) -> PageHash {
    let mut h = blake3::Hasher::new();
    for w in words {
        h.update(&w.to_le_bytes());
    }
    first16(h.finalize())
}

fn first16(h: blake3::Hash) -> [u8; 16] {
    h.as_bytes()[..16].try_into().unwrap()
}

fn entries_hash(e: &Entries) -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    for w in e {
        h.update(&w.to_le_bytes());
    }
    h.finalize().as_bytes()[..8].try_into().unwrap()
}

fn page_dir(fp: &Fingerprint, ph: &PageHash, fr1: bool) -> Option<PathBuf> {
    Some(root()?.join(hex(fp)).join(format!("{}-{}", hex(ph), fr1 as u8)))
}

fn covers(have: &Entries, want: &Entries) -> bool {
    have.iter().zip(want).all(|(h, w)| w & !h == 0)
}

fn count(e: &Entries) -> u32 {
    e.iter().map(|w| w.count_ones()).sum()
}

// ---- statistics ---------------------------------------------------------

static LOOKUPS: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
/// Same page hash, but the stored words differed: a hash collision, or the
/// fault-injected skip turned off. Should stay 0.
static COMPARE_FAILS: AtomicU64 = AtomicU64::new(0);
static BAD_FILES: AtomicU64 = AtomicU64::new(0);
static STORES: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static UNION_ADDED: AtomicU64 = AtomicU64::new(0);
static LOAD_NS: AtomicU64 = AtomicU64::new(0);
/// Probation blobs promoted to protected by their first hit.
static PROMOTED: AtomicU64 = AtomicU64::new(0);
static EVICTED_FILES: AtomicU64 = AtomicU64::new(0);
static EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);
/// Bytes written since the last collection pass.
static WRITTEN_SINCE_COLLECT: AtomicU64 = AtomicU64::new(0);
/// Hits served from memory; misses the index answered without touching the
/// disk; blobs the startup preload read.
static RAM_HITS: AtomicU64 = AtomicU64::new(0);
static INDEX_MISSES: AtomicU64 = AtomicU64::new(0);
static PRELOADED: AtomicU64 = AtomicU64::new(0);

/// A compile whose output can't be stored (relocations, or a configuration
/// that bakes host addresses).
pub fn note_refused() {
    REFUSED.fetch_add(1, Relaxed);
}

/// Entries union-on-miss added to a compile.
pub fn note_union_added(n: u32) {
    UNION_ADDED.fetch_add(n as u64, Relaxed);
}

pub fn note_load_time(d: std::time::Duration) {
    LOAD_NS.fetch_add(d.as_nanos() as u64, Relaxed);
}

pub fn summary() -> String {
    let (l, h) = (LOOKUPS.load(Relaxed), HITS.load(Relaxed));
    format!(
        "jitcache: lookups={l} hits={h} ({:.1}%) stored={} refused={} union_added={} compare_fail={} bad_files={} load_us_avg={:.0} promoted={} evicted={} ({} MB)",
        if l == 0 { 0.0 } else { 100.0 * h as f64 / l as f64 },
        STORES.load(Relaxed), REFUSED.load(Relaxed), UNION_ADDED.load(Relaxed),
        COMPARE_FAILS.load(Relaxed), BAD_FILES.load(Relaxed),
        if h == 0 { 0.0 } else { LOAD_NS.load(Relaxed) as f64 / h as f64 / 1000.0 },
        PROMOTED.load(Relaxed), EVICTED_FILES.load(Relaxed), EVICTED_BYTES.load(Relaxed) >> 20,
    ) + &format!(" ram_hits={} index_misses={} preloaded={}",
        RAM_HITS.load(Relaxed), INDEX_MISSES.load(Relaxed), PRELOADED.load(Relaxed))
}

// ---- blobs ---------------------------------------------------------------

/// One compiled page, as stored.
pub struct Blob {
    /// Entry points the code has a dispatch case for.
    pub entries: Entries,
    /// Words the compile decoded (the churn-avoidance snapshot's mask).
    pub used: Entries,
    pub instr_count: u32,
    pub align: u32,
    pub words: Box<[u32; ENTRIES_PER_PAGE]>,
    pub code: Vec<u8>,
}

struct Header {
    fr1: bool,
    fp: Fingerprint,
    ph: PageHash,
    entries: Entries,
    used: Entries,
    instr_count: u32,
    align: u32,
    code_len: usize,
}

fn put_entries(out: &mut Vec<u8>, e: &Entries) {
    for w in e {
        out.extend_from_slice(&w.to_le_bytes());
    }
}

fn encode(fp: &Fingerprint, ph: &PageHash, fr1: bool, b: &Blob) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + WORDS_LEN + b.code.len() + SUM_LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(fr1 as u32).to_le_bytes());
    out.extend_from_slice(fp);
    out.extend_from_slice(ph);
    put_entries(&mut out, &b.entries);
    put_entries(&mut out, &b.used);
    out.extend_from_slice(&b.instr_count.to_le_bytes());
    out.extend_from_slice(&b.align.to_le_bytes());
    out.extend_from_slice(&(b.code.len() as u32).to_le_bytes());
    debug_assert_eq!(out.len(), HEADER_LEN);
    for w in b.words.iter() {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.extend_from_slice(&b.code);
    let sum = first16(blake3::hash(&out));
    out.extend_from_slice(&sum);
    out
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        a
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }
    fn arr16(&mut self) -> [u8; 16] {
        self.take(16).try_into().unwrap()
    }
    fn entries(&mut self) -> Entries {
        let mut e = [0u64; BITMAP_WORDS];
        for w in &mut e {
            *w = u64::from_le_bytes(self.take(8).try_into().unwrap());
        }
        e
    }
}

fn decode_header(buf: &[u8]) -> Option<Header> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let mut r = Reader(&buf[..HEADER_LEN]);
    if r.take(8) != MAGIC || r.u32() != FORMAT {
        return None;
    }
    let fr = r.u32();
    if fr > 1 {
        return None;
    }
    Some(Header {
        fr1: fr == 1,
        fp: r.arr16(),
        ph: r.arr16(),
        entries: r.entries(),
        used: r.entries(),
        instr_count: r.u32(),
        align: r.u32(),
        code_len: r.u32() as usize,
    })
}

/// Parse and fully check one file against the key it was found under.
fn decode(buf: &[u8], fp: &Fingerprint, ph: &PageHash, fr1: bool) -> Option<Blob> {
    let h = decode_header(buf)?;
    if h.fr1 != fr1 || &h.fp != fp || &h.ph != ph || !h.align.is_power_of_two() {
        return None;
    }
    if buf.len() != HEADER_LEN + WORDS_LEN + h.code_len + SUM_LEN {
        return None;
    }
    let body = &buf[..buf.len() - SUM_LEN];
    if first16(blake3::hash(body)) != buf[body.len()..] {
        return None;
    }
    let mut words = Box::new([0u32; ENTRIES_PER_PAGE]);
    for (w, c) in words.iter_mut().zip(buf[HEADER_LEN..HEADER_LEN + WORDS_LEN].chunks_exact(4)) {
        *w = u32::from_le_bytes(c.try_into().unwrap());
    }
    Some(Blob {
        entries: h.entries,
        used: h.used,
        instr_count: h.instr_count,
        align: h.align,
        words,
        code: buf[HEADER_LEN + WORDS_LEN..body.len()].to_vec(),
    })
}

fn variant_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_blob(p))
        .take(MAX_VARIANTS)
        .collect()
}

fn is_blob(p: &Path) -> bool {
    p.extension().is_some_and(|x| x == PROBATION || x == PROTECTED)
}

fn discard_bad(path: &Path) {
    BAD_FILES.fetch_add(1, Relaxed);
    let _ = std::fs::remove_file(path);
}

/// Find a stored compile of exactly `words` whose entries cover `want`. When
/// several do, the one with the most entries wins.
pub fn lookup(
    fp: &Fingerprint,
    ph: &PageHash,
    fr1: bool,
    words: &[u32; ENTRIES_PER_PAGE],
    want: &Entries,
) -> Option<Arc<Blob>> {
    let n = LOOKUPS.fetch_add(1, Relaxed) + 1;
    if n % REPORT_EVERY == 0 {
        eprintln!("{}", summary());
    }
    let key = (*fp, *ph, fr1);
    // Memory first: no filesystem call at all.
    {
        let mut ram = RAM.lock().unwrap();
        if let Some(blob) = ram.get(&key, words, want) {
            drop(ram);
            HITS.fetch_add(1, Relaxed);
            RAM_HITS.fetch_add(1, Relaxed);
            if let Some(dir) = page_dir(fp, ph, fr1) {
                send(Job::Touch(dir.join(format!("{}.{PROBATION}", hex(&entries_hash(&blob.entries))))));
            }
            return Some(blob);
        }
        // The index knows this build's pages: one it doesn't list was never
        // stored, so there is nothing to read.
        if ram.known_absent(&key) {
            INDEX_MISSES.fetch_add(1, Relaxed);
            return None;
        }
    }
    let dir = page_dir(fp, ph, fr1)?;
    let mut best: Option<(Blob, PathBuf)> = None;
    for path in variant_files(&dir) {
        let Ok(buf) = std::fs::read(&path) else { continue };
        // Only a covering variant is worth the full check.
        match decode_header(&buf) {
            Some(h) if covers(&h.entries, want) => {}
            Some(_) => continue,
            None => { discard_bad(&path); continue; }
        }
        let Some(blob) = decode(&buf, fp, ph, fr1) else { discard_bad(&path); continue };
        if *blob.words != *words {
            COMPARE_FAILS.fetch_add(1, Relaxed);
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| count(&blob.entries) > count(&b.entries)) {
            best = Some((blob, path));
        }
    }
    let (blob, path) = best?;
    HITS.fetch_add(1, Relaxed);
    send(Job::Touch(path));
    let blob = Arc::new(blob);
    RAM.lock().unwrap().insert(key, blob.clone(), limits().ram_bytes);
    Some(blob)
}

/// Every entry any stored variant of this page has, for union-on-miss. Only
/// headers are read; the walk that follows decides what is really covered.
pub fn known_entries(fp: &Fingerprint, ph: &PageHash, fr1: bool) -> Entries {
    let mut all = [0u64; BITMAP_WORDS];
    {
        let ram = RAM.lock().unwrap();
        let key = (*fp, *ph, fr1);
        ram.union_entries(&key, &mut all);
        if ram.known_absent(&key) {
            return all;
        }
    }
    let Some(dir) = page_dir(fp, ph, fr1) else { return all };
    for path in variant_files(&dir) {
        if let Some(h) = read_header(&path) {
            if &h.fp == fp && &h.ph == ph && h.fr1 == fr1 {
                for (a, e) in all.iter_mut().zip(&h.entries) {
                    *a |= e;
                }
            }
        }
    }
    all
}

// ---- in memory -------------------------------------------------------------

/// A page's key: codegen fingerprint, page hash, FR mode.
type Key = (Fingerprint, PageHash, bool);

struct RamEntry {
    blob: Arc<Blob>,
    bytes: usize,
    last_use: u64,
}

/// The in-memory side of the cache: decoded blobs of recently used pages
/// (bounded by `cache_ram_mb`, least recently used out first), and an index
/// of which pages this build has on disk. Lookups try it before the disk, so
/// a page used once this session, or preloaded at startup, never needs the
/// filesystem again, and a page never stored costs no filesystem call once
/// the index is built. Both are hints: a blob is still checked against the
/// live page words, and a page the index lists that has gone from disk is an
/// ordinary miss.
struct Ram {
    pages: std::collections::HashMap<Key, Vec<RamEntry>>,
    bytes: usize,
    clock: u64,
    /// `None` until the first scan of this build's directory.
    on_disk: Option<std::collections::HashSet<Key>>,
}

static RAM: std::sync::LazyLock<Mutex<Ram>> = std::sync::LazyLock::new(|| Mutex::new(Ram::new()));

fn blob_bytes(b: &Blob) -> usize {
    b.code.len() + WORDS_LEN + HEADER_LEN
}

impl Ram {
    fn new() -> Self {
        Ram { pages: Default::default(), bytes: 0, clock: 0, on_disk: None }
    }

    /// The variant of `key` that covers `want` with the most entries, if its
    /// stored words are these.
    fn get(&mut self, key: &Key, words: &[u32; ENTRIES_PER_PAGE], want: &Entries) -> Option<Arc<Blob>> {
        self.clock += 1;
        let clock = self.clock;
        let vs = self.pages.get_mut(key)?;
        let best = vs.iter_mut()
            .filter(|e| covers(&e.blob.entries, want) && *e.blob.words == *words)
            .max_by_key(|e| count(&e.blob.entries))?;
        best.last_use = clock;
        Some(best.blob.clone())
    }

    /// Add a variant: like the disk, it replaces those it covers. Then the
    /// least recently used variants go until the total is under `cap`.
    fn insert(&mut self, key: Key, blob: Arc<Blob>, cap: usize) {
        if cap == 0 {
            return;
        }
        self.clock += 1;
        let bytes = blob_bytes(&blob);
        let vs = self.pages.entry(key).or_default();
        let before: usize = vs.iter().map(|e| e.bytes).sum();
        vs.retain(|e| !covers(&blob.entries, &e.blob.entries));
        let after: usize = vs.iter().map(|e| e.bytes).sum();
        vs.push(RamEntry { blob, bytes, last_use: self.clock });
        self.bytes = self.bytes + after + bytes - before;
        if self.bytes > cap {
            self.evict_to(cap * 9 / 10);
        }
    }

    fn evict_to(&mut self, target: usize) {
        let mut all: Vec<(u64, Key, usize)> = self.pages.iter()
            .flat_map(|(k, vs)| vs.iter().enumerate().map(move |(i, e)| (e.last_use, *k, i)))
            .collect();
        all.sort_by_key(|(t, _, _)| *t);
        let mut gone: std::collections::HashMap<Key, Vec<usize>> = Default::default();
        let mut bytes = self.bytes;
        for (_, k, i) in all {
            if bytes <= target {
                break;
            }
            bytes -= self.pages[&k][i].bytes;
            gone.entry(k).or_default().push(i);
        }
        for (k, mut idx) in gone {
            idx.sort_unstable_by(|a, b| b.cmp(a));
            if let Some(vs) = self.pages.get_mut(&k) {
                for i in idx {
                    vs.swap_remove(i);
                }
                if vs.is_empty() {
                    self.pages.remove(&k);
                }
            }
        }
        self.bytes = bytes;
    }

    fn union_entries(&self, key: &Key, all: &mut Entries) {
        for e in self.pages.get(key).into_iter().flatten() {
            for (a, x) in all.iter_mut().zip(&e.blob.entries) {
                *a |= x;
            }
        }
    }

    /// The index exists and doesn't list `key`.
    fn known_absent(&self, key: &Key) -> bool {
        self.on_disk.as_ref().is_some_and(|set| !set.contains(key))
    }

    fn note_on_disk(&mut self, key: Key) {
        if let Some(set) = self.on_disk.as_mut() {
            set.insert(key);
        }
    }

    /// Rebuild the index from a scan of this build's directory.
    fn set_index(&mut self, files: &[FileInfo], build: &Path) {
        self.on_disk = Some(files.iter().filter_map(|f| key_of(&f.path, build)).collect());
    }
}

fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A blob path's key: `<build>/<fingerprint>/<page hash>-<fr>/<file>`.
fn key_of(path: &Path, build: &Path) -> Option<Key> {
    let rel = path.strip_prefix(build).ok()?;
    let mut parts = rel.iter().map(|c| c.to_str());
    let fp = unhex16(parts.next()??)?;
    let (ph, fr) = parts.next()??.rsplit_once('-')?;
    Some((fp, unhex16(ph)?, fr == "1"))
}

/// Read blobs into memory at startup, protected (reused) ones first and the
/// most recently used of each first, until `budget` bytes. Runs on the
/// collector thread, before anything else it does, so it competes with
/// nothing but the guest's own early boot.
fn preload(mut files: Vec<FileInfo>, budget: usize) {
    let Some(d) = dirs() else { return };
    files.sort_by_key(|f| (std::cmp::Reverse(f.protected), std::cmp::Reverse(f.mtime)));
    let mut loaded = 0usize;
    for f in files {
        if loaded >= budget {
            break;
        }
        let Some(key) = key_of(&f.path, &d.build) else { continue };
        let Ok(buf) = std::fs::read(&f.path) else { continue };
        let Some(blob) = decode(&buf, &key.0, &key.1, key.2) else { continue };
        loaded += blob_bytes(&blob);
        RAM.lock().unwrap().insert(key, Arc::new(blob), limits().ram_bytes);
        PRELOADED.fetch_add(1, Relaxed);
    }
}

/// (bytes held, cap) of the in-memory copy, for status.
pub fn ram_usage() -> (usize, usize) {
    (RAM.lock().unwrap().bytes, limits().ram_bytes)
}

// ---- writer --------------------------------------------------------------

enum Job {
    Store { fp: Fingerprint, ph: PageHash, fr1: bool, blob: Arc<Blob> },
    /// A blob served a lookup: promote it and mark it recently used.
    Touch(PathBuf),
}

/// Queue a successful compile for writing. Returns at once; a background
/// thread does the filesystem work.
pub fn store(fp: Fingerprint, ph: PageHash, fr1: bool, blob: Blob) {
    let blob = Arc::new(blob);
    RAM.lock().unwrap().insert((fp, ph, fr1), blob.clone(), limits().ram_bytes);
    send(Job::Store { fp, ph, fr1, blob });
}

fn send(job: Job) {
    static TX: OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("jitcache-writer".into())
            .spawn(move || {
                for job in rx {
                    match job {
                        Job::Store { fp, ph, fr1, blob } => write_job(fp, ph, fr1, &blob),
                        Job::Touch(path) => touch(&path),
                    }
                }
            })
            .expect("spawn jitcache writer");
        std::sync::Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send(job);
}

/// A blob served a lookup: a probation blob becomes protected, and either way
/// its mtime becomes now, its recency for eviction. A blob deleted meanwhile
/// is simply gone.
fn touch(path: &Path) {
    let mut path = path.to_path_buf();
    if !path.exists() && path.extension().is_some_and(|x| x == PROBATION) {
        path = path.with_extension(PROTECTED);
    }
    if path.extension().is_some_and(|x| x == PROBATION) {
        let to = path.with_extension(PROTECTED);
        if std::fs::rename(&path, &to).is_err() {
            return;
        }
        PROMOTED.fetch_add(1, Relaxed);
        path = to;
    }
    if let Ok(f) = std::fs::File::options().write(true).open(&path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn write_job(fp: Fingerprint, ph: PageHash, fr1: bool, blob: &Blob) {
    let Some(dir) = page_dir(&fp, &ph, fr1) else { return };
    let Some(written) = write_blob(&dir, &fp, &ph, fr1, blob) else { return };
    STORES.fetch_add(1, Relaxed);
    RAM.lock().unwrap().note_on_disk((fp, ph, fr1));
    let l = limits();
    let since = WRITTEN_SINCE_COLLECT.fetch_add(written, Relaxed) + written;
    if since >= l.max_bytes * COLLECT_EVERY_PERCENT / 100 {
        WRITTEN_SINCE_COLLECT.store(0, Relaxed);
        if let Some(d) = dirs() {
            collect(&d.base, &d.build, l.max_bytes, l.keep_builds, SystemTime::now(), true);
            // Eviction removed files; the index follows the disk again.
            let files = scan_build(&d.build, SystemTime::now());
            RAM.lock().unwrap().set_index(&files, &d.build);
        }
    }
}

/// Write one variant into its page directory: temp file, rename, then drop
/// the variants it covers and trim to `MAX_VARIANTS`. Returns the bytes
/// written.
fn write_blob(dir: &Path, fp: &Fingerprint, ph: &PageHash, fr1: bool, blob: &Blob) -> Option<u64> {
    std::fs::create_dir_all(dir).ok()?;
    let bytes = encode(fp, ph, fr1, blob);
    let stem = hex(&entries_hash(&blob.entries));
    // The variants the new one covers can never be chosen over it again, so
    // they go once it is written. It takes their place, and so keeps any
    // protection they earned: union-on-miss rewrites a page that boot reuses
    // with more entries, and that must not put the page back on probation.
    let covered: Vec<PathBuf> = variant_files(dir).into_iter()
        .filter(|p| !p.file_stem().is_some_and(|n| n == stem.as_str()))
        .filter(|p| read_header(p).is_some_and(|h| covers(&blob.entries, &h.entries)))
        .collect();
    let protected = dir.join(format!("{stem}.{PROTECTED}")).exists()
        || covered.iter().any(|p| p.extension().is_some_and(|x| x == PROTECTED));
    let name = format!("{stem}.{}", if protected { PROTECTED } else { PROBATION });
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_err() || std::fs::rename(&tmp, dir.join(&name)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    for path in covered {
        let _ = std::fs::remove_file(&path);
    }
    if protected {
        // The same entry set still on probation is now a duplicate.
        let _ = std::fs::remove_file(dir.join(format!("{stem}.{PROBATION}")));
    }
    trim_variants(dir, &stem);
    Some(bytes.len() as u64)
}

fn read_header(path: &Path) -> Option<Header> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; HEADER_LEN];
    std::io::Read::read_exact(&mut f, &mut buf).ok()?;
    decode_header(&buf)
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Keep at most `MAX_VARIANTS` in a page directory (a lookup reads no more):
/// past that, the variants with the fewest entries go first, then the least
/// recently used. `keep` (the one just written) always stays.
fn trim_variants(dir: &Path, keep: &str) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut all: Vec<(u32, SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_blob(p) && !p.file_stem().is_some_and(|n| n == keep))
        .map(|p| (read_header(&p).map_or(0, |h| count(&h.entries)), mtime(&p), p))
        .collect();
    if all.len() < MAX_VARIANTS {
        return;
    }
    all.sort_by_key(|a| (a.0, a.1));
    for (_, _, p) in all.iter().take(all.len() + 1 - MAX_VARIANTS) {
        let _ = std::fs::remove_file(p);
    }
}

// ---- bounds --------------------------------------------------------------

/// What a collection pass found, and what it deleted.
#[derive(Default, Debug)]
pub struct Usage {
    /// Bytes in blobs after the pass, and how they split.
    pub total: u64,
    pub probation: u64,
    pub protected: u64,
    /// Per build directory: (name, bytes, is this process's build).
    pub builds: Vec<(String, u64, bool)>,
    pub evicted_files: u64,
    pub evicted_bytes: u64,
    /// Another process held the lock, so this pass did nothing.
    pub skipped: bool,
}

struct FileInfo {
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
    protected: bool,
}

struct BuildInfo {
    name: String,
    path: PathBuf,
    used: SystemTime,
    files: Vec<FileInfo>,
    gone: bool,
}

impl BuildInfo {
    fn bytes(&self) -> u64 {
        if self.gone { 0 } else { self.files.iter().map(|f| f.size).sum() }
    }
}

/// What a file takes on disk: its allocated blocks where the platform says,
/// so the cap matches `du`, otherwise its length.
fn disk_size(m: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        m.len()
    }
}

/// Every blob under one build directory; stale temp files are deleted on the
/// way.
fn scan_build(build: &Path, now: SystemTime) -> Vec<FileInfo> {
    let mut out = Vec::new();
    let dirs = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d).into_iter().flatten().filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path()).collect()
    };
    for fp in dirs(build) {
        for page in dirs(&fp) {
            for e in std::fs::read_dir(&page).into_iter().flatten().filter_map(|e| e.ok()) {
                let path = e.path();
                let Ok(m) = e.metadata() else { continue };
                let mt = m.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if is_blob(&path) {
                    let protected = path.extension().is_some_and(|x| x == PROTECTED);
                    out.push(FileInfo { path, size: disk_size(&m), mtime: mt, protected });
                } else if path.extension().is_some_and(|x| x == "tmp")
                    && now.duration_since(mt).unwrap_or_default() > STALE_TMP
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    out
}

/// One collection pass over the cache at `base`, whose running build is
/// `current`. With `evict`, it keeps the `keep_builds` most recently used
/// builds and, over `max_bytes`, deletes down to `TARGET_PERCENT` of it:
///
/// 1. builds other than `current` that no process has touched within
///    `IN_USE`, least recently used first;
/// 2. probation blobs, oldest mtime first;
/// 3. protected blobs, oldest mtime first.
///
/// A build in use by another process is never deleted whole (its blobs can
/// still go in steps 2 and 3, and a deleted blob is only a miss). One pass at
/// a time across processes (a lock on `<base>/.lock`). Without `evict` it
/// only measures.
pub fn collect(base: &Path, current: &Path, max_bytes: u64, keep_builds: usize, now: SystemTime, evict: bool) -> Usage {
    let mut u = Usage::default();
    let _lock = if evict {
        let Ok(f) = std::fs::File::create(base.join(".lock")) else { return u };
        if f.try_lock().is_err() {
            u.skipped = true;
            return u;
        }
        Some(f)
    } else {
        None
    };
    let in_use = |b: &BuildInfo| b.path == current || now.duration_since(b.used).unwrap_or_default() < IN_USE;

    let mut builds: Vec<BuildInfo> = std::fs::read_dir(base).into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let path = e.path();
            BuildInfo {
                name: e.file_name().to_string_lossy().into_owned(),
                used: mtime(&path.join(".used")),
                files: scan_build(&path, now),
                path,
                gone: false,
            }
        })
        .collect();
    // Most recently used first; the running build counts as the most recent.
    builds.sort_by_key(|b| std::cmp::Reverse((b.path == current, b.used)));

    let remove_build = |b: &mut BuildInfo, u: &mut Usage| {
        if std::fs::remove_dir_all(&b.path).is_ok() {
            u.evicted_files += b.files.len() as u64;
            u.evicted_bytes += b.bytes();
            b.gone = true;
        }
    };
    if evict {
        for b in builds.iter_mut().skip(keep_builds) {
            if !in_use(b) {
                remove_build(b, &mut u);
            }
        }
    }
    let mut total: u64 = builds.iter().map(|b| b.bytes()).sum();
    if evict && total > max_bytes {
        let target = max_bytes * TARGET_PERCENT / 100;
        for b in builds.iter_mut().rev() {
            if total <= target {
                break;
            }
            if !b.gone && !in_use(b) {
                total -= b.bytes();
                remove_build(b, &mut u);
            }
        }
        for protected in [false, true] {
            let mut files: Vec<&mut FileInfo> = builds.iter_mut().filter(|b| !b.gone)
                .flat_map(|b| b.files.iter_mut()).filter(|f| f.protected == protected).collect();
            files.sort_by_key(|f| f.mtime);
            for f in files {
                if total <= target {
                    break;
                }
                if std::fs::remove_file(&f.path).is_ok() {
                    total -= f.size;
                    u.evicted_files += 1;
                    u.evicted_bytes += f.size;
                    f.size = 0;
                    // An emptied page directory, and then its fingerprint
                    // directory, go too (remove_dir fails while not empty).
                    if let Some(page) = f.path.parent() {
                        if std::fs::remove_dir(page).is_ok() {
                            let _ = page.parent().map(std::fs::remove_dir);
                        }
                    }
                }
            }
        }
    }
    for b in &builds {
        if b.gone {
            continue;
        }
        for f in &b.files {
            if f.protected { u.protected += f.size } else { u.probation += f.size }
        }
        u.builds.push((b.name.clone(), b.bytes(), b.path == current));
    }
    u.total = u.probation + u.protected;
    EVICTED_FILES.fetch_add(u.evicted_files, Relaxed);
    EVICTED_BYTES.fetch_add(u.evicted_bytes, Relaxed);
    u
}

// ---- monitor -------------------------------------------------------------

fn describe(u: &Usage, max_bytes: u64) -> String {
    let mb = |b: u64| b as f64 / (1u64 << 20) as f64;
    let mut s = format!(
        "{}\njitcache: {:.1} MB of {:.0} MB ({:.1} MB probation, {:.1} MB protected)",
        summary(), mb(u.total), mb(max_bytes), mb(u.probation), mb(u.protected),
    );
    let (ram, cap) = ram_usage();
    s.push_str(&format!("\n  in memory: {:.1} MB of {:.0} MB", mb(ram as u64), mb(cap as u64)));
    for (name, bytes, current) in &u.builds {
        s.push_str(&format!("\n  {name}  {:.1} MB{}", mb(*bytes), if *current { "  (this build)" } else { "" }));
    }
    if u.evicted_files > 0 {
        s.push_str(&format!("\n  evicted {} blobs, {:.1} MB", u.evicted_files, mb(u.evicted_bytes)));
    }
    if u.skipped {
        s.push_str("\n  another process is collecting; nothing done");
    }
    s
}

/// `jitcache [status|prune|clear]` on the monitor. `None` when the cache is
/// off.
pub fn monitor(arg: Option<&str>) -> Option<Result<String, String>> {
    if !enabled() {
        return None;
    }
    let d = dirs()?;
    let l = limits();
    Some(match arg.unwrap_or("status") {
        "status" => Ok(describe(&collect(&d.base, &d.build, l.max_bytes, l.keep_builds, SystemTime::now(), false), l.max_bytes)),
        "prune" => Ok(describe(&collect(&d.base, &d.build, l.max_bytes, l.keep_builds, SystemTime::now(), true), l.max_bytes)),
        "clear" => {
            // This build's blobs only: other builds may belong to running
            // processes.
            for fp in std::fs::read_dir(&d.build).into_iter().flatten().filter_map(|e| e.ok()) {
                if fp.file_type().is_ok_and(|t| t.is_dir()) {
                    let _ = std::fs::remove_dir_all(fp.path());
                }
            }
            Ok(describe(&collect(&d.base, &d.build, l.max_bytes, l.keep_builds, SystemTime::now(), false), l.max_bytes))
        }
        other => Err(format!("jitcache: unknown '{other}'; usage: jitcache [status|prune|clear]")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(entries: Entries, seed: u32) -> Blob {
        let mut words = Box::new([0u32; ENTRIES_PER_PAGE]);
        for (i, w) in words.iter_mut().enumerate() {
            *w = seed.wrapping_mul(2654435761).wrapping_add(i as u32);
        }
        Blob { entries, used: [!0; BITMAP_WORDS], instr_count: 7, align: 16, words, code: vec![0xd5, 0x03, 0x20, 0x1f, 1, 2, 3] }
    }

    #[test]
    fn roundtrip_and_tamper() {
        let fp = [1u8; 16];
        let mut e = [0u64; BITMAP_WORDS];
        e[0] = 0b1011;
        let b = blob(e, 5);
        let ph = page_hash(&b.words);
        let bytes = encode(&fp, &ph, true, &b);
        let back = decode(&bytes, &fp, &ph, true).expect("decodes");
        assert_eq!(back.entries, b.entries);
        assert_eq!(back.code, b.code);
        assert_eq!(*back.words, *b.words);
        assert_eq!(back.align, 16);
        // Wrong key, wrong FR, one flipped bit, truncation: all rejected.
        assert!(decode(&bytes, &[2u8; 16], &ph, true).is_none());
        assert!(decode(&bytes, &fp, &ph, false).is_none());
        for i in [HEADER_LEN + 3, bytes.len() - SUM_LEN - 1] {
            let mut bad = bytes.clone();
            bad[i] ^= 0x40;
            assert!(decode(&bad, &fp, &ph, true).is_none(), "flip at {i}");
        }
        assert!(decode(&bytes[..bytes.len() - 1], &fp, &ph, true).is_none());
    }

    /// A fresh directory under the system temp dir, removed when dropped.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("iris-pcache-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MB: u64 = 1 << 20;
    const T0: Duration = Duration::from_secs(1_000_000_000);

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + T0 + Duration::from_secs(secs)
    }

    /// A build directory used at `used`.
    fn build(base: &Path, name: &str, used: SystemTime) -> PathBuf {
        let b = base.join(name);
        std::fs::create_dir_all(&b).unwrap();
        std::fs::File::create(b.join(".used")).unwrap().set_modified(used).unwrap();
        b
    }

    /// A blob file of `size` bytes, last used at `mtime`.
    fn file(build: &Path, page: &str, name: &str, size: u64, mtime: SystemTime) -> PathBuf {
        let d = build.join("fp").join(page);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, vec![0u8; size as usize]).unwrap();
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(mtime).unwrap();
        p
    }

    #[test]
    fn eviction_order_and_target() {
        let t = TempDir::new("order");
        let now = at(100_000);
        let cur = build(&t.0, "cur", now);
        let old = build(&t.0, "old", at(0));
        // Another process ran this build a minute ago: in use.
        let busy = build(&t.0, "busy", now - Duration::from_secs(60));
        let old_blob = file(&old, "p1", "a.jh", 2 * MB, at(50_000));
        let busy_blob = file(&busy, "p1", "a.jh", 2 * MB, at(1));
        let prob_old = file(&cur, "p1", "a.jc", 2 * MB, at(10));
        let prob_new = file(&cur, "p2", "b.jc", 2 * MB, at(20));
        let prot_old = file(&cur, "p3", "c.jh", 2 * MB, at(5));
        let prot_new = file(&cur, "p4", "d.jh", 2 * MB, at(30));
        // 12 MB against a 10 MB cap: delete down to 8 MB.
        let u = collect(&t.0, &cur, 10 * MB, 3, now, true);
        // The unused build goes whole, then the oldest probation blob, even
        // though a protected blob is older still.
        assert!(!old.exists() && !old_blob.exists());
        assert!(!prob_old.exists());
        assert!(prob_new.exists() && prot_old.exists() && prot_new.exists() && busy_blob.exists());
        assert_eq!(u.total, 8 * MB);
        assert_eq!((u.evicted_files, u.evicted_bytes), (2, 4 * MB));
        // An emptied page directory is removed with its last blob.
        assert!(!prob_old.parent().unwrap().exists());

        // A 5 MB cap (target 4 MB): the last probation blob, then the least
        // recently used protected one, which is in the busy build: its blobs
        // can go, though the build is never removed whole.
        let u = collect(&t.0, &cur, 5 * MB, 3, now, true);
        assert!(!prob_new.exists() && !busy_blob.exists());
        assert!(prot_old.exists() && prot_new.exists() && busy.exists());
        assert_eq!((u.total, u.protected), (4 * MB, 4 * MB));
    }

    #[test]
    fn keeps_recent_builds_and_builds_in_use() {
        let t = TempDir::new("keep");
        let now = at(100_000);
        let cur = build(&t.0, "cur", at(0)); // the running build ranks first anyway
        let b1 = build(&t.0, "b1", at(5_000));
        let b2 = build(&t.0, "b2", at(4_000));
        let b3 = build(&t.0, "b3", at(3_000));
        let busy = build(&t.0, "busy", now - Duration::from_secs(120));
        collect(&t.0, &cur, 1 << 40, 3, now, true);
        // Kept: cur, busy (in use) and b1 (the newest of the rest), so b2 and b3
        // are over the count of 3; busy is in use and survives regardless.
        assert!(cur.exists() && busy.exists() && b1.exists());
        assert!(!b2.exists() && !b3.exists());
    }

    #[test]
    fn measuring_deletes_nothing() {
        let t = TempDir::new("measure");
        let now = at(100_000);
        let cur = build(&t.0, "cur", now);
        let old = build(&t.0, "old", at(0));
        file(&cur, "p", "a.jc", 3 * MB, at(1));
        file(&old, "p", "a.jh", 3 * MB, at(1));
        let u = collect(&t.0, &cur, MB, 1, now, false);
        assert!(old.exists());
        assert_eq!((u.total, u.probation, u.protected, u.evicted_files), (6 * MB, 3 * MB, 3 * MB, 0));
        assert!(u.builds.iter().any(|(n, b, c)| n == "cur" && *b == 3 * MB && *c));
    }

    #[test]
    fn stale_temp_files_are_removed() {
        let t = TempDir::new("tmp");
        let now = SystemTime::now();
        let cur = build(&t.0, "cur", now);
        let stale = file(&cur, "p", ".a.jc.1.tmp", 10, now - 2 * STALE_TMP);
        let fresh = file(&cur, "p", ".b.jc.2.tmp", 10, now);
        collect(&t.0, &cur, 1 << 40, 3, now, true);
        assert!(!stale.exists() && fresh.exists());
    }

    #[test]
    fn a_hit_promotes_and_refreshes() {
        let t = TempDir::new("touch");
        let p = file(&t.0, "p", "a.jc", 10, at(0));
        touch(&p);
        let promoted = p.with_extension(PROTECTED);
        assert!(!p.exists() && promoted.exists());
        assert!(mtime(&promoted) > at(1_000_000));
        // A second hit only refreshes.
        std::fs::File::options().write(true).open(&promoted).unwrap().set_modified(at(0)).unwrap();
        touch(&promoted);
        assert!(promoted.exists() && mtime(&promoted) > at(1_000_000));
    }

    #[test]
    fn a_covering_variant_inherits_protection() {
        let t = TempDir::new("inherit");
        let fp = [4u8; 16];
        let mut small = [0u64; BITMAP_WORDS];
        small[0] = 0b01;
        let mut big = small;
        big[0] = 0b11;
        let b = blob(small, 1);
        let ph = page_hash(&b.words);
        write_blob(&t.0, &fp, &ph, false, &b).unwrap();
        let first = t.0.join(format!("{}.{PROBATION}", hex(&entries_hash(&small))));
        assert!(first.exists());
        touch(&first); // reused: protected
        // Union-on-miss writes a superset: it replaces the protected variant
        // and stays protected.
        write_blob(&t.0, &fp, &ph, false, &blob(big, 1)).unwrap();
        let names: Vec<String> = std::fs::read_dir(&t.0).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec![format!("{}.{PROTECTED}", hex(&entries_hash(&big)))]);
        // A superset of a probation-only page stays on probation.
        let mut other = [0u64; BITMAP_WORDS];
        other[1] = 1;
        let ph2 = page_hash(&blob(other, 2).words);
        let d2 = t.0.join("p2");
        write_blob(&d2, &fp, &ph2, false, &blob(other, 2)).unwrap();
        other[1] = 3;
        write_blob(&d2, &fp, &ph2, false, &blob(other, 2)).unwrap();
        let names: Vec<String> = std::fs::read_dir(&d2).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec![format!("{}.{PROBATION}", hex(&entries_hash(&other)))]);
    }

    #[test]
    fn variants_are_capped() {
        let t = TempDir::new("variants");
        let fp = [3u8; 16];
        let dir = t.0.join("page");
        std::fs::create_dir_all(&dir).unwrap();
        // Variant i has i+1 entries; all from the same page.
        let mut names = Vec::new();
        for i in 0..MAX_VARIANTS + 2 {
            let mut e = [0u64; BITMAP_WORDS];
            e[0] = (1u64 << (i + 1)) - 1;
            let b = blob(e, 9);
            let ph = page_hash(&b.words);
            let stem = hex(&entries_hash(&e));
            std::fs::write(dir.join(format!("{stem}.{PROBATION}")), encode(&fp, &ph, false, &b)).unwrap();
            names.push(stem);
        }
        // The one "just written" has a single entry, and still stays.
        trim_variants(&dir, &names[0]);
        let left: Vec<String> = std::fs::read_dir(&dir).unwrap()
            .map(|e| e.unwrap().path().file_stem().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(left.len(), MAX_VARIANTS);
        assert!(left.contains(&names[0]));
        // The fewest-entry others went: variants 1 and 2.
        assert!(!left.contains(&names[1]) && !left.contains(&names[2]));
    }

    fn ram_blob(entries: Entries, seed: u32, code_len: usize) -> Arc<Blob> {
        let mut b = blob(entries, seed);
        b.code = vec![0u8; code_len];
        Arc::new(b)
    }

    #[test]
    fn memory_serves_covering_variants_of_the_same_words() {
        let mut ram = Ram::new();
        let key = ([1u8; 16], [2u8; 16], false);
        let mut e = [0u64; BITMAP_WORDS];
        e[0] = 0b0110;
        let b = ram_blob(e, 3, 100);
        let words = *b.words;
        ram.insert(key, b, 1 << 30);
        let mut want = [0u64; BITMAP_WORDS];
        want[0] = 0b0100;
        assert!(ram.get(&key, &words, &want).is_some());
        // Not covered, other words, or another key: no.
        want[0] = 0b1000;
        assert!(ram.get(&key, &words, &want).is_none());
        want[0] = 0b0100;
        let mut other = words;
        other[5] ^= 1;
        assert!(ram.get(&key, &other, &want).is_none());
        assert!(ram.get(&([9u8; 16], [2u8; 16], false), &words, &want).is_none());
        // A covering variant replaces the one it covers.
        e[0] = 0b1110;
        ram.insert(key, ram_blob(e, 3, 100), 1 << 30);
        assert_eq!(ram.pages[&key].len(), 1);
        assert_eq!(ram.bytes, blob_bytes(&ram.pages[&key][0].blob));
    }

    #[test]
    fn memory_evicts_least_recently_used() {
        let mut ram = Ram::new();
        let size = blob_bytes(&ram_blob([1; BITMAP_WORDS], 0, 1000));
        let cap = size * 4;
        let mut e = [0u64; BITMAP_WORDS];
        e[0] = 1;
        let keys: Vec<Key> = (0..4u8).map(|i| ([i; 16], [i; 16], false)).collect();
        let words: Vec<_> = (0..4).map(|i| *ram_blob(e, i, 1000).words).collect();
        for (i, k) in keys.iter().enumerate() {
            ram.insert(*k, ram_blob(e, i as u32, 1000), cap);
        }
        // Use the first, so the second is now the least recently used.
        assert!(ram.get(&keys[0], &words[0], &e).is_some());
        ram.insert(([7; 16], [7; 16], false), ram_blob(e, 7, 1000), cap);
        assert!(ram.bytes <= cap);
        assert!(ram.pages.contains_key(&keys[0]), "recently used stays");
        assert!(!ram.pages.contains_key(&keys[1]), "least recently used goes");
        // Off: nothing kept.
        let mut off = Ram::new();
        off.insert(keys[0], ram_blob(e, 0, 10), 0);
        assert!(off.pages.is_empty());
    }

    #[test]
    fn index_answers_misses_and_keys_parse() {
        let build = Path::new("/c/build");
        let fp = [0xabu8; 16];
        let ph = [0xcdu8; 16];
        let path = build.join(hex(&fp)).join(format!("{}-1", hex(&ph))).join("0011223344556677.jh");
        assert_eq!(key_of(&path, build), Some((fp, ph, true)));
        assert_eq!(key_of(Path::new("/elsewhere/x"), build), None);
        let mut ram = Ram::new();
        // No index yet: never "absent".
        assert!(!ram.known_absent(&(fp, ph, true)));
        let files = vec![FileInfo { path, size: 1, mtime: SystemTime::UNIX_EPOCH, protected: true }];
        ram.set_index(&files, build);
        assert!(!ram.known_absent(&(fp, ph, true)));
        assert!(ram.known_absent(&(fp, ph, false)));
        ram.note_on_disk((fp, ph, false));
        assert!(!ram.known_absent(&(fp, ph, false)));
    }

    #[test]
    fn subset_rule() {
        let mut have = [0u64; BITMAP_WORDS];
        have[1] = 0b110;
        let mut want = [0u64; BITMAP_WORDS];
        want[1] = 0b100;
        assert!(covers(&have, &want));
        want[2] = 1;
        assert!(!covers(&have, &want));
    }
}

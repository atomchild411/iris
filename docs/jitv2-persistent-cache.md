# jitv2 persistent code cache — design

Status, 2026-09-24: implemented behind `IRIS_JIT_CACHE=1` (`src/cpu/jitv2/pcache.rs`),
under verification. Where the implementation departs from the plan below, the
section says so.

Branch `jitcache-explore` (experimental, not for upstream yet): the sections
from "Organizing the cache by program" on are explorations, not plans.

Update, 2026-10-04: the cache is bounded: `[jitv2] cache_max_mb` (default
1024) and `cache_keep_builds` (default 3), with eviction that keeps reused
pages over pages written once. See "Bounds and eviction".

Update, 2026-10-01: the toggle is now `[jitv2] cache`/`cache_dir` in
`iris.toml` (also exposed in the iris-gui config editor), which
`Jitv2Config::apply_env` (`src/config.rs`) turns into the `IRIS_JIT_CACHE`/
`IRIS_JIT_CACHE_DIR` env vars described below. The env vars still work as a
direct override — same rule as the rest of `[debug]`/`[jitv2]` — but are no
longer the documented interface.

The R10000 JIT recompiles the same pages in every run. This keeps compiled
pages on disk, keyed by what they were compiled from, so a later run loads
them instead of running Cranelift again. It is also the storage an
ahead-of-time compiler would fill (the last section).

## Why: measured, not assumed

Two identical sessions on our IP28 (R10000) machine, which comes in a later pull request (boot to login, two MIPSpro compiles, two
`ls -lR /usr/include`, small utilities), every compile logged with its page
content hash, FR mode and entry set (`IRIS_JIT_HASHSTATS_LOG`):

| | run 2's compiles | of compiled instructions |
|---|---|---|
| page content already compiled in run 1 | **97.1%** | 97.0% |
| a run-1 compile covers the entries run 2 wanted | 82.7% | 79.8% |
| run 1's entries merged per page (a cache that grows its variants) | **85.7%** | 83.0% |
| first quarter of run 2 (boot), merged | **92.9%** | 91.4% |

Cost of what a hit replaces: 5,500 compiles took **122 s of Cranelift time,
22 ms per page** (4 compile threads). A warm cache saves on the order of
100 CPU-seconds per session, concentrated where it hurts: boot, and the first
run of a program, which today waits behind the compile queue.

Within one session, by contrast, no page's content is ever compiled at a
second physical address (0 of ~6,500): IRIX keeps a program's text in the
same frames. The reuse is across runs and across mega-flushes, which is what a
persistent cache captures.

Footprint: one run compiled 3,053 distinct (page, FR) pairs.

## What a compiled page depends on: the key

A compiled page is one Cranelift function with a dispatch switch over its
entry points. It is fully determined by:

1. **The page's bytes.** v1 keys on all 1024 words and stores them, so a hit
   is verified with a 4 KB compare; no hash collision can serve wrong code.
2. **Its entry set.** A blob serves any request whose entries are a subset of
   the blob's (the switch has a case for each).
3. **FR mode**, **ISA level** (MIPS IV), **CPU model**.
4. **Codegen configuration:** direct memory on or off, inline memory on or
   off, cache geometry, instruction budgets, opt level, the per-category
   enables (`j2 alu|fpu|...`), CP0/atomics in-region, lockstep/developer
   features. All of it goes into a fingerprint computed **per compile**, since
   several are runtime toggles.
5. **Build identity:** a BLAKE3 hash of the running executable. Any rebuild is
   a new cache namespace; nothing is ever served across builds.
6. **Host ISA:** Cranelift's target flags (detected CPU features).

The physical page is **not** an input: branch targets are page-relative,
`j`/`jal` resolve at run time, and the `PhysicalCodePage` pointer codegen
receives is bookkeeping, never emitted.

Correctness does not need Cranelift to be deterministic: a blob only has to be
*a* correct compilation of identical inputs under an identical configuration.
The real risk is a hidden input missing from the key, which is why the
fingerprint is strict and the key is conservative (whole page, whole build).

## Position independence

Compiled code can bake two kinds of host address:

- **Rust hook pointers** (`JitConsts::hook_addr`): read out of the core at
  compile time and emitted as call targets. They move with every launch
  (ASLR). **Only under `IRIS_BAKE_HOOKS=1`**: baking measured as a code-size
  loss, so it is off by default, and codegen loads the pointer from the core.
- **Shared memory-helper addresses** (`emit_mem_helper_call`): they live in
  the JIT arena. Off by default (`IRIS_MEM_HELPERS` enables them).

So default compiled code is already position independent: the core pointer is
a function argument, never baked. `Codegen::cache_fingerprint` refuses (no
caching) when either switch would put an address in the code.

Correction: an earlier draft reported a cost for `IRIS_JIT_PIC=1` (median
+1.1%, geometric mean +2.7% on jitcov). With `IRIS_BAKE_HOOKS` unset, the
published core address is read by nothing but `hook_addr`, which returns
before using it, so the two builds emit the same code. Those numbers were
run-to-run variance, not a cost.

Guard: a blob is stored only if Cranelift reports **no relocations** for it
(for example a library call for an FP operation). Such a page simply isn't
cached.

## Storage

As implemented: no index file. The layout is
`<base>/<build-id>/<fingerprint>/<page-hash>-<fr>/<entries-hash>.jc`, and a
lookup lists one page directory (a failed `open` on a never-seen page). Blobs
carry a BLAKE3 checksum; a new variant deletes the ones it covers. The plan
as first written:

- `$IRIS_JIT_CACHE_DIR`, default the user cache directory's `iris/jitv2/<build-id>/` (`~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or `~/.cache` elsewhere)
  (off the project drive; per build).
- One file per blob, named `<page-hash>-<fr>-<entries-hash>.jc`: a header
  (magic, format version, fingerprint, FR, entry bitmap, used-word bitmap,
  code alignment and length), then the 4 KB page, then the code.
- Written by a background thread via temp file + rename, so a crash never
  leaves a torn blob; a blob that fails its header or length check is ignored
  and deleted.
- At startup the directory is scanned into an index
  `(page-hash, fr) -> [blob]`. About 3,000 files for a session: milliseconds.
- Pruning: keep the newest three build directories, and cap total size
  (LRU by access time). As built (2026-10-04), the cap evicts pages never
  reused before reused ones; see "Bounds and eviction".

## Lookup and insert

In `comp.rs`'s deferred path, after `prepare_multi_entry_compile` (snapshot and
walk, both cheap):

1. Hash the page's words. Find a blob with the same FR mode whose entries
   cover this compile's covered entries.
2. **Hit:** compare the stored 4 KB with the snapshot; on any difference it is
   a miss. Otherwise `declare_function` + `define_function_bytes` (Cranelift
   copies the bytes through the same `PagedArenaMemoryProvider`), then the
   unchanged placeholder / seal / publish path. It publishes the blob's
   entries (a superset, all valid for these bytes) and stages the churn
   snapshot from the blob's used-word mask.
3. **Miss:** compile the *union* of the requested entries and the best cached
   variant's, so variants converge towards "every entry this page ever
   needed" (the 97% ceiling above). Hand the bytes to the writer thread.

Nothing else changes: generation checks, the seal queue, churn avoidance and
denials behave as for a compile.

## Verification plan

1. The page compare, fault-injected: `IRIS_BREAK=jitcache-skip-compare`
   serves a blob for a page with one word changed. It must fail a test that
   passes with the compare, so the compare is known to be load-bearing.
   (As run: a full-page hash never collides by itself, so this also needs
   `jitcache-weak-key`; see Results.)
2. jitcov cold vs warm: all 188 kinds agree in both. Warm timings equal cold
   ones, since the code is the same.
3. IP28 boot to login, cold then warm: hit rate (expect about 90% at boot),
   compile time, and time to login.
4. The first-program backlog: `jitcov nowarm` right after boot, cold vs warm.
5. A rebuild never serves an old blob (a new build id, a new directory).

## Results (2026-09-24, IP28 / R10000, 4 compile threads)

Workload per session: boot to login, a MIPSpro compile of jitcov, `jitcov
100000 nowarm` (the first program after boot), `ls -lR /usr/include` twice, a
loop of small utilities, `jitcov 100000`. One cold session (empty cache), then
two warm ones.

| | cold | warm | warm 2 |
|---|---|---|---|
| lookups served from the cache | 15% (in-run) | 86% | 95% |
| Cranelift time, all threads | 143 s | 63 s | 27 s |
| boot to `login:` | 42 s | 39 s | 39 s |
| first program: jitcov routines still > 10 ns | 27 of 188 | 3 | 3 |
| first program: sum of the 188 timings | 1060 ns | 205 ns | 195 ns |

A cached page loads in about 8 µs; compiling it takes about 22 ms. No compile
was refused for relocations. In every jitcov run, cold and warm, the 186
deterministic kinds give the same checksums as the last no-cache run. `sc` and
`scd` count successful store-conditionals, which an interrupt between `ll` and
`sc` makes fail, so they differ by a few counts in every run, with or without
the cache. (An earlier version of this section claimed all 188 agree; that was
a misreading.)

The page compare, fault-injected with a guest program built for it
(`cpu-tests/jitcov/irix-kc`, `kc K`: 200 `addiu a0,a0,K`, so runs with different K differ only in
immediates):

- `IRIS_BREAK=jitcache-weak-key` keys pages on each word's top 16 bits only.
  The runs collide in the cache; the compare rejected 8 of them and every
  result was correct.
- Adding `jitcache-skip-compare` serves whatever collides. The guest could not
  finish the MIPSpro compile that precedes the test.

So the compare is load-bearing, and it holds. (The SMC test could not serve
here: its function is three words and never exercised the cache.)

A rebuild gets a new build id and so an empty cache namespace; a new build's
first session showed the cold hit rate.

Size: 210–270 MB per build (about 3,300–4,400 blobs of 64 KB on average, most
of it code), so up to about 800 MB with three builds kept. No size cap yet.

The warm sessions first measured here showed a loop that the cache exposed but
did not cause: one kernel page prepared thousands of times at one generation
(0x20010: 6,734 FR0 compiles straight after 3 FR1 ones), each `publish`
refused. Kernel pages run under whichever FR mode the interrupted process
uses. An FR1 compile was published, the next request came from an FR0
context and re-pinned the page, and the FR0 compile was refused as "already
covered at this generation": that check ignored the FR mode. The page was
left pinned FR0 with FR1 code and an FR1 churn snapshot, so every later
request failed the churn skip on the mode, recompiled and was refused again,
until a mega-flush. Arrivals at denylisted offsets (kernel entries the JIT
excludes, hit on every exception) supplied the requests. Without the cache
each round is a 22 ms compile, which throttles it; with the cache it is an
8 µs load, so it ran about 10,000 extra rounds per session.

Fixed in two parts: `publish` records the FR mode of the installed code and
treats a compile for the other mode as never redundant (replacing the
advertised entries, keeping denials); and the dispatch gate no longer
requests a compile for a denylisted offset while the page's bytes are
unchanged. Five sessions afterwards (one cold, three warm, one without the
cache): at most 48 compiles of any page per session, 8,600-9,100 in total
(16,500-17,000 while looping), one mega-flush each, and the same warm
results as above (hits 80-96%, Cranelift time 45, 18 and 14 s). R4400 and
R5000 Indy boots still reach login.

## Bounds and eviction

Until 2026-10-04 the only bounds were "three builds" and "a new variant
deletes the ones it covers". Nothing capped bytes, so the cache grew with every
machine configuration, guest release and one-off program, and a 17th variant
of a page was written but never read (lookups read 16) or deleted. Pruning
could also delete the build directory of another iris process still running.

### Settings

- `[jitv2] cache_max_mb` (`IRIS_JIT_CACHE_MAX_MB`), default **1024**: the
  whole cache directory, all builds together, counted in allocated blocks (as
  `du` counts).
- `[jitv2] cache_keep_builds` (`IRIS_JIT_CACHE_KEEP_BUILDS`), default 3.

### What a page is worth

Plain LRU is the wrong policy: a session that builds packages in the guest
compiles thousands of pages once, and under LRU they push out the kernel and
libc pages every boot needs. A page that has been *reused* is worth more than
one that was only *written*, and the filesystem holds that without an index:

- **Probation** (`.jc`): every new blob.
- **Protected** (`.jh`): a blob that has served a lookup. The first hit renames
  `.jc` to `.jh` (atomic, no lock); every hit also sets the file's mtime, which
  is its recency.
- A variant that covers others replaces them, and **inherits their
  protection**: union-on-miss rewrites boot pages with more entries, and that
  must not put them back on probation.

Promotion and the mtime update are jobs for the writer thread, never the
compile thread.

### Eviction (`pcache::collect`)

Over the cap, a pass deletes down to **80%** of it (so the next write does not
start another pass), in this order:

1. build directories other than the running one that no process has used in
   the last 15 minutes, least recently used first;
2. probation blobs, oldest first;
3. protected blobs, least recently used first.

Emptied page and fingerprint directories go with their last blob; temp files
older than an hour (a writer that died) go too. Beyond the size cap, only the
`cache_keep_builds` most recently used builds are kept, again never one in use.

A pass runs at startup on a background thread, and on the writer thread after
every 5% of the cap written. The directory can therefore pass the cap by a few
percent between passes; the next pass brings it back. A pass is a directory
walk with one `stat` per file (a few thousand files): milliseconds.

**Several processes:** a lock on `<base>/.lock` lets one pass run at a time
(the others skip theirs); each running process touches its build's `.used`
every 5 minutes, which keeps step 1 and the build count away from it. A blob
deleted under a reader is a miss: a failed read or check is never served.

**Variants:** a page keeps at most 16; past that, the variants with the fewest
entries go first, then the least recently used.

### Monitor

`jitcache` (or `jitcache status`): the session's counters, the size against
the cap, probation and protected bytes, and each build's size. `jitcache
prune` runs a pass now; `jitcache clear` deletes this build's blobs (other
builds may belong to running processes).

### Results (2026-10-04, IP28 / R10000, IRIX 6.5.22, 4 compile threads)

Four sessions in a row on one cache directory. Each session: boot, then over
ssh either a light workload (`ls -lR /usr/include`, a small `cc` compile) or a
flood (`nm` over 300 shared libraries, a dozen `man` pages, `file` over 3,000
files, `cc` and `CC` compiles), then a clean shutdown.

| session | cap | workload | hits | cache after | probation / protected |
|---|---|---|---|---|---|
| 1 cold | 1024 MB | light | 19.2% | 231 MB | 127 / 23 MB |
| 2 warm | 1024 MB | light | 78.8% | 295 MB | 99 / 195 MB |
| 3 | 320 MB | flood | 73.8% | 324 MB | 36 / 284 MB |
| 4 warm | 320 MB | light | **85.0%** | 313 MB | 25 / 257 MB |

(Probation and protected are from the monitor shortly before shutdown;
"cache after" is `du` after it.) The flood's pages took the evictions
(65 MB in session 3, 67 MB at session 4's start), the reused set stayed, and
session 4 hits more than session 2: its pages had grown more entries meanwhile.

The first version did not pass protection on to a covering variant. With a
256 MB cap the protected set then shrank 200 → 179 → 154 MB across the same
sessions and the warm boot after the flood fell to 64%, because union-on-miss
had put boot pages back on probation. The inheritance rule above fixed it.

Unit tests (`pcache::tests`) cover the eviction order and the 80% target, a
build in use surviving both the count and the cap, measuring without deleting,
stale temp files, promotion on a hit, inheritance, and the variant limit.

## Risks

- **A hidden input left out of the fingerprint** serves code compiled for a
  different configuration. Mitigations: the whole-build id, a conservative
  fingerprint, and `IRIS_JIT_CACHE=off` as a kill switch.
- **Disk-resident executable code:** the cache directory is the user's own;
  blobs are never shared between machines or users.
- **Size:** roughly 3,000 blobs per build for this workload; bounded by
  `cache_max_mb` since 2026-10-04.

## Implementation steps

1. `IRIS_JIT_PIC` becomes the default when the cache is on (done as a switch).
2. Capture bytes, alignment and relocations after `define_function`; store
   only relocation-free blobs.
3. The blob format, writer thread and startup index.
4. Hit path via `define_function_bytes`, with the page compare.
5. Union-on-miss.
6. Verification 1–5.

## Towards AOT

The key is page content, not where or when a page was compiled, so an offline
tool can fill the same cache: walk the text pages of the IRIX kernel (`/unix`)
and the shared libraries (quickstart-prelinked at fixed addresses, so their
pages are byte-identical in memory), compile each with a superset of plausible
entry points (symbols, branch targets, return sites), and write blobs. The
first boot of a fresh build would then start warm.

## Organizing the cache by program (exploration, 2026-10-04)

The cache is keyed by page content, so it already serves any program that has
run before: an application's text pages, and its shared libraries' (PIC and
quickstart-prelinked), have the same bytes in every run, wherever IRIX puts
them. What the store does not know is *which program a page belongs to*. That
matters for two things the flat store cannot do well:

- **Eviction.** Boot is the same every time and is worth keeping. Firing up an
  editor or Netscape is what one user does and another doesn't. Half of a
  program's working set is worth much less than all of it: the first page that
  misses puts the program behind the compile queue again.
- **Prefetch.** Each lookup today is triggered by a request, one page at a
  time, and a program's start is a burst of a few hundred requests. If the
  cache knew that page X begins Netscape's start, it could read the whole
  start-up set at once, before the requests arrive.

### Ways to know the program

**A. Watch exec (needs knowledge of the guest).** At IRIX's `execve` the path
is a user string at `a0`; iris can read it through the guest TLB the way the
monitor does. Identity is the path plus a hash of the first text page run in
user mode afterwards, so a replaced binary is a new program. Precise and cheap,
but per guest OS (syscall numbers differ between IRIX, NetBSD and others), and
per ABI.

**B. Watch the address space (guest-agnostic).** A program starts as a new
ASID running user-mode code at a page not seen in that ASID before. Identity is
the content hash of the entry page plus the virtual address of the entry. No OS
knowledge, but exec within one process is a boundary it has to infer, and
`crt1`'s start-up code is common to many programs (the entry page also holds
the program's first functions, which usually make it distinct; to be checked).

**C. Learn successors, name nothing.** Record, per page, the pages that were
requested soon after it in the same ASID. On a hit, prefetch those successors.
This learns "Netscape's start" without ever naming Netscape, works for any
guest, and needs no exec boundary. Shared-library pages have many successors,
so fan-out has to be capped, or keyed by the previous page as well (a short
Markov context).

These combine: C as the general mechanism, with A's names when the guest is
known (for display, and for whole-program eviction).

### Working sets

Whichever way it is found, a program's **working set** is the ordered list of
(page hash, FR, entries) that its first seconds request, recorded per run. With
working sets:

- a page's value is taken from the working sets that contain it: how often and
  how recently each ran. Kernel and libc pages are in every set and rank
  highest; a page only one forgotten program used ranks lowest;
- eviction removes the least valuable *working set* as a unit, after
  probation, rather than scattering misses across many programs;
- prefetch on a program's first page reads its set into a bounded in-memory
  side table, not into the JIT arena: the arena is finite (a mega-flush at
  4,096 pages), so blobs are installed only when a request arrives, but from
  memory instead of from disk.

Working sets are hints. A stale one costs a wasted read; it can never serve
wrong code, because every blob still passes the page compare and the
fingerprint.

### Storage that follows from it

- **Packs.** Like git's loose objects and packfiles: new blobs are written
  loose, as today, and a program's protected working set is periodically
  rewritten as one pack file (an index of page hashes, then the blobs). Start-up
  is then one sequential read or one `mmap` instead of hundreds of opens, and
  eviction of a program is deleting one file.
- **Compression.** Blobs average 64 KB for a 4 KB guest page, most of it code;
  machine code typically compresses 2-3x (zstd, fast to decompress). That would
  halve or better the disk and the I/O, at some CPU per load: measure against
  the 8 µs load.
- **A shipped base.** Boot, the kernel, libc and the desktop are the same for
  everyone on a given IRIX release and IRIS build. That base could come from
  the AOT path above (or from a recorded session) as a read-only pack, with
  each user's own programs learned on top.

### Experiments before building any of this

1. **Traces.** Log every request with time, page hash, FR, ASID, kernel/user
   and PC, and the path at each `execve` (A), over a set of scenarios on the
   same machine: boot only; boot and the desktop; Netscape's start; an editor;
   `less` on a big file; a compile; Maya's start.
2. **Analysis of the traces.** How many pages each scenario adds beyond boot;
   how much scenarios share (kernel, libc, X and Motif libraries); whether the
   entry page identifies a program (B); how predictable successors are (C);
   how much of a program's cold start is compile time (the gap between first
   request and running compiled code).
3. **Simulated policies.** Replay the traces against LRU, the two-segment
   policy above and working-set eviction, at caps of 256 MB, 512 MB and 1 GB,
   with hit rate and time-to-warm as the result. This picks the policy with
   data before any of it is written into `pcache.rs`.
4. **Prefetch value.** In one scenario, prefetch the recorded working set at
   the program's first page and measure time to its first window, against the
   plain warm cache.

The bounds above are worth building first, independent of this: they fix
unbounded growth with a policy that the experiments can later refine.

## Beyond pages: whole functions, whole binaries (exploration, 2026-10-04)

The unit today is a 4 KB physical page compiled as one Cranelift function with
a dispatch switch over its entries; control that leaves the page goes back
through the dispatcher. The cache inherits that unit. Two larger units are
worth exploring, and the cache's key (content, not address) carries over to
both.

### Caching whole guest functions

A guest function (entry to `jr ra`) can span pages, and today it is split
wherever a page ends, with a dispatch between the pieces.

- **Unit and key.** A function is compiled as one region over the pages it
  touches. Its key is the ordered list of (page hash, FR) of those pages plus
  the entry offset; a hit needs every page's 4 KB compare, and the region is
  valid while all of its pages are unchanged (a generation check per page,
  where today there is one).
- **Finding functions at run time.** `jal` targets are entries; a `jr ra` ends
  one; the pages a function reaches between them are its extent. Recorded per
  run, this is the same kind of trace as the working sets above.
- **What it buys:** no dispatch at page boundaries inside a function, Cranelift
  optimizing across them, and calls between cached functions chained directly
  instead of through the dispatcher.
- **What it costs:** invalidation gets wider (a write to any of the pages kills
  the region), regions overlap (a page in several functions), and the arena
  holds duplicates. Measure: how many hot functions actually cross a page; how
  much time goes to page-boundary dispatch today.

### AOT for whole binaries

Everything a program runs is in its ELF file before it runs, so an offline tool
can fill the cache (or one pack per binary) before the first launch.

- **The pages are predictable.** Non-PIC IRIX executables load their text at
  the link address, unrelocated; DSOs are PIC, and quickstart-prelinked at
  fixed addresses. A text page in memory has the file's bytes, including
  whatever follows the text in the same page, so its hash can be computed from
  the file. The FR mode follows the ABI in the ELF header (o32 FR0; n32 and 64
  FR1).
- **Entries:** the symbol table (`.dynsym`, and `.symtab` or the `.mdebug`
  procedure table where not stripped), every `jal` target, return sites after
  calls, the GOT's function addresses, and jump tables found from their
  relocations. A superset is fine: the switch just has more cases.
- **Units:** pages first (the current store, no runtime change); whole functions
  once the section above exists.
- **What to AOT:** the kernel (`/unix`), `rld`, libc and the X and Motif
  libraries for everyone; per user, the binaries they actually run (the
  working sets above say which).
- **Verification:** the same as for the runtime cache (fingerprint, page
  compare), plus jitcov and A/B runs, AOT cache against an empty one.
- **Measure first:** how much of an AOT'd page's entry set the program
  actually uses, and how close AOT gets to a warm runtime cache on boot and on
  a program's first start.

## A second layer: Cranelift's incremental cache (exploration, 2026-10-04)

Cranelift 0.134 has an experimental `incremental-cache` feature
(`cranelift_codegen::incremental_cache`, `Context::compile_with_cache`): it
keys a compile on the function's IR (its "stencil": the IR with external names
abstracted out) plus the Cranelift version and the target's flags, SHA-256
hashed, and caches the compiled stencil (postcard-serialized). We don't use
it.

| | our cache (`pcache`) | Cranelift's incremental cache |
|---|---|---|
| key | page bytes, FR, codegen fingerprint, **iris binary hash** | IR + Cranelift version + ISA flags |
| looked up | before our IR is built | after it is built |
| a hit skips | IR building and Cranelift | Cranelift's backend (lowering, regalloc, emission) |
| a hit costs | ~8 µs | our IR build + hashing it + deserializing |
| after a rebuild | always cold | warm wherever the emitted IR didn't change |

The IR is exactly the right test for "did codegen change": a moved core field
changes an offset in the IR, a changed helper signature changes the IR, and a
Rust helper whose body changed is called from the current build anyway. So a
second layer keyed on IR would keep the cache warm across rebuilds that don't
touch the JIT, for us while developing and for users across releases.

**Design:** keep `pcache` as layer 1 (per build, 8 µs). On a layer-1 miss,
build the IR, look it up in a layer-2 store shared by all builds (under
`<base>/ir/`, same bounds); on a hit skip the backend and write the result to
layer 1. A rebuild's first session then pays only for our IR build per page.

**Measure first:**
1. How the 22 ms per page splits between our IR build and Cranelift's
   backend. Layer 2 is worth it only if the backend dominates.
2. Whether our IR is deterministic: compile one page twice, compare keys; then
   across two builds that differ in unrelated code. (A `HashMap` order or a
   host address in the IR would make every key miss; never wrong code.)
3. The hit rate across a real rebuild, with
   `enable_incremental_compilation_cache_checks` on (it recompiles every hit
   and asserts the result is identical).

## Open question: what counts as reuse (2026-10-04)

`jitcache-bounds` promotes a blob on any hit, so a mega-flush's reloads count
as reuse. In the cold session above, 126 blobs were promoted before the
session's one mega-flush and about 890 after it: 88% of that session's
promotions came from the flush. That protects the kernel, libc and the shell,
as it should, but also anything hot across a flush, such as a package build
running for an hour; and protected blobs are evicted least recently used
first, so boot's pages, last used at boot, would go before the build's.

Options:

1. **Promote only on a hit in a later run** than the one that wrote the blob
   (the process remembers what it wrote); a same-run hit only refreshes the
   mtime. Small; candidate for the bounds PR itself.
2. **Count the runs** a blob has been reused in (a counter in the name).
3. **Frequency-aware protected eviction:** runs reused first, then recency
   (needs 2).

Test: the four sessions above plus one long session with several mega-flushes
(a package build in the guest), with and without option 1. The trace replay
experiments above can compare 2 and 3.

## AOT step 0: do the pages IRIX runs exist in its files? (2026-10-04)

One IP28 session with the cache on (IRIX 6.5.22m: boot, `ls -lR`, a `cc`
compile, `ssh-keygen -t ed25519`, `openssl rand/genrsa/dgst/speed`, `less`,
`nm`, shutdown): 4,495 distinct compiled (page, FR). Then every ELF file that
could have run was copied off the guest (kernel, rld, every DSO, every
executable outside large applications; 3,872 files), plus `sash` from the
volume header and the PROM image, and each was laid out as in memory (PT_LOAD
pages; raw images and relocatable `.text` in 4 KB pieces). Scripts:
`scratch/jitcache-aot/{match,partial,anyoff}.py`.

| | pages | share |
|---|---|---|
| byte-identical to a page of a file (or the PROM) | 4,391 | **97.7%** |
| its code is in a file, but at another offset in the page | 41 | 0.9% |
| its code is in none of the copied files | 63 | 1.4% |

(First count 64.5%, then 93.2%: the copy had missed `/usr/etc` and the
compiler's executables, then `/lib32`, where `/usr/lib32/libc.so.1` points.)

- The identical pages cover the kernel (`/unix`, ~600 pages), the PROM, rld,
  libc, libcrypto, the MIPSpro compiler, bash, sh, Perl, X, sshd, ssh-keygen,
  openssl and less. Their page hash, and so their cache key, can be computed
  from the file: AOT can produce exactly these blobs offline.
- At another offset: `sash`, which the PROM relocates when it loads it (15),
  PROM code copied into RAM (4), kernel code copied elsewhere (4); the rest
  single pages, probably chance matches of 4 instructions.
- In none of the copied files: a search of the whole 36 GB disk image found
  the code of 49 of them on disk (files not copied, or deleted since, like the
  workload's own test programs); 10 are nowhere on disk (7 of them FR0, boot
  time: code generated or relocated in memory); 4 had no distinctive run.
- Content of all 104: code, not data (84-94% of words decode as
  instructions, the rest mostly zeros); the only strings are symbol and
  section names, library paths and `mload version 7.0`.
- Entropy: ssh-keygen and openssl ran from file pages like everything else;
  keys and random data are data and never reach the key or the code. A blob
  stores its whole 4 KB page, so a page mixing code with live data would put
  that data on disk; none of the pages in this run did. Storing and comparing
  only the decoded words would close that path for good.

## AOT step 1: can static analysis predict the entries? (2026-10-04)

`scratch/jitcache-aot/step1.py`, over the 4,315 file pages: runtime entries
(the union over each page's variants) against entries found statically in the
file.

| static entries | runtime entries predicted | pages fully covered |
|---|---|---|
| symbols, calls, return sites, cross-page branches, code addresses in data, page start | 9.7% | 0 |
| + in-page branch targets and fall-throughs (3x as many entries) | 23.7% | 0 |

Pages average **75 runtime entries**, spread over every kind of instruction
in proportion to how common it is. The cause is in `step_jit`: an arrival
with `jit_trigger` set at an offset with no compiled code requests a compile
with that offset added, and `eret` sets the trigger
(`exec_complete_pc_set`). So every return from an interrupt, a TLB miss or a
system call adds the instruction it lands on, and interrupts land anywhere on
a hot page.

So purely static AOT would serve a page until the first `eret` lands on an
unpredicted offset, then recompile it (union-on-miss), and so on. Directions:

1. **Don't compile for `eret` landings** (interpret on to the next published
   entry instead). Entry sets would become close to static, recompiles fewer
   for the JIT as a whole, and AOT viable. Cost: a short interpreted stretch
   after each interrupt return. Needs measuring: compile requests by arrival
   class, and the interpreted instructions per `eret`.
2. **Profile-guided AOT:** the warm cache already is the record of real entry
   sets; AOT supplies the code, recorded runs the entries (a shipped base).
3. **Every instruction an entry** for AOT blobs: measure the code size and
   speed cost of a switch with up to 1,024 cases.

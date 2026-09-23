# What in `atom-wip` could become a pull request

Inventory taken 2026-09-23 against `upstream/main` (`bea0af6`). Nothing here
is pushed or opened; this is the map.

**102 non-merge commits** sit ahead of upstream. They are not 102 PRs — most
belong to one of three machine-support series, and a third of them are
documentation.

## How each commit was triaged

Not by reading it. Each was cherry-picked onto a scratch worktree at
`upstream/main` and the result measured:

| verdict | meaning | count |
|---|---|---|
| **ALREADY** | cherry-pick produces an *empty* diff — upstream has it | 5 |
| **STANDALONE** | applies cleanly and changes something | 27 |
| **NEEDS-CONTEXT** | conflicts: depends on commits before it | 70 |

**`git cherry` is not trustworthy here.** It reported all 102 as new,
including three that are demonstrably already upstream (PRs #126, #127, #128)
— patch-ids differ once a commit has been rebased and re-merged. The empty
cherry-pick is the test that was right.

**Nine subjects appear twice**, same work under two SHAs, from committing on
two branches and merging both. Any series built from history rather than from
file contents will carry both. Build PR branches from *today's file contents*.

---

## The catalogue — by feature, not by commit

Reviewed one by one 2026-09-23. A *feature* is the unit that becomes a PR;
several are more than one commit, and two commits split into more than one
feature. Each row was cherry-picked onto `upstream/main` and built before it
earned a verdict.

**The question asked of every row: does upstream have this problem?** Several
things that looked generic turned out to be ours alone, and several that
looked local turned out to be upstream bugs sitting in their tree right now.

### A. MIPS IV instruction coverage — *one feature, three commits*

`d0e984d` (gate the ISA level on the CPU model) + `c75b005` (MADD family,
RECIP/RSQRT, PREFX) + `97dbcd4` (BC1).

**Upstream has the underlying bug.** Its CPU is a runtime choice, its
`CpuModel` carries a `MIPS4` const, its **interpreter checks that const at 17
sites** — and its **jitv2 checks it at none**, gating 45 sites on a
`cfg(feature = "mips4")` that is not in the default feature set. A stock
upstream build with `cpu = "r5000"` interprets MIPS IV correctly and refuses
to compile it.

Worth **~20% integer**, **~3x FP**, **~4x** on FP-branch-heavy code.

**Ordering constraint, verified:** chronologically this is MADD -> BC1 -> gate,
and `c75b005` *adds* 25 `cfg(feature = "mips4")` sites that `d0e984d` later
deletes. Cherry-picking in history order leaves the first two PRs inert on a
default build. Sending the gate first means **rewriting** MADD and BC1 without
their `cfg` attributes — real work, not a cherry-pick.

**Honest gap:** the FMA portability argument (aarch64 `FMADD`, x86-64 FMA3,
`LibCall.FmaF64` — all round once, so guests get bit-identical results) is
*reasoned and never tested*. We have only ever run on aarch64. Say so.

### B. jitv2 region formation — CP0 need not end a region

`6de06d1`. **~13-15% on syscall-bound work.** 677 COP0 sites in `unix.B`,
concentrated in the paths that run most.

Best-argued and riskiest thing here. The module doc names **exactly two**
things a region bakes in — FR mode and the pending-interrupt sample — and
excludes Status (12) and Cause (13) accordingly. That is the right shape of
argument, but it is an argument that the list is exhaustive, not a proof, and
unlike BC1 there is no large equivalence test behind it: the evidence is a
boot plus a live `j2 cop0 on/off` toggle that reverses cleanly. Send last.

### C. CP0 register width — MTC0 moves the whole register

`7c67833` + `f812f8e` (a stale comment that contradicted the code) + `ce0693c`
(the EntryHi test). **974 tests on upstream.**

Deviates from MIPS64 Vol II's literal wording on purpose; the rebuttal belongs
at the top of the PR body, not buried. Invisible on upstream's 32-bit guests —
latent correctness for them, not an observed bug. Say that too.

### D. XContext layout follows the CPU's VA width

`bf8af6b` + `9d08450`. **978 tests, 6 of them new.**

Verified **bit-for-bit identical at 40 bits** — mask `0x000000ffffffe000` is
`EH_VPN2_64`, ptebase mask and region shift both match. So upstream gets a
pure refactor plus tests on code that had none. Frame it that way, not as a
fix.

### E. Cache semantics — `Index_Store_Tag` must not write the line back

The 41 generic lines of `c52ce8e`, which is otherwise 825 lines of `ip32.rs`.
Isolated and tested: **applies alone, 973 tests, +39/-2, carries its own
test**, and a PR body already exists in `pr-drafts/`.

The strongest small fix in the inventory: `Index_Store_Tag` writing back sends
a line to an address derived from the tag being discarded, so a PROM
initialising the cache corrupts unrelated memory — on the O2 it landed on its
own stack. **The L2 path in the same `match` already gets this right and says
so.** Self-evident once seen.

### F. jitv2 memory accounting

`481c2cd`. **Upstream has this bug too**: it still carries
`HOST_PAGE_SIZE: u64 = 4096` while using the *packing*
`PagedArenaMemoryProvider`. On a 16 KB-page host — any Apple Silicon Mac —
three entries of 215, 300 and 48 bytes report as 49152 instead of 563, an
**87x over-count**, and that inflated number is what a reader consults to
judge whether the arena is near its flush threshold. Regression test included.

### G. Config precedence — a config file must not delete the caller's env var

The `src/config.rs` half of `b3c7192`. **Upstream has this byte for byte**:
`apply_env` calls `remove_var` whenever the config carries no `debug_log` key,
so `IRIS_DEBUG_LOG=l2c ./iris --config foo.toml` silently does nothing — under
a comment promising "env vars still override if set externally". Two tests, on
key names unique to them so they cannot race the suite.

**The rest of `b3c7192` does not apply upstream**: the devlog `cp0` mask
category and the ported tracers exist to serve IP28 traces upstream does not
have. Those go with IP28.

### H. Physical bus robustness — *three fixes trapped in one IP28 commit*

`556ac79` is four changes. Three are generic, and all three are the emulator
dying on something the guest is entitled to do:

- a bank mapped outside the lomem/himem windows was never unmapped again;
- **write only the slots that change** — the old shape did ~8200 non-atomic
  stores to 16-byte fat pointers on every MEMCFG write, racing the MC thread's
  own DMA fill. A torn read there is **a segfault in the emulator, and it was
  happening every few boots**;
- fill the dispatch table with a real device rather than null pointers whose
  vtable is live.

The fourth — the low-memory alias following where RAM actually is — is
IP28-observable only and goes with IP28. **Splitting this commit is the one
history rewrite the plan needs.**

### I. Orphaned `rules/` for fixes already merged upstream

The code went with the PR, the note never did: `ip7-timer-fix-concept.md`
(#120), `netbsd-confirms-clkid-is-a-generator-number.md` (#119),
`netbsd-wd33c93-empty-cdb.md` (#127), and
`seeq-enet-thread-stops-pumping-under-load.md`, whose code half turned out to
be upstream already.

## Every test watched failing

A test that ships as a PR's evidence has to be *seen* to fail without its fix.
Done for all eight features 2026-09-23, by reverting each change and re-running
its tests.

| feature | tests | fail when reverted | the rest |
|---|---|---|---|
| C MTC0 | 3 | 2 | the third guards the unchanged 32-bit path |
| D XContext | 6 | 3 | the other three pin the existing 40-bit behaviour — the refactor's safety net |
| E `Index_Store_Tag` | 1 | **0 → fixed → 1** | see below |
| F `code_bytes_used` | 1 | **missing → restored → 1** | see below |
| G `apply_env` | 2 | 1 | the other guards unchanged behaviour |
| A MADD | 6 | 1 | the dedicated rounding test; the broad equivalence test's operands do not reach the precision boundary |
| A BC1 | 8 | 3 | five are `bc1_fallback_*`, correct to pass under the old classification |
| B CP0 | 11 | 4 | the negatives (`cause_is_never_admitted`, `unsafe_cop0_still_ends_the_region`) correctly pass |

**Two of the eight were not testing anything.**

**E's test passed with the bug restored.** It built an `R4400Cache`, whose L2
is inclusive, so the writeback went to L2 and memory never changed. The
corrupting path is `IS_R5K` — non-inclusive or absent L2, line straight to RAM.
The O2 is an R5000. Fixed in `1311612`.

**F's test did not exist.** `481c2cd`'s message ends "`code_bytes_used_is_not_page_rounded`
fails if the rounding comes back" and gives the command to run it. The test was
written on the `jitv2-packing-stats` branch and lost when that branch came
through a conflicted merge; the message survived. Restored in `e0cce87`, which
also had to adapt it: `publish`/`claim` changed signature, and the model is one
function per page, so three sizes now means three pages rather than three
publishes into one.

Both would have shipped as a PR's stated evidence.

**Harness caveat:** restoring the file and re-running race each other; a
"1 failed" on the restore pass turned out to be cargo reading a half-written
file. Confirm the tree is clean (`git diff` empty) before believing the
second number.

## Withdrawn on review

- **`68fd9c8`** (debug-build test aborts) — **not a PR**. Both halves fix
  tests that do not exist upstream: `src/ip32.rs` is absent, and the TLB test
  is introduced by the JTLB commit itself. Folds into whichever of those ships.
- **`8f8a1c2`** (JTLB size per model) — safe (hot path verified untouched,
  2632 bytes more on a 520 KB structure) but **zero upstream benefit**: its
  only consumer is a 64-entry model and upstream has none. Goes with IP28.
- **`1c71178`** (256 MB banks) — one line, but `VALID_BANK_SIZES` is a single
  global list checked for *every* machine, and the MC cannot express a 256 MB
  bank at the IP22/IP24 base shift. As written it lets a user configure a
  machine that cannot exist. Wants profile-aware validation, or IP28.
- **`9889910`** (seeq `intpend` in `seeq status`) — **upstream already
  prints it**, and more besides (`rx_delivered`, `rx_refused`,
  `rx_nothing`). The cherry-pick conflicted because git would not add a
  duplicate line. Its rules file moves to group I.

## Group 2 — R10000 / IP28 machine support

~15 commits. A coherent series that ends with the real IP28 PROM passing POST
and IRIX 6.5.7 booting. It introduces `mips_cache_shadow.rs` and the
`R10K_CACHE_OPS` / `R10000ShadowCache` model, so **nothing in it is a
standalone fix** — it is "add a machine", and should be offered as such or not
at all.

Order it already has: `e5ba0f3` (PROM through memory sizing) -> `7c527bc`
(cache model + renamed cache ops) -> `135d88e` (R10000 Config) -> `794bbbc`
(TagHi, 64-bit tags, two ways) -> `21ccccf` (shadow cache) -> `971611c`,
`d64c45e`, `0c64fb2` (the MRU hunt) -> `f5a44c1` (POST passes) -> `5e787b0`,
`556ac79`, `1c71178`, `3228f6b` (profile) -> `e440a0f` (contracts confirmed).

Two members are **generic and should be lifted out** rather than sent with it:
`556ac79` (the low-memory alias must follow where RAM is) and `1c71178`
(256 MB banks). `556ac79` currently also carries an IP28 alias change — that
is the one history rewrite this needs.

## Group 3 — IP32 / O2

**29 commits**, the largest group, and the least finished: NetBSD 11 reaches
multi-user, IRIX stops after its banner. Introduces `src/ip32.rs` wholesale.
Not PR material until IRIX boots — offering a half-machine invites review
effort on something that will change.

One exception worth lifting: `c52ce8e` (**`Index_Store_Tag` must not write the
line back**) is a genuine cache-semantics fix that unblocked the O2 PROM, and
it is not O2-specific.

## Group 4 — ARCS firmware

**Not IP28/IP32-specific, despite the motivation.** Checked: `src/arcs.rs`
mentions neither machine, it is driven by the generic `IRIS_ARCS_BOOT` env
var, and `mips_exec_test.rs` exercises it on a plain machine. The scoping doc
says so outright — "the value is general: a working ARCS means direct kernel
boot on every machine".

The IP28 motivation has since evaporated anyway: that PROM now passes POST
(`f5a44c1`), so ARCS is no longer needed to boot it.

Still a *new subsystem* rather than a fix, so it is worth asking upstream
whether they want it before writing the PR. 4 commits: `7dc2544` (scope),
`8299de6` (observe), `fbd19f9` (the firmware), `d0f122f` (executor
interception).

## Group 5 — Documentation and rules

**31 commits touch no source at all.** Cheapest possible PRs, no behavioural
risk, and several document fixes that are *already upstream* — the code
travelled with the PRs, the `rules/` files never did (IP7 timer, HAL2 CLKID,
the NetBSD CDB work).

`ea3322f` is the provenance cleanup and should go with whatever it touches.

## Group 6 — Held back deliberately

- **`32c6188` hostcall** — host services behind a private syscall. Not ready
  to upstream (owner's call). Note it is already visible on the public fork.
- **`049a708` GIO slot-claim ledger** — on hold: someone is building an FPGA
  IRIS off this code. Also has a real review defect — `src/gio.rs` restates
  slot bases that `ioc.rs` already defines.
- **Everything `atomchild`** — 66 commits, a separate branch, not in scope.

## Already upstream — do not re-send

`bf54fcb` (wd33c93a CDB), `25dbd8a` (DNS reply source), `83e18b3` (HPC3 RX
chain), plus the two `rules/` notes that document them. These are our own
merged PRs #126-#128 returning through a merge.

## Suggested order

1. **`68fd9c8`** — the debug-build test aborts. It costs a reviewer nothing
   and unbreaks `cargo test` for everyone.
2. **`7c67833`** — MTC0. Drafted and verified.
3. **`bf8af6b`**, then **`8f8a1c2`** — once `bf8af6b` has a test.
4. **The jitv2 performance series** — `d0e984d`, `c75b005`, `97dbcd4`,
   `6de06d1`. The biggest wins, and the ones upstream is most likely to want.
5. **`481c2cd`**, **`c52ce8e`**, **`a9ea7e6`** — small standalone fixes.
6. **The `rules/` files for already-merged fixes** — free, and they close a
   documentation gap upstream currently has.
7. **IP28 as a series**, if and when there is appetite for a new machine.

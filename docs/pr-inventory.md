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

## Group 1 — General, and upstream already owns the code

The only group being prepared. Decided 2026-09-23: IP32/O2 and R10000/IP28
are held back as series, except two generic members lifted out of IP28 and one
out of IP32. **Every PR carries its own `rules/` or `docs/` file** — a fix and
the note explaining it travel together, and several of ours were orphaned
before by going out without theirs.

Each row was cherry-picked onto `upstream/main` and built.

| # | commits | what | verified | its doc |
|---|---|---|---|---|
| 1 | `68fd9c8` | two tests abort the whole binary in a debug build | 1090 rel / full debug suite | — |
| 2 | `7c67833` + `f812f8e` | **MTC0 moves the whole register into a 64-bit CP0 register** | **974 tests**, +79/-2 | — |
| 3 | `bf8af6b` + `9d08450` | **XContext's layout follows the CPU's VA width**, derived from the R4000 manual | **978 tests**, +206/-5, **6 tests** | — |
| 4 | `8f8a1c2` | the JTLB is as big as the CPU model says | **975 tests**, +136/-37 | — |
| 5 | `d0e984d` | jitv2: gate the ISA level on the CPU model, not a cargo feature — **~20% integer** | needs context | `rules/build/the-three-builds-we-actually-use.md` |
| 6 | `c75b005` | jitv2: the MIPS IV multiply-add family, RECIP/RSQRT, PREFX — **~3x FP** | standalone | `rules/jitv2/instructions-jitv2-still-interprets.md` |
| 7 | `97dbcd4` | jitv2: compile BC1 — **~4x** on a BC1-heavy loop | needs context | `rules/jitv2/bc1-is-an-ordinary-branch.md` + `rules/jitv2/bc1/` |
| 8 | `6de06d1` | jitv2: CP0 instructions stop ending every region — **~13-15%** syscall-bound | needs context | `rules/jitv2/cop0-does-not-have-to-end-a-region.md` |
| 9 | `481c2cd` | jitv2: stop page-rounding `code_bytes_used`, drop `HOST_PAGE_SIZE` | standalone | — |
| 10 | `b3c7192` | put the IP28 tracers on devlog; fixes `apply_env` deleting an externally-set `IRIS_DEBUG_LOG` | 1089 tests | `rules/build/tracing-goes-through-devlog.md` |
| 11 | `a9ea7e6` | seeq: report `intpend` in `seeq status` | standalone | `rules/irix/seeq-enet-thread-stops-pumping-under-load.md` |
| 12 | `556ac79` | **lifted from IP28**: the low-memory alias has to follow where RAM actually is | standalone; **carries an IP28 alias change that must be split out** | — |
| 13 | `1c71178` | **lifted from IP28**: allow 256 MB memory banks | standalone | — |
| 14 | `c52ce8e` | **lifted from IP32**: `Index_Store_Tag` must not write the line back | needs context | — |

Rows 5-8 are the jitv2 performance series and should go as one ordered set:
they build on each other, they are pure wins on upstream's own hot path, and
each is measured rather than argued.

### Orphaned `rules/` for fixes already merged upstream

The code went with the PR; the note never did. Free to send, and they close a
documentation gap upstream has right now.

| file | its merged PR |
|---|---|
| `rules/irix/ip7-timer-fix-concept.md` | #120 |
| `rules/hal2/netbsd-confirms-clkid-is-a-generator-number.md` | #119 |
| `rules/irix/netbsd-wd33c93-empty-cdb.md` | #127 |

### Measurement methodology, to go with rows 5-8

`rules/perf/guest-cpu-time-accounting-undercounts.md` (why these must be timed
by host wall clock — a broken `tms_utime` inflated a Dhrystone figure ~3x) and
`rules/testing/dhrystone-whetstone-on-an-irix-guest.md`.

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

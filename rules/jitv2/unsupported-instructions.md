# JIT v2: unsupported instructions

Inventory of MIPS III/IV instructions that `jitv2` cannot currently compile,
as of the point this file was written. Source of truth: `codegen.rs`'s
`lookup_semantics`/`lookup_cp1_semantics`/`lookup_branch_or_jump`/
`lookup_regjump` (what has a real emitter), mirrored exactly by
`jitv2/opcode_support.rs::has_emitter` — the single source of truth
`analyzer::classify()` now calls to decide `Excluded` vs `Sequential` for
every opcode not covered by an architectural exclusion or a resolved
branch/jump/regjump.

**Historical note**: earlier, `classify()` had its own independent opinion
(anything not explicitly excluded fell through to `Sequential`), separate
from codegen's emitter tables — the two drifted, and an opcode `classify()`
called `Sequential` but codegen had no emitter for would silently poison
(decline, not just exclude-in-place) every other instruction in whatever
region it was walked into (`compile_region`'s upfront rejection loop
declines the *whole* region if any visited instruction lacks an emitter).
Routing `classify()` through `has_emitter()` closed that class of bug
structurally: an unimplemented opcode is now `Excluded` at analysis time
(never enters a region, so it can't poison anything downstream of it
either) until an emitter actually exists, at which point it becomes
`Sequential` automatically — no `analyzer.rs` edit needed when a new
emitter lands. The list below is now purely "candidates to implement,"
with no separate poisoning risk to track.

Two different reasons an instruction doesn't compile, worth keeping
distinct:

- **Architecturally excluded** (`classify()` returns `Excluded` regardless
  of emitter coverage): the instruction is a deliberate region boundary —
  never visited, never compiled, always interpreted. Design decision (COP0,
  CACHE, LL/SC, COP2, BC1), not a gap to close.
- **Missing emitter** (would be `Sequential` once implemented): the real
  "candidates to implement" list below.

## Closed this session

Every instruction group below this line was listed as a gap earlier in this
file's history and has since been implemented (emitter in `codegen.rs`,
coverage in `opcode_support.rs`, equivalence test(s) in `equiv_test.rs`):
`MOVCI`/`MOVZ`/`MOVN`, `SYNC`/`PREF`, `DMULT`/`DMULTU`/`DDIV`/`DDIVU`,
`TGE`/`TGEU`/`TLT`/`TLTU`/`TEQ`/`TNE` (+ the six `OP_REGIMM` trap-immediate
variants), the unaligned load/store family (`LWL`/`LWR`/`SWL`/`SWR`/`LDL`/
`LDR`/`SDL`/`SDR`), `DADDI`, the FPU load/store family (`LWC1`/`LDC1`/
`SWC1`/`SDC1`, routed through `lookup_cp1_semantics` specifically so the
region-wide CU1/FR guard's trigger check still catches them), and
`MOVCF.fmt` (funct 0x11 under `RS_S`/`RS_D`).

`MOVCF.S`/`MOVCF.D` needed separate emitter bodies despite looking like a
conditional version of the already-implemented `FMOV` — unlike `emit_fmov_s`/
`_d` (which both alias to the same full-64-bit-slot copy because
`exec_fmov_s` itself uses `fpr_read_l`/`fpr_write_l` even for the `.s`
funct), `exec_fmovcf_s` uses the 32-bit `fpr_read_w`/`fpr_write_w` accessors
while `exec_fmovcf_d` uses the full-slot `fpr_read_d`/`fpr_write_d` ones —
genuinely different widths, not a S/D formatting difference that collapses
to the same bit pattern. An initial implementation copied `emit_fmov_s`'s
full-slot-copy shape for both functs and passed compilation, but the
`fmovcf_s_matches_interpreter_across_all_cc_and_tf_combinations` equivalence
test caught the divergence immediately (JIT clobbered fd's upper 32 bits
under FR=1; the interpreter's 32-bit write preserves them) — a reminder that
"looks like an existing pattern" isn't the same as "is architecturally the
same op," and equivalence tests earn their keep even on seemingly-trivial
conditional-move instructions.

Trap instructions raise `EXC_TR` through the same `emit_exception_exit`
shared-infrastructure path `DADDI`'s overflow trap already used — this was
confirmed in scope (not excluded by the "no COP0 side effects/privilege
changes" rule) since `deliver_exception`'s Cause/EPC/Status.EXL/vector-jump
side effects are uniform architectural-exception delivery, not something
specific to or introduced by these instructions; the hard-no is aimed at
instructions whose primary purpose is reading/writing CP0 registers or
switching privilege mode directly (MTC0, ERET, TLB\*, etc.), which none of
these do.

SWL/SWR/SDL/SDR needed one piece of new infrastructure beyond a plain
emitter: a masked-write JIT hook (`MipsCore::write64_masked_fn` /
`jit_write64_masked`, mirroring `MipsExecutor::write_data64_masked`) since
these instructions write a runtime-variable byte range at an unaligned
address, which no fixed-width `write8_fn`/`write16_fn`/`write32_fn`/
`write64_fn` call can express.

`PREF` (plain, top-level opcode 0x33) is a true no-op in this emulator
(`exec_pref` — see below) and was implemented; `PREFX` (COP1X-encoded,
funct 0x0F) was not — see "Architecturally excluded" below, it's a
different instruction with a real COP0-adjacent side effect despite the
similar name.

## Missing emitters

### OP_COP1X (MIPS IV) — entirely unhandled except PREFX (excluded, see below)

No `lookup_cp1_semantics` arm exists for `op == OP_COP1X` at all (only
`OP_COP1` is checked):

| Instruction | funct | Notes |
|---|---|---|
| `LWXC1` | 0x00 | indexed FP load word |
| `LDXC1` | 0x01 | indexed FP load double |
| `SWXC1` | 0x08 | indexed FP store word |
| `SDXC1` | 0x09 | indexed FP store double |
| `MADD_S` / `MADD_D` / `MADD_PS` | 0x20 / 0x21 / 0x26 | multiply-add (product rounded, not fused) |
| `MSUB_S` / `MSUB_D` / `MSUB_PS` | 0x28 / 0x29 / 0x2E | multiply-subtract (not fused) |
| `NMADD_S` / `NMADD_D` / `NMADD_PS` | 0x30 / 0x31 / 0x36 | negated multiply-add (not fused) |
| `NMSUB_S` / `NMSUB_D` / `NMSUB_PS` | 0x38 / 0x39 / 0x3E | negated multiply-subtract (not fused) |

`*_PS` (paired-single) variants are R5000/MIPS IV-only and likely out of
scope for an R4400 target regardless — but the scalar S/D forms
(`LWXC1`/`SWXC1`/`MADD_S`/`MADD_D`/etc.) are plausible IRIX/compiler output.

### OP_COP1 (funct field, RS_S/RS_D) — scalar FPU gaps

| Instruction | funct | Notes |
|---|---|---|
| `MOVZ.fmt` | 0x12 | MIPS IV, FP conditional move on GPR == 0 |
| `MOVN.fmt` | 0x13 | MIPS IV, FP conditional move on GPR != 0 |
| `RECIP.fmt` | 0x15 | MIPS IV reciprocal approximation |
| `RSQRT.fmt` | 0x16 | MIPS IV reciprocal-sqrt approximation |
| `CVT.PS.fmt` | 0x26 | MIPS IV, convert to paired-single |

`RS_PS` (paired-single format, 0x16) itself has no arm in
`lookup_cp1_semantics`'s outer `match rs` either — any instruction using
`fmt=PS` (not just `CVT.PS`) is unhandled.

## Architecturally excluded (not a gap — do not implement)

For completeness, so this list isn't mistaken for exhaustive coverage of
"missing." These are permanent region boundaries by design:

- **OP_COP0** (all of it): MFC0/DMFC0/CFC0/MTC0/DMTC0/CTC0/TLB\*/ERET/WAIT.
  CP0-dense code (exception vectors, TLB refill handlers) is exactly the
  code that must run through the interpreter's single-implementation
  exception/TLB semantics — see `rules/jitv2/jit-v2-design.md` §4.4 and
  the "exception vectors lose nothing" note there.
- **OP_COP2**: unimplemented coprocessor on this platform.
- **OP_CACHE**: cache-management instruction, self-modifying-code
  interactions live entirely in the interpreter (§7.3/§7.4).
- **OP_LL/OP_LLD/OP_SC/OP_SCD**: load-linked/store-conditional — atomicity
  semantics not (yet) modeled in compiled code.
- **RS_BC1** (COP1 conditional branch): condition-code-dependent target,
  the walker doesn't resolve it.
- **OP_LWC2/OP_LDC2/OP_SWC2/OP_SDC2**: CP2 memory ops, CP2 unimplemented.
- **FUNCT_SYSCALL/FUNCT_BREAK** (under OP_SPECIAL): software exceptions,
  always go through the interpreter's exception path.
- **PREFX** (`OP_COP1X`, funct 0x0F): unlike plain `PREF` (implemented as a
  true no-op), `MipsExecutor::exec_prefx` checks `STATUS_CU1` and raises
  `cpu_unusable` if it's clear — a genuine COP0-adjacent side effect, so
  this one stays excluded under the same rule as COP0 itself, not merely
  unimplemented.

## Fixed while building this inventory

`analyzer.rs`'s `classify()` used to share one match arm for both `OP_COP1`
and `OP_COP1X`, treating `OP_COP1X`'s `rs` field (a **base register** for
indexed loads/stores like `LWXC1`/`SWXC1`) as if it were `OP_COP1`'s format
selector — `rs == RS_BC1` (0x08) would misclassify e.g. `LWXC1 $f0, ($8)`
(using `$r8` as base) as `Excluded` (as if it were a BC1 conditional
branch) instead of `Sequential`. Fixed: `OP_COP1X` now has its own arm,
routed through `has_emitter()` like everything else, with no special-casing
of `rs` at all — see `analyzer::classify_cop1x_with_rs_equal_to_bc1_encoding_is_not_treated_as_a_branch`
for the regression test.

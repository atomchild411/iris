# What jitv2 still interprets, and which of it is worth compiling

Surveyed 2026-09-22, after enabling `mips4` turned out to be worth ~20% on
integer code and exactly nothing on FP. This is the follow-on question: what
else is left on the table.

## The gap

Of 204 `InstrKind` variants, **31 have no jitv2 emitter** — computed by
diffing the enum against `has_jitv2_emitter()` + `has_jitv2_support()` in
`mips_instr_stats.rs`.

They fall into three groups:

**Privileged / system (17) — correctly interpreted, leave alone.**
`Syscall Break Mfc0 Dmfc0 Mtc0 Dmtc0 Tlbr Tlbwi Tlbwr Tlbp Eret Wait Cache`
plus the atomics `Ll Sc Lld Scd`. These need to trap into the emulator by
nature. (CP0 access *ending* a compiled region is a separate, known cost —
see the jit-atomized notes — not an emitter-coverage problem.)

**MIPS IV FP arithmetic (13) — this is the opportunity.**
`Madd_s Madd_d Msub_s Msub_d Nmadd_s Nmadd_d Nmsub_s Nmsub_d`,
`Frecip_s Frecip_d Frsqrt_s Frsqrt_d`, `Prefx`.

**`Bc1` (1) — worse than a fallback: it is a region boundary.**
`analyzer.rs`'s `classify` returns `Classify::Excluded` for `RS_BC1`, so
branch-on-FP-condition *terminates* the compiled region rather than merely
bailing for one instruction.

**MIPS III has no gaps at all.** Every MIPS III compute instruction already
has an emitter. There is nothing to win by looking there.

## Measured usage, not guessed

Disassembled the actual MIPSpro `-Ofast -mips4` guest binaries (N32 MIPS-IV)
with `cross-binutils/bin/mips-sgi-irix6.5-objdump`:

| | `movz`/`movn` (mips4 enables) | MADD-family | `bc1*` | measured result |
|---|---|---|---|---|
| `dhry` | 7 + 3 | 0 | 0 | **20% faster with mips4** |
| `whetstone` | 0 | 29 `madd.d` + 18 `nmsub.d` | 1 | **flat** |

That is the whole story of the `mips4` A/B in one table, and it is causal
rather than correlational.

**`/usr/lib32/libm.so` is where it really bites** — every transcendental any
FP program calls:

| instruction | sites |
|---|---|
| `madd.d` | **2554** |
| `nmsub.d` | 528 |
| `bc1t` + `bc1f` | 456 + 396 = **852** |
| `msub.d` / `madd.s` / `msub.s` / `nmsub.s` / `nmadd.d` | 180 |
| `recip.d` / `recip.s` | 17 |

## Ranked, with the work each needs

1. **MADD/MSUB/NMADD/NMSUB — 8 emitters. Do this one.**
   ~3300 sites in libm alone. Pure arithmetic with no memory or addressing
   complexity; the existing `fadd`/`fmul` emitters are the template.

   **It must be a fused multiply-add.** The interpreter is
   `fs_val.mul_add(ft_val, fr_val)` (`exec_madd_d`), and Rust's `mul_add` is
   FMA — one rounding. Emitting `fmul` then `fadd` rounds twice and will
   diverge from the interpreter in the last bit, which `jitv2_lockstep`
   will (correctly) flag. Use Cranelift's `fma`; on aarch64 that is a single
   `FMADD`.

   Watch the operand mapping: COP1X puts `fr` in `rs`, `ft` in `rt`, `fs` in
   `rd` and `fd` in `sa`, and the result is `fd = fs*ft + fr`. NMADD/NMSUB
   negate the result. Flags: Invalid iff any of the *three* sources is a
   signalling NaN (`fpu_arith_flags_snan_only3_d`).

2. **`Bc1` — 852 sites in libm, and the cost compounds.**
   Not just the interpreted branch: the region ends there, so a hot FP loop
   containing one `bc1t` gets chopped up. Harder than group 1 — the analyzer
   comment explains it is excluded because the target is condition-code
   dependent — so read that reasoning before assuming it is merely
   unimplemented.

3. **RECIP/RSQRT — 4 emitters, 17 sites.** Low value, but trivial next to the
   MADD work and shares its shape.

4. **`Prefx` — 1 emitter, emits nothing.** A prefetch is architecturally a
   hint; `Pref` is already in the compiled set and `Prefx` is its indexed
   form. Near-zero sites in what we run, but it is a one-line arm.

## Gating

All of these are MIPS IV, so any new emitter needs the same `mips4` treatment
— which is the second argument for moving that gate from a cargo feature to
the runtime `C::MIPS4` const (see
[`../build/the-three-builds-we-actually-use.md`](../build/the-three-builds-we-actually-use.md)).
Adding 13 more `#[cfg(feature = "mips4")]` pairs to a gate that is already on
the wrong axis makes the eventual fix bigger.

Related: [`../perf/guest-cpu-time-accounting-undercounts.md`](../perf/guest-cpu-time-accounting-undercounts.md)
for why these must be timed by host wall clock.

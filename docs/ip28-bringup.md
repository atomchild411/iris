# Indigo2 IMPACT R10000 (IP28) bring-up

Running account, in the style of `ip32-o2-bringup.md`. IP28 is "Pacecar" — an
Indigo2 chassis with an R10000 CPU module, the last machine IRIX 6.5.22
supports, and the reason to want it is IRIX64 and a 1 GB memory ceiling.

**Status: the real PROM reaches the secondary cache diagnostic.** Memory sizing
passes. The cache test does not.

## Running it

```
IRIS_IP28=1 ./target/release/iris --config ip28.toml
```

`IRIS_IP28` is a temporary gate, not a machine profile. Everything IP28 needs
so far is a *decode* difference inside devices IP22 already has, and the
default of every gate reproduces IP22/IP24 exactly. A `MachineProfile::
Indigo2Ip28` replaces it once the shape of the machine is known; inventing the
profile first would have meant guessing which differences exist.

`IRIS_IP28_CACHEOPS=1` logs the first occurrence of each distinct CACHE
operation the guest issues. That is what identified the op encodings below.

Console must be **serial**. The PROM is IMPACT-era — its strings include
`IP28/GR2 PROM HQ Microcode`, `Solid Impact`, `High Impact` — and `src/mgras.rs`
is a register stub, so there is no graphics engine for it to drive. Let IRIX
bring REX3 up later.

## The PROM images

`ip28/ip28prom.070-1477-001.bin` and `-002.bin`, "SGI Version 6.2 Rev A IP28",
June and August 1996. `MIPS-R10000`, `PROM Monitor %s - 64 Bit`.

`-001` ships with a capture of a real machine's monitor dumping its own PROM.
110,368 words were checked against the image and **all of them match**, so the
image is a genuine dump rather than a reconstruction. Worth knowing, because
everything below is inferred from its behaviour.

No IRIX medium carries an IP28 PROM: 6.5.30, the original 6.5 of June 1998, and
the 6.5.22 set each ship exactly IP30, IP32 and io4 firmware, and no IP22 image
either. IRIX only ships PROM images for machines whose boot PROM it can reflash
in-system, which the Indigo2 family is not.

## What is different from IP22, so far

### MEMCFG decodes with a 24-bit base shift, not 22

The PROM writes base byte `0x60` and then probes `0x60000000`; base byte `0x20`
corresponds to `0x20000000`, which is where NetBSD loads IP28 kernels. Neither
address falls out of a 22-bit shift; both fall out of 24. The size arithmetic
scales with it — proven by the failing address, not by reasoning: with the 4 MB
granule the PROM failed at exactly the offset where `addr_mask` folded the
address back (`base+32MB` for 128 MB banks, `base+8MB` for 32 MB banks) and the
failure moved when the SIMM size moved.

IP28 RAM therefore lives at `0x20000000`, not `0x08000000`.

A theory that `0x60000000` was an uncached alias at `+0x40000000` is **wrong**,
and is recorded here because it was attractive: it predicted the right address,
and IRIX really does carry `ip28_enable_ucmem` / `ip28_return_ucmem` symbols.
`0x60000000` is an ordinary bank base once the shift is right.

### R10000 reassigns cache operations 5, 6 and 7

Confirmed against NetBSD `sys/arch/mips/include/cache_r10k.h`, and against the
PROM, which issues all of them.

| op | R4000 | R10000 | selects |
|----|-------|--------|---------|
| 5 | Hit_Invalidate | `Cache_Barrier` | I only — D/SD keep the R4000 meaning |
| 6 | Hit_WB_Invalidate / Fill | `Index_Load_Data` | I, D, SD |
| 7 | Hit_Writeback / Hit_Set_Virtual | `Index_Store_Data` | SI, SD |

The scoping matters: the PROM uses *both* readings of op 5, a barrier against
the instruction cache and an R4000 hit-invalidate against the secondary.

Index operations address the cache arrays directly, so they must not be
translated — an index is not a valid virtual address.

Bit 0 of the index selects the way on real silicon. This model is
direct-mapped, and bit 0 falls below the u64 slot index, so it drops out with
no special case.

## Where it stops

```
Secondary Cache Failure: Address: 0xa800000020000000 TAG walking 1s
                         Expected: 0x0000000000000001 SRAM U1
```

Open, and the next thing to work on. What is known:

- The PROM **never issues `Index_Load_Tag`**, so it is not reading tags back
  through the CACHE instruction. Whatever the tag test checks, it infers.
- It never issues `Index_Load_Data` either. It stores data by index and must be
  reading it back by ordinary load — which only works if the line's tag and
  state make that load hit.
- Implementing `Index_Store_Data` as a direct write into `l2.data` made things
  **worse**, not better: the PROM now stalls part-way through printing the
  failure message. The likely cause is ours — writing walking-1s patterns into
  slots backing lines the CPU still considers valid corrupts cached PROM code.
  Whatever the real semantics are, they cannot be "scribble on the data array
  and leave the tags alone".

The apparent *regression* from implementing `Index_Store_Data` was a
misreading. The PROM does not hang: it reaches a deliberate dead stop.

```
bfc012cc: beq fp, zero, 5   -> bfc012e4    ; no continuation registered?
bfc012e4: beq zero, zero,-1 -> bfc012e4    ; then spin here forever
```

`fp` is zero, so it takes the second branch. This is the PROM's panic path —
jump to a registered continuation, or halt. The truncated failure message is
just output unflushed when the run is killed; the message repeats five times
across retries before the PROM gives up.

A physical-address watch on `0x20000000` (`IRIS_IP28_WATCH`) records **no
accesses at all**, so the address in the failure text is a label for the region
under test, not something the test reached. It fails before touching memory.

CP0 `Config` was then found to be presented in R4000 format, which an R10000
PROM reads as "primary caches minimal, secondary cache size zero" — see below.
Fixing the layout did not clear the gate either; sweeping the 3-bit `SS` field
across 0..3 fails at every value. (An apparent pass at `SS=3` was a timing
artefact of too short a run. It is recorded here because it was briefly
believed.)

**Next step**: stop inferring and use the debugger. Put a breakpoint on
`0xbfc012cc` — the give-up decision — and read the stack there. The diagnostic's
own return addresses should be on it, which names the test routine directly
instead of guessing at its mechanism from op traces.

## The CPU model

`R10000Cache` in `mips_cache_v2.rs`. Real L1 and L2 sizes and line sizes, but
**all three modelled direct-mapped** where the real part is two-way, and no
attempt at out-of-order execution. That is deliberate: this branch wants speed,
and associativity reaches software only through the way-select bits of
`CACHE Index_*`, which an operating system only uses to flush everything.

It also avoids a real hazard. A two-way L1 *with* an L2 is a combination this
file has never had, and `fetch()` selects the two-way tag probe and the
L1I-resident decode slots under a single condition — such a part needs the
first with the second's alternative, and both arms compile. Direct-mapped plus
L2 is exactly the R4400's shape, so every associativity and decode-slot branch
is already right for this model.

What is *not* faked is anything software reads back: PRId, TLB size, MIPS IV
decoding, and the cache operation semantics above.

`model::R10000` is an explicit `MODEL` parameter rather than something inferred
from shape. It used to be inferred from `IC_WAYS == 2`, which held only while
"2-way" and "R5000" named the same part.

## Open questions

- `Index_Store_Data` semantics (above) — the live blocker.
- The CP0 `Config` `SS` (secondary size) encoding. The layout is from NetBSD's
  `MIPS4_CONFIG_*`; the other fields are `4096 << field`, but `SS` is not
  decoded anywhere to hand, so its base is unknown. `IRIS_IP28_SS` overrides it
  for sweeping. Pin the value and delete the override once something depends
  on it being right.
- The R10000 secondary cache TagLo layout. The current code uses the R4400
  format (`[31:13] ptag, [12:10] state, [9:7] PIdx`) and discards bits `[6:0]`
  and unrecognised state codes, so arbitrary bit patterns do not round-trip.
- The Virtual Coherency Exception. R4400 has it, R10000 does not, and this
  model takes the R4400 path. It needs an explicit gate before it fires
  spuriously; it has not fired yet.
- PRId is `0x0000_0900` (imp 9, revision 0). The revision is a guess. The PROM
  has not objected — it ran happily with an R5000 PRId before this model
  existed, so it is not strict about CPU identity.

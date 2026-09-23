# Indigo2 IMPACT R10000 (IP28) bring-up

Running account, in the style of `ip32-o2-bringup.md`. IP28 is "Pacecar" — an
Indigo2 chassis with an R10000 CPU module, the last machine IRIX 6.5.22
supports, and the reason to want it is IRIX64 and a 1 GB memory ceiling.

**Status: the whole of POST passes and IRIX 6.5.7 boots.** Memory sizing, the
secondary cache diagnostic and the rest of power-on all pass. The section
below is kept because the cache diagnostic was the hard part and what it
wanted is worth knowing; what finally satisfied it is at the end of it.

## Running it

```
./target/release/iris --config ip28irix.toml
```

The machine is `[machine] profile = "indigo2_ip28"` now. It began as an
`IRIS_IP28=1` environment gate — everything IP28 needed at first was a
*decode* difference inside devices IP22 already had — and became a real
profile once the shape of the machine was known. Inventing the profile first
would have meant guessing which differences exist.

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

## Where it stopped, and what the diagnostic actually wanted

```
Secondary Cache Failure: Address: 0xa800000020000000 MRU bit set
                         Expected: 0x0000000100000000 SRAM U2
                         Actual:   0x0000000000000000
```

The **tag tests pass** — both walking phases, after TagHi was carried
properly, TagLo widened to 64 bits, and the shadow given its two ways. What
remains is the MRU (most-recently-used) bit, which the PROM sets by writing
TagHi[31] and reads back at TagHi[0]. Different bits, which is what says it is
hardware state and not stored tag.

Modelling it as "one MRU way per set, set by the command bit" is not what the
PROM wants. The exact sequence, with `IRIS_SHADOW_CACHEOPS=1`
(which now traces tag operations only — tracing all of them is slower than the
emulation, because the PROM issues 65k+ `Index_Store_Data` ops and an
`eprintln` each stops the boot finishing at all):

```
IST va=…20000000  stores 0          + MRU command (TagHi[31])
IST va=…20000080  stores 0
ILT va=…20000001  -> our model returns 0x00000007ffffcdff
```

and the PROM reports `Expected: 0x0000000100000000, Actual: 0x0`.

Two things do not reconcile, and naming them is the useful part:

- The PROM expects a tag whose address bits are **zero** plus the MRU bit. The
  only slots holding zero at that point are `…20000000` and `…20000080`, not
  `…20000001`, which still holds a tag from the previous phase.
- `Actual: 0x0` is not what we return for `…20000001` either. So the read the
  PROM is failing on may not be the last one this trace captures — tracing
  perturbs the timing enough that the message and the operations interleave.

The tempting reading is that `va + 0x80` — one line — selects the other *way*
rather than the next *set*. That cannot be right on its own: the walking-tag
phase writes different tags to `…0000` and `…0001` and reads both back
distinctly, which passes only because bit 0 selects the way, and would fail if
`0x80` did. Both facts are solid and they do not yet fit one index scheme.

And then the decisive one. Across a whole run — **67 tag reads** — not a
single `Index_Load_Tag` returns zero. So `Actual: 0x0` cannot be the result of
any tag read this model services. Whatever the PROM compares for the MRU check,
it is not reading it through the path we implement.

The CP0 trace shows what it does read: after *every* tag read it also reads
`$26` (ECC), and `$26` is always zero here because nothing sets it. That is the
only register in the sequence whose value matches the reported `Actual`.

### Resolved

Three things were wrong, all of them in how the secondary cache reports state
that is *not* tag storage:

- **The MRU bit is one bit per set, shared between the ways.** Written through
  either way it marks the set, and reading the tag of either way reports it.
  The model above recorded which way had been marked and reported it only on
  that way, which satisfies nothing — the way the diagnostic reads is never
  the way it wrote.
- **The check bits ride with the data.** `Index_Store_Data` takes them from
  CP0 `ECC` and `Index_Load_Data` returns them there, so they are storage,
  not a computed value.
- **The MC must report revision 5 or better**, which the PROM reads out of
  SYSID.

Two claims made above are **wrong**, and are left in place because the wrong
turn is the instructive part:

- "the PROM sets TagHi[31] and reads back at TagHi[0] — different bits". It
  reads it back at TagHi[31], where it wrote it. Reporting it at TagHi[0]
  instead fails; reporting it at *both* passes, so bit 32 is simply a
  don't-care.
- "`Actual: 0x0` cannot be the result of any tag read this model services".
  True, and the reason is that it was never a tag read: the PROM prints both
  this test's and the ECC test's expectation in a 10-bit field at bits 41:32
  of the printed word. That is also where the `Expected: 0x0000000100000000`
  that suggested TagHi[0] comes from.

All three contracts were re-confirmed 2026-09-23 by switching each one off
and watching the PROM reject the result — see
[`../rules/irix/ip28-secondary-cache-contracts.md`](../rules/irix/ip28-secondary-cache-contracts.md),
which also has the full MRU protocol as traced and the measurement of the
check-bit field (ten bits per 64-bit word).

### Skipping the diagnostic does not work either

Two levers were tried, and both fail for the same underlying reason.

`diagmode=dc` makes this PROM family continue past failed diagnostics — both
the IP22 and IP28 PROMs carry the strings `Diagnostics failed.` and
`Continuing with diagmode=dc.`. A valid NVRAM was produced for it by booting
**IP22** in this emulator, setting `diagmode` and `console` from its own PROM
monitor, and saving the image (`nveeprom save`) — the two machines share the
93CS56 and the PROM family, so the format matches. It changes nothing: on IP22
the console shows `NVRAM checksum is incorrect` and `Running power-on
diagnostics` *before* anything else, while on IP28 the cache failure is the
**first** output with no preamble at all. The secondary cache test runs in
early POST, before the PROM has read its environment, so `diagmode` is never
consulted.

Sweeping the Config `SS` field does not skip the test either. It does change
it — `SS=4` runs longer and reports the `Actual:` line the other values omit —
so the PROM sizes its walk from `SS`, but no value makes it decide there is no
secondary cache to test.

That NVRAM image is worth keeping regardless (`ip28/nveeprom-ip28.bin`): it
carries `console=d`, which is what IRIX will want later.

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

- `Index_Store_Data` semantics. Partly answered: it carries ten check bits
  from CP0 `ECC` alongside the data, and the PROM walks a bit through them
  (confirmed 2026-09-23). What state it should leave the *line* in is still
  unverified.
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

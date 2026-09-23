# What the IP28 PROM requires of the secondary cache — confirmed by breaking it

Three properties of the R10000 secondary cache were worked out in September
2026 while getting the IP28 PROM through POST, and then written down as
contracts. They were *inferred* — each was the hypothesis that happened to
make the diagnostic stop complaining — and an inference that has only ever
been seen to hold is a belief, not a measurement.

Confirmed 2026-09-23 by switching each one off and watching the PROM reject
the result. Every row below is a console line from this machine, not a
recollection.

## The instrument

`IRIS_BREAK=<name>` (`src/faultinject.rs`) re-introduces one named bug. It is
validated and announced at startup, and an unknown name is fatal — a typo
that quietly gave you a control run would destroy the entire point, which is
knowing which of two runs you are looking at.

The MC revision needed no new code: `IRIS_IP28_MCREV` already sweeps it.

Harness: the real `ip28prom.070-1477-002.bin`, `profile = "indigo2_ip28"`,
`cpu = "r10000"`, 1 GB, a blank disk, headless. POST is over in **two
seconds**, so this is a cheap thing to re-run. The build had no `jitv2`; the
PROM is interpreted either way.

**The verdict is not "did it reach the boot line".** This PROM prints a
secondary-cache failure and then carries on to boot anyway, so a run that
boots can still have failed its diagnostic. A run is clean only when the
console carries no `Cache Failure`, `ECC Error` or `FATAL ERROR` line at all.

| run | result |
|---|---|
| control | POST clean, boot attempt in 2 s |
| `IRIS_BREAK=mru-per-way` | **2 × `Secondary Cache Failure … MRU bit set`** |
| `IRIS_BREAK=l2-ecc-not-stored` | **`ECC walking 1s` / `ECC walking 0s`, forever — POST never finishes** |
| `IRIS_IP28_MCREV=3` | **`FATAL ERROR: Rev A/BC MC detected--Need rev D or greater.`** |
| `IRIS_BREAK=mru-read-at-taghi0` | **2 × `… MRU bit set`** |
| `IRIS_BREAK=mru-read-at-both` | POST clean |

All five failure strings exist verbatim in the PROM binary, so the messages
are the firmware's own and not something the emulator invented.

## 1. The MRU bit is one bit per set, shared between the ways

Confirmed twice over: once by breaking it, and once by watching the whole
protocol. With `IRIS_SHADOW_CACHEOPS=1` the test is four sub-tests, and it
does the same thing each time — **writes the bit through one way and reads it
back through the other**:

```
IST va=…20000000 arg=0x8000000000000000   set the bit, way 0
IST va=…20000080 arg=0x0000000000000000   clear it in the NEXT set
ILT va=…20000001        returns 0x80000007ffffcdff   way 1 must report it
IST va=…20000000 arg=0x0000000000000000   clear it, way 0
IST va=…20000080 arg=0x8000000000000000   set it in the next set
ILT va=…20000001        returns 0x00000007ffffcdff   way 1 must NOT report it
```

then the same pair with the roles of way 0 and way 1 exchanged. The write to
the neighbouring set is there to make sure the answer came from the array and
not from a latch holding the last thing written.

A model that records *which way* was marked and reports the bit only on that
way therefore fails all four: the way that is read is never the way that was
written. That is the `mru-per-way` row, and its two console lines are
character-for-character the failure this bring-up was stuck on for three
hypotheses.

Note that the tag is untouched by all of this — `…20000001` keeps
`0x…07ffffcdff` across the whole sequence, with only bit 63 appearing and
disappearing. Hardware state, not tag storage, exactly as claimed.

## 2. It reads back where it was written: TagHi[31], not TagHi[0]

`docs/ip28-bringup.md` said the PROM "sets TagHi[31] and reads back at
TagHi[0] — different bits, which is what says it is hardware state". **That is
wrong**, and it cost three hypotheses' worth of work.

- `mru-read-at-taghi0` — report it at bit 32 only — **fails**.
- `mru-read-at-both` — report it at bit 63 *and* bit 32 — **passes**.

So bit 63 is load-bearing and bit 32 is a don't-care. The trace agrees: the
PROM writes `arg=0x8000000000000000` and accepts a tag with bit 63 set.

The TagHi[0] reading came from the PROM's own message,
`Expected: 0x0000000100000000`, which does look like bit 32. It is not a tag
word: the model that passes returns bit 63 set and bit 32 *clear*. Both this
test and the ECC test print their expectation in a 10-bit field at bits
41:32 of the printed word (see below), which is where that `1` comes from —
and it is also why `Actual: 0x0` could never be matched against any
`Index_Load_Tag` this model serviced. **It was never a tag read.** That
closes the open question the bring-up doc left under "Resume here".

## 3. The check bits ride with the data, through CP0 ECC

`Index_Store_Data` takes them from CP0 `ECC` ($26) and `Index_Load_Data`
returns them there. Make the load return zero instead and the PROM walks a
bit through them forever — it never leaves the test, which is the strongest
signal of the three.

The failure also **measures the field**, which nothing before had: the
walking-1s expectations are exactly

```
0x0000000100000000 … 0x0000020000000000      bits 32..41
```

and the walking-0s ones are their complement within `0x3FF`
(`0x3fe, 0x3fd, 0x3fb … 0x1ff`). **Ten check bits per 64-bit secondary data
word**, presented at bits 41:32. The trace shows the PROM writing exactly
those patterns: `ISD slot=1 data=0xfffffffffffffffe ecc=0x3fe`.

## 4. The MC must report revision 5 or better

`IRIS_IP28_MCREV=3` — the rev C value every other machine reports — gets

```
FATAL ERROR: Rev A/BC MC detected--Need rev D or greater.
```

and POST stops there permanently. Nothing subtle, but it was the one claim
with a quotable PROM string, and now the string has been seen to come out of
the emulator rather than out of a strings(1) dump.

## Why this is worth keeping

Four of these five faults were once the actual state of the model. Being able
to switch back to them means the next person to touch the secondary cache can
find out in two seconds whether a change broke a contract, instead of
rediscovering the contract. Add a fault rather than deleting one.

See also `docs/ip28-bringup.md` and
[`../perf/`](../perf/) for the measurement discipline this follows.

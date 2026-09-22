# Dhrystone and Whetstone on an IRIX guest

Period benchmarks built and run *inside* the guest, to compare against
results published for real SGI hardware. This is a different question from
`bench/`, which measures the emulator (guest MIPS, accuracy score); this
measures the guest the way a 1990s user would have.

## Build

Sources: netlib **Dhrystone 2.1** (`dhry-c` shar) and **Whetstone 1.2**
(Painter, 1998). Built natively with MIPSpro, using the flags the published
SGI figures cite:

```sh
cc -Ofast -mips4 -o whetstone whetstone.c -lm
cc -Ofast -mips4 -DTIMES -DHZ=100 -o dhry dhry_1.c dhry_2.c
```

Three things bite, none of which touch a measured loop:

- **Dhrystone's K&R `extern int times ();`** (dhry_1.c line 48) conflicts with
  the real prototype in `<sys/times.h>`. Delete the line.
- **`HZ` is undefined.** IRIX defines `CLK_TCK` as `sysconf(3)`, a call rather
  than a literal, so it cannot initialise Dhrystone's float expressions.
  `sysconf(_SC_CLK_TCK)` is **100** on 6.5; pass `-DHZ=100`.
- **Whetstone needs a large loop count.** `./whetstone 10000` prints
  `Insufficient duration- Increase the LOOP count`. Use 1000000 — about 20 s.

Dhrystone takes its run count on stdin: `echo 5000000 | ./dhry`.

## Validate the clock before believing any of it

A self-timed benchmark measures the **emulated** clock, not the work. Check it
before quoting a number — time a run from the host and compare:

```sh
START=$(date +%s); ssh ... './whetstone 1000000'; END=$(date +%s)
echo "host elapsed: $((END-START))s"      # compare with the reported Duration
```

Measured on IP28: **19 guest seconds vs 20 s host wall clock** — the extra
second is ssh and process startup, so the guest clock is honest. Note the two
benchmarks use *different* clocks: Dhrystone `times()` (CPU ticks via
`CLK_TCK`), Whetstone `time()` (wall seconds). Both check out.

The standing caveat: CP0 Count is a fixed 33 MHz that IRIX reports as a
195 MHz CPU, so these figures describe the *host* running iris in guest units.
Fine as a relative measure against the same benchmark on real hardware; not a
statement about what the original machine did.

## Results, emulated IP28 (R10000, IRIX 6.5.7, 768 MB)

| Benchmark | Emulated | Published, Indigo2 R10000/195 |
|---|---|---|
| Dhrystone 2.1 | 1,165,501 Dhry/s = **663 VAX MIPS** | 444.303 / 430.849 VAX MIPS |
| Whetstone 1.2 | **~4790 MWIPS** (4761.9 / 4347.8 / 5263.2) | not comparable, see below |

Dhrystone runs about **1.5x real hardware**, and every `should be:` check in
its output passes, so the work was not optimised away.

**Whetstone varies ±10% run to run** (19-23 s). One-second timer resolution on
a 20 s run is already ±5%. Never quote a single Whetstone figure as precise.

**Do not compare our Whetstone to the published column.** The program prints
**MWIPS** (`KIPS = 100.0*LOOP*II/secs`, then labelled "MIPS"), while the
published table's column is "MFLOPS" with no stated source, version or
methodology. They are different units, and 455 "MFLOPS" is implausible as real
floating point for a 195 MHz R10000 (peak ~390 MFLOPS with multiply-add, and
Whetstone never approaches peak) — so it is probably MWIPS mislabelled.
Probably is not a basis for a ratio.

## The finding that is ours

**FP/integer asymmetry: ~10x on Whetstone against ~1.5x on Dhrystone**, both
measured within one setup so the unit question does not apply. Dhrystone is
branch- and pointer-heavy; Whetstone is dense double-precision arithmetic that
evidently maps far better onto the host. Worth understanding before reading
any single number as "how fast the emulator is".

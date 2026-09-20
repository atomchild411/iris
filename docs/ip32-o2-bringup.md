# Bringing up IP32 (O2) in IRIS — what it would take

Assessment written 2026-09-19. Nothing implemented yet. Scope agreed with the
owner: **run the real O2 PROM** (not a synthetic ARCS), serial console only,
graphics explicitly out of scope.

## The PROM images we have

`ip32/ip32prom.rev4.18.bin` and `ip32/ip32prom.rev4.3.bin`, both 512 KiB, both
genuine IP32 firmware. Verified by parsing the section table rather than
trusting the filenames.

Rev 4.18:

| SHDR @ | name | length | version | load |
|---|---|---|---|---|
| 0x000008 | `sloader` | 0x04000 | 1.0 | 0xbfc00000 |
| 0x004008 | `env` | 0x00400 | 1.0 | — |
| 0x004408 | `post1` | 0x04d44 | 1.0 | 0xa0004000 (copied to RAM) |
| 0x009208 | `firmware` | 0x5fffc | **4.18** | 0x81000000 (.rodata +0x48e70) |
| 0x069208 | `version` | 0x00388 | 4.18 | ELF metadata |

SHDR layout, from the bytes: magic at +0, length at +4, type flags at +8,
NUL-padded name at +12, version string at +44, checksum at +52, then load
address and .rodata offset for `firmware`.

4.3 has the same five sections at the same offsets bar the last (0x68008).

**Rev 4.18 is the revision `ip32prom-decompiler` is tested against**, so the
published annotations line up with the image we hold.

## What tracing the reset path already told us

    bfc00000:  b      0xbfc00048        # jump over the SHDR
    bfc00048:  b      0xbfc003a8        # ...to the real entry
    bfc00100:  mtc0   a0,$17            # exception vector stub
    bfc00104:  lui    t8,0xb400
    bfc00108:  sd     a0,624(t8)        # 0xb4000270
    bfc0010c:  lui    t8,0xb400
    bfc00110:  addiu  t8,t8,512
    bfc00118:  sd     zero,0(t8)        # 0xb4000200

`0xb4000000` is uncached KSEG1 for physical `0x14000000` — **CRIME**. Against
NetBSD's `crimereg.h` those two offsets are `CRIME_MEM_ERROR_ECC_REPL` (0x270)
and `CRIME_MEM_CONTROL` (0x200): a memory-error exception handler, installed
before anything else runs.

Two facts fall out immediately:

- CRIME registers are **64-bit** — the PROM uses `sd`/`ld`, not `sw`/`lw`. Our
  bus plumbing must carry 64-bit accesses to this device.
- CRIME is live from the very first exception, so it cannot be stubbed out
  later; it is the first thing to build.

## The machine, from NetBSD's own GENERIC32_IP3x

```
crime0     at mainbus0 addr 0x14000000     memory + interrupt controller
crmfb0     at mainbus0 addr 0x16000000     framebuffer            [out of scope]
mace0      at mainbus0 addr 0x1f000000     I/O ASIC
  macepci0 +0x080000 → pci0 → ahc          Adaptec SCSI on real PCI
  mec0     +0x280000                       ethernet
  mavb0    +0x300000                       audio                  [out of scope]
  macekbc0 +0x320000                       keyboard               [out of scope]
  com0/1   +0x390000 / +0x398000           16550-style serial
  mcclock0 +0x3a0000                       RTC
```

Nothing of this exists in IRIS today: 17 source files mention IP22/IP24, and
`crime`/`mace` appear nowhere.

The serial console is a genuine simplification — `com` is a 16550 derivative,
far simpler than the Indy's Z85C30, and the PROM's built-in `env` section uses
the **same `console` variable** we already handle (`console=g` is its default,
`console=d` selects serial).

## Why the real PROM, and what that costs

NetBSD tries real ARCS first and only falls back to its own emulation:

```c
if (arcbios_init(ARCS_VECTOR) == 1) { ... arcemu_init(...) }
```

and `arcemu` only covers IP6/IP10/IP12/IP20/IP22. **IP32 needs real ARCS**, so
the PROM is not optional if the goal is stock NetBSD — and it is certainly not
optional for IRIX.

The consequence, and it is the important one: **virtio does not shrink this
job.** virtio removes devices the *guest OS* needs; the PROM POSTs the real
machine regardless. `post1` is ~20 KiB of memory test driven through CRIME, and
the firmware section will probe MACE and the PCI bridge. virtio's payoff comes
after the PROM is satisfied — it means we would not also have to emulate `mec`
and the `ahc` SCSI controller well enough for a running system, only well
enough for POST to not reject them.

## Licensing — settle this before writing code

- IRIS is **BSD-3-Clause**.
- `ip32prom-decompiler` and its annotations are **GPL-3.0**.

Reading the annotations to learn what a register does is fine. Copying comments,
structure, or generated assembly into IRIS is not. Any CRIME/MACE code must be
written independently, and commits that were informed by the annotations should
say so plainly. The PROM binaries themselves are SGI's; they stay out of the
repo exactly like `ip24prom.070-9101-011.bin` does.

## Milestone reached (and the check was wrong)

The success message POST prints on the good path is

    <post1> <SizeMEM> bank%d = 0x%0lx (128M)

The `<post1>` tag settles it: **we are executing post1.** The check below —
"trace the PC reaching 0xa0004000" — never fires because post1 runs *in place*
from the PROM window at 0xbfc04408+, not from the address its section header
names. The SHDR load address is where sloader would copy it, not where it first
executes. Worth remembering for any future section.

## Suggested first milestone

Not "boot NetBSD" — too far. Instead: **get `sloader` to hand off to `post1`.**

That needs only:

1. A `MachineType::Ip32` profile: reset at 0xbfc00000, the IP32 physical map,
   PROM image loaded at 0x1fc00000.
2. CRIME as a 64-bit register file at 0x14000000 — enough of `CRIME_MEM_*` and
   the revision/ID register to get through early setup.
3. RAM behind CRIME's bank controls.

Success is observable without any console at all: trace the PC reaching
0xa0004000. That single milestone will also answer the real unknown — how much
of CRIME the memory test actually exercises — which is what determines whether
the rest is weeks or months.

After that, in order: MACE register file → `com0` (first console output, and
the first time the PROM talks to us) → RTC → then decide how much of PCI/`ahc`
POST insists on.

## First milestone: what the PROM actually did

`src/ip32.rs` drives the real `MipsExecutor` against a minimal IP32 bus (RAM at
0, CRIME, a MACE stub, PROM). `cargo test --lib ip32::bringup -- --nocapture`,
with a PROM image present. It executes — and answered the question the
milestone existed to answer, on the first run.

**CRIME registers POST touches** (first-touch order):

| offset | | meaning |
|---|---|---|
| 0x0008 | R, then W | `CRIME_CONTROL` |
| 0x0208 … 0x0240 | W | eight consecutive qwords — the memory bank controls |

So the PROM reads CRIME_CONTROL, writes it, then programs **eight memory banks
at a linear 8-byte stride**. Worth noting against NetBSD's `crimereg.h`, which
names these in an interleaved order (`BANK_CTRL0` 0x208, `CTRL1` 0x218,
`CTRL2` 0x210 …). The PROM writes them straight through, which is the better
guide to what the hardware is.

Nothing else in CRIME was touched: no `TIME`, no interrupt registers, no ECC.
The memory-error handler installed at reset never fired, which is the outcome
we want.

**MACE**: 5 reads, 20 writes, against a stub returning zero — so MACE is needed
earlier than expected, but is evidently satisfied by benign answers for now.

**Where it stops.** At PC 0xbfc05c9c the PROM writes physical 0x40000000 and
then reads a register block at 0x40000080–0x400000dc. From
`mace/macereg.h`:

```c
#define MACE_PCI_NATIVE_VIEW    0x40000000
```

It is enumerating the **PCI bus**, looking for the Adaptec SCSI controller.
With nothing there it takes a bus error, wanders, and is lost by 2 M
instructions (ending in RAM at 0x00780824).

**So the gate is not CRIME — CRIME is nearly free.** Eight bank-control writes
and a control register is all POST asked for. The gate is the MACE PCI host
bridge. That is a much better position than the assessment above assumed, and
it changes the order of work: PCI config space next, not more CRIME.

## Second run: the empty PCI bus works

Adding a MACE PCI host bridge whose config reads return **all ones**, plus a
PCI native-view window that does the same, cleared it completely: **no bus
error, and zero unmapped accesses**. The PROM accepts a bus with nothing on it.

It never used `CONFIG_ADDR`/`CONFIG_DATA` at all — 0 config cycles. It only
touched the native-view window (1 read, 7 writes). So at this stage it is not
enumerating PCI so much as poking at where a device would be.

The bug that mattered was in the stub, not the bridge: returning **0** for an
absent device reads back as vendor ID 0x0000, which firmware takes for a
present-but-broken device. `0xffffffff` is the architectural "nobody home".

## Third and fourth gates: two timers and a 1-Wire chip

With PCI answered, the PROM spun 393,301 times on `MACE + 0x340000` —
`MACE_UST_MSC`, the free-running system timer. Its delay loops are

    ld   v1, 0(0xbf340000)      # now
    daddu v1, v1, a0            # target = now + delay
    1:  ld t0, 0(0xbf340000)
        sltu at, t0, v1
        bnezl at, 1b

so a constant hangs forever. Giving it a counter that advances (currently a
coarse `UST_STRIDE` per read — see the caveat in `ip32.rs`) dropped MACE reads
from 393,307 to 6 and let it run on.

It then went straight to hammering `MACE + 0x310008` —
`MACE_ISA_FLASH_NIC_REG` — 20,331 paired read-modify-writes between delays.

**That register is shared, and the first reading of it was wrong.** It does
carry the Dallas 1-Wire data line (`NIC_DATA` 0x08, `NIC_DEASSERT` 0x04), which
is what the name suggests and what I assumed. But capturing the actual values
written settled it: every write is **0x30**, and

```c
#define MACE_ISA_LED_RED    0x10
#define MACE_ISA_LED_GREEN  0x20
```

It is blinking the front-panel LED, not bit-banging a serial ID. The loop is

    bfc05e70:  lui  s0, 0xbf31          # s0 = 0xbf310008, the LED register
    bfc05e80:  ori  s1, s1, 0xa120      # delay = 0x7a120 = 500000
    bfc05e8c:  jal  0xbfc058b0          # delay(s1)
    bfc05e94:  ld   t7, 0(s0)
    bfc05e98:  xori t6, t7, 0x30        # toggle RED|GREEN
    bfc05e9c:  b    0xbfc05e8c          # forever

a panic blinker at 2 Hz. **Lesson: name the register by the bits that are
actually set, not by the name of the register.** Watching values, not
addresses, is what caught it.

## The real gate: `Error, no SIMM in bank0`

The blinker is entered from `bnez s1, ...` — `s1` is a size, and zero means
failure. Just before it, the PROM prints a string, and the string is in the
image at 0xbfc06d14:

    Error, no SIMM in bank0

So POST's **memory sizing** is what fails. The test itself is at 0xbfc05dc8:
three 64-bit patterns written into RAM and compared against constants held in
the PROM at 0xbfc06f90/f98/fa0, with any mismatch branching to the blinker.

What CRIME is told beforehand is the clue:

    0x0008 <- 0x0
    0x0208 <- 0x100      } all eight bank-control
    0x0210 <- 0x100      } registers get the same
    ...                  } value
    0x0240 <- 0x100

Eight banks, all programmed identically, then probed. Our RAM is flat at
physical 0 and ignores the bank registers entirely, so every bank aliases onto
the same store — which is exactly what a sizing algorithm is designed to detect
and report as "nothing there". ### SizeMEM passes: RAM is at 0x40000000

Tracing back from the failing `bne` gives the whole routine (0xbfc05c80):

    sd t6, 0(s3)          # base + 0
    sd t9, 0(a0)          # base + 0x01fffff8
    sd t1, 0(a1)          # base + 0x02000000
    sd t3, 0(a2)          # base + 0x07fffff8
    ld v1, 0(s3)          # read base + 0 back
    bne v1, t0, fail

It *does* write its patterns. The reason none appeared in RAM is that the
physical address they land on is **0x40000000** — visible all along as the
target of the bus-erroring `sd` at 0xbfc05c9c, which I had read as a PCI probe.

**O2 main memory is based at 0x40000000, not at 0.** This is the "RAM at a
non-zero base address" GXemul's documentation mentions as an O2 trait. It
collides with what `macereg.h` calls `MACE_PCI_NATIVE_VIEW`; memory has the
stronger claim on that address, so the PCI window is unmapped for now.

Moving RAM there, SizeMEM passes outright:

    0x40000000 W ffffffff00000000     0x40000000 R ffffffff00000000
    0x41fffff8 W fe00000701fffff8     0x41fffff8 R fe00000701fffff8
    0x42000000 W fdffffff02000000     0x42000000 R fdffffff02000000
    0x47fffff8 W f800000707fffff8     0x47fffff8 R f800000707fffff8
    0x48000000 W ...                  <- bank 1, at base + 128 MB

Every pattern reads back. MACE traffic drops from 20,334 accesses to **5** —
the panic blinker is gone — and CRIME's bank registers go from `0x100` to
`0x104` for the banks behind bank 0, so bit 2 is something POST sets once a
bank has been sized.

The one remaining unmapped access is `0x48000000 W`: bank 1, past our 128 MB.
That is correct behaviour rather than a bug — the PROM installs a bus-error
handler (`jr s8` at 0xbfc00388, with a CP0 dump behind it) specifically so it
can probe banks that are not populated.

### The lesson from this one

The bus error at 0x40000000 was visible in the *first* run of the evening. I
read it as "the PROM is enumerating PCI" because `macereg.h` has a constant at
that address, and then built a PCI window to satisfy it — which duly absorbed
the writes and made main memory invisible. The address was right there; the
wrong name was attached to it.

Twice in one session the same mistake: `MACE_ISA_FLASH_NIC_REG` read as 1-Wire
when the bits said LED, and 0x40000000 read as PCI when the access pattern said
memory. **Trust the access pattern over the header name.**

## Instrumentation, and then the milestone

Three instruments, each chosen because its absence had already cost a wrong
turn:

1. **PC attribution on every recorded access.** The bus publishes the current
   PC (`PcTap`) and the miss log records it. `0x00000018 W from PC 0xbfc051b4`
   is a one-line diagnosis; `0x00000018 W` alone is a puzzle.
2. **A printf tap.** post1's print routine at 0xbfc04d74 is a stub that emits
   nothing, but the format strings are in the image and the arguments are in
   `a0`-`a3` at the call. Reading them there recovers POST's narrative that the
   hardware never prints.
3. **Stall detection with CP0.** When the PC stops spreading, report the loop
   body *and* Cause/ExcCode/EPC. "Spinning at 0xbfc003a0" says nothing;
   "ExcCode=7 (DBE) EPC=0xbfc051b4" names the faulting instruction.

They paid for themselves immediately. The printf tap gave:

    <post1> <SizeMEM> Entering routine
    <post1> <SizeMEM> ECC off
    <post1> <SizeMEM> Initilaize BANK0
    <post1> <SizeMEM> bank0 = 0x... (128M)
    <post1> <SizeMEM> bank1 = 0x..., no simm installed
    ...
    <post1> <SizeMEM> MEM Size = 0x8000000 bytes

and the stall reporter turned two further hangs into single-line answers:

- `ExcCode=7 (DBE)` at 0xbfc051b4 → miss log → `0x00000018 W`. POST writes a
  walking pattern to **low physical memory** long after sizing RAM at
  0x40000000. Low memory aliases the base of RAM, as it does on the Indy.
- The same again at 0xbfc05088 → `0x00001000 R`, i.e. the alias was too small.

Two more gates, both found and fixed in minutes rather than by disassembly.

### The milestone, literally

    stopped after 112536 steps at PC 0xa0004000  <-- post1 entry
    stalls detected: 0
    unmapped accesses (0):

sloader sizes memory, copies post1 into RAM and jumps to it. No stalls, no
unmapped accesses, and the test now asserts it rather than merely printing it.

What it took, in total: CRIME's control and bank registers, a UST timer that
advances, a PCI bridge answering all-ones, a memory window that absorbs
accesses to unpopulated banks instead of bus-erroring, and a low-memory alias.
No PCI enumeration, no `ahc`, no graphics, no 1-Wire, no console.

## POST completes, and the PROM talks

Letting it run past post1 rather than stopping there: post1 executes, returns
to sloader, and sloader **prints to `com0`**. 522 bytes:

    ^@^A^B ... !"#$%&'()*+,-./0123456789:;<=>?@ABC ... ~^?  (the whole byte range)
    SL-9600-8E>

A character-set sweep — the UART testing itself — followed by **sloader's
prompt**: "SL", 9600 baud, 8 data bits, even parity. It then sits polling
`com0`'s line status for data-ready, which the disassembly confirms directly:

    0xbfc01ea0 -> 0xbf390507      com0 + 0x507, LSR
    0xbfc01e8c -> 0xbf390007      com0 + 0x007, RBR

exactly the MACE register layout implemented here. So **POST is finished and
the machine is waiting for console input.**

Receive is implemented (`Com16550::feed`) and the PROM does consume what it is
given — the queue drains — but neither `?` nor a bare carriage return draws any
response. `SL-9600-8E>` is a *serial loader* prompt, which most likely expects
a download protocol rather than typed commands; that is the next thing to
establish. Set `IRIS_IP32_INPUT` to experiment.

An open question this raises: on a healthy O2, does sloader stop here, or
should it hand off to the `firmware` section (4.18, at 0x81000000) — the actual
ARCS monitor? If the latter, something is still steering it into a fallback
path, and finding what is the next real gate.

## The console will not help during post1## Open questions

- ~~How much does `post1` insist on?~~ Answered: CRIME is cheap, MACE PCI is
  the gate. See above.
- ~~What does the PROM expect to find on PCI?~~ Answered: an empty bus is fine.
- What do CRIME's bank-control bits mean? POST writes `0x100` to all eight,
  then `0x104` to the ones behind bank 0. Bit 2 is set once a bank is sized.
- Where does the PCI native view really live, given memory owns 0x40000000?
- ~~Implement `com0` to read POST's messages.~~ **Done, and it does not help
  yet — see below.**
- 1-Wire is still there and still unimplemented; the PROM simply has not
  reached it yet.
- `UST_STRIDE` is a bring-up shortcut: the counter advances per read rather
  than with time. If the PROM ever derives a clock rate from it, that has to
  become time-based.
- Does the PROM require a framebuffer to be present even with `console=d`? The
  Indy PROM does not; the O2's `crt_option=1` in the default env suggests it at
  least looks.
- CPU: O2 shipped R5000/RM5200/RM7000/R10000/R12000. We have R5000, which is
  the cheapest target. RM7000 would need extra CP0 state, and the decompiled
  PROM branches on `CP0_PRID`, so it will notice.

Related: [`virtio-mmio-design.md`](virtio-mmio-design.md).

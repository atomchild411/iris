# Batched HOSTRW / VDMA — design sketch

Not built. Written while the analysis is fresh.

## What it costs today

Per 8 bytes transferred, derived from the 58.3 ns push cost measured this
session (~9.52 ns per guest instruction at ~105 MHz emulated):

**GIO -> Mem (reading pixels out of REX3)** — `mc.rs` `dma_read64` path:

| step | ns |
|---|---|
| `gfifo_push(REX3_DMA_PURE_GO)` | 58.3 |
| **`wait_idle()` — full pipeline drain, per qword** | ~200 (unmeasured) |
| 8x `translate_addr` + `write8`, **per byte** | ~200 |
| **total** | **~458 ns/qword ≈ 17 MB/s** |

**Mem -> GIO (writing pixels in)**:

| step | ns |
|---|---|
| 8x `translate_addr` + read, per byte | ~200 |
| `dma_write64` -> gfifo push | 58.3 |
| **total** | **~258 ns/qword ≈ 31 MB/s** |

Two separate pathologies:

1. **`wait_idle()` per qword.** The read path pushes a GO, drains the *entire*
   pipeline, reads one latched word, and repeats. That is the double round-trip:
   every 8 bytes costs a full producer/consumer synchronisation.
2. **Per-byte TLB translation.** Both directions walk one byte at a time calling
   `translate_addr` for each, even when the whole line sits in one page.

## The design

Move the bulk transfer *into* the shader, and let VDMA hand it a buffer rather
than a word.

**HOSTRW becomes an array, and a single transfer is length 1.** No batched-vs-
single special case anywhere: the pixel bodies index `buf[cursor]` instead of
touching a scalar `ctx.hostrw`, and a PIO register access is simply `cursor = 0,
len = 1`. One code path, one shader, no const-selected arm — which is the whole
point of doing it this way rather than adding a parallel batched pipeline.

### Where the buffer lives — not in `Rex3Context`

`Rex3Context` is `#[derive(Copy)]` and is serialised field-by-field to TOML on
every snapshot (`save_rex3_context`). A multi-megabyte array inside it would make
each context copy a multi-megabyte memcpy and write ~131k qwords of TOML per
snapshot per megabyte.

So: **the buffer sits on `Rex3`, beside `fb_rgb`/`fb_aux`; only the cursor and
length live in the context.** The shader already receives the two framebuffers as
pointer arguments — the host buffer is the third, and reaching it costs the same
as reaching a framebuffer. `Rex3Context` grows by ~8 bytes and the snapshot
format is untouched.

That also keeps the JIT working unchanged: `offset_of!(hostrw)` stays valid for
the cursor field, and Cranelift's existing HOSTRW handling can keep using element
0 until it is taught about the buffer.

### Writes (Mem -> GIO)

- Extend HOSTRW backing store from one qword to a real buffer (a few MB).
- VDMA leads the transfer with a **batch-write command** pushed through the
  GFIFO: `(buffer, length)` rather than N separate qword pushes.
- The consumer streams the payload into the buffer and hands it to the draw
  engine **once**, so the shader consumes host pixels from memory instead of
  being fed one qword at a time through `fetch_host_pixel`.

### Reads (GIO -> Mem)

- VDMA pushes a **batch-read GO** with the destination buffer and length.
- The shader fills the buffer directly — it already computes every pixel; it just
  writes them contiguously instead of latching one at a time into `ctx.hostrw`.
- VDMA consumes the whole buffer in **one** `dma_read` call.

That removes `wait_idle()` from the per-qword path entirely: one synchronisation
per *batch* instead of per 8 bytes.

### While in there: fix the per-byte TLB walk

Translate once per page and copy the run, rather than per byte. A 4KB page holds
512 qwords; at ~25 ns per translation that is most of the remaining cost in both
directions.

## Constraints this must respect

- **The pixel bodies already take the host pixel from `ctx`** (`fetch_host_pixel`
  / `store_host_pixel`, now in `rex3_generic.rs` and shape-driven). They index
  `buf[cursor]` instead of a scalar — and because a single transfer is just
  `len = 1`, there is no batched-vs-single branch to select. Same code, same
  shader, whatever the length.
- **`host_count`/`host_shift` still govern packing** — 1 to 16 pixels per 64-bit
  word depending on HOSTDEPTH/RWPACKED/RWDOUBLE. Batching changes where the words
  come from, not how they unpack.
- **`dma_read64`'s ordering inversion must be preserved.** CPU-driven PIO gets
  read-then-advance for free; VDMA has no software discard loop, so the current
  code pushes a GO, waits, *then* reads. A batch read has the same requirement at
  batch granularity: the buffer must be filled before VDMA consumes it.
- **BUS_BUSY/retry discipline.** `dma_write64` spins on BUS_BUSY because the DMA
  worker has no EXEC_RETRY. A batch command must either be accepted whole or
  rejected whole — the same rule `try_push2` enforces for the 64-bit pair, and
  the same bug class if it is got wrong (partial commit + retry = duplicate).
- **Two producers.** CPU and the VDMA worker both push. A batch command occupies
  one queue entry, so this does not change the producer-lock story, but the
  buffer itself needs a clear owner while in flight.

## Expected win

If the batch amortises both `wait_idle()` and the TLB walk over, say, a 4KB page
(512 qwords):

- read: ~458 ns/qword -> roughly the memcpy cost plus one sync per batch
- write: ~258 ns/qword -> likewise

Both directions should land in the hundreds of MB/s rather than tens. Worth
measuring rather than promising: the numbers above have one unmeasured term
(`wait_idle`), and the real gain depends on typical transfer length, which the
corpus does not record.

## Why it also cleans up `mc.rs`

The current `dma_loop` interleaves address translation, byte packing, direction
handling, zoom/stride bookkeeping and BUS_BUSY spinning in one nest. Batching
splits it: translate a run, hand a slice to the device, advance. The zoom/stride
logic stays, but it stops being tangled with per-byte bus access.

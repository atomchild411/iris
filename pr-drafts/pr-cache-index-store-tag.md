# cache: Index_Store_Tag must not write the line back

## The bug

`C_IST` (`cache Index_Store_Tag`) on the L1 data cache writes the line back to
memory before installing the new tag:

```rust
// Writeback dirty data before overwriting the tag.
self.writeback_l1d_line(idx, cascade);
self.invalidate_l1d_line(idx, true, cascade);
```

`Index_Store_Tag` writes CP0 `TagLo` into the tag of the line at the given
index. It does not write back. That is precisely what makes it usable to
initialise a cache at reset, when every tag is power-up garbage — and
initialising the cache is what the op exists for.

Writing back first sends the line's data to an address derived from the tag
that is about to be discarded. During cache init that tag is meaningless, so
the store lands on unrelated memory. `C_IWBINV`, a few arms above in the same
`match`, is the op that writes back.

The L2 path in this same arm already gets it right, and says so:

```rust
// C_IST does not writeback (it's used for cache init/invalidation).
self.invalidate_l2_line(idx);
```

Only the L1 data path flushed.

## How it was found

An SGI O2 (IP32) PROM, which initialises the D-cache the ordinary way: walk
every index, store an invalid tag.

```asm
mtc0    at, $28 (TagLo)
cache   Index_Store_Tag(PD), 0(t2)
bgez    t0, -8
```

One of those lines held a stale copy of the PROM's own stack. Watching the
physical address of the saved return address:

```
@787  0x40000fa4 W 0xbfc044b0   sw ra, 44(sp)  — the correct return address
@917  0x40000fa4 W 0xbfc04498   the writeback, from cache Index_Store_Tag(PD)
@928  0x40000fa4 R 0xbfc04498   lw ra, 44(sp)  — reads the clobbered value
```

`jr ra` then returned into the middle of the calling function rather than after
the call. That re-ran a stack-integrity cookie store, so the check after POST
compared a cookie captured with one stack pointer against a different one,
failed, and the PROM dropped into its serial loader instead of booting. With
the writeback removed it completes POST and hands off normally.

Nothing about this is specific to that machine: any firmware that initialises
the D-cache by index, with anything dirty in it, can have memory corrupted
underneath it.

## The change

Drop the writeback. The invalidate, the tag store, the `llbit` handling and the
L2 cascade are unchanged.

## Test

`index_store_tag_discards_the_line_instead_of_writing_it_back` dirties a line,
stores an invalid tag over it by index, and requires that memory still holds
its original contents.

No existing test covered this. The nearest one,
`cache_ops_do_not_divert_transparent_data_away_from_ram`, asserts that a write
*after* `C_IST` reaches RAM, which is a different property and still passes.

## Regression testing

Both guests on copy-on-write clones, never the working images:

| guest | result |
|---|---|
| NetBSD 11.0/sgimips, R4400 | boots multi-user; 16 MB random-data checksum round trip; gzip/gunzip round trip; `fsck_ffs -n` clean; no errors in `dmesg` |
| IRIX 6.5.22m, R4400 | boots to login; `hinv` reports the expected 320 MB; gzip is byte-identical across two runs of the same input; `sum /unix` stable |

`cargo test --lib`: 586 passed, 0 failed.

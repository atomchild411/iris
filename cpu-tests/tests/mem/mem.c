/* mem — loads, stores, and the unaligned-access instructions.
 *
 * The unaligned family (LWL/LWR/SWL/SWR and their doubleword counterparts) is
 * the densest concentration of emulator bugs in the whole ISA: each one is a
 * shift-and-merge whose direction depends on the byte offset AND the
 * endianness, they preserve the untouched bytes of the destination register,
 * and there are 4 (or 8) cases per instruction. This file walks every one.
 *
 * All addresses used here are in the suite's own .bss scratch area, reached
 * through KSEG0 (cached) unless a test says otherwise.
 */

#include "testlib.h"
#include "cp0.h"

/* Strict asm prologue — see the note in testlib.h. Abbreviated because it
 * appears on every block in this file. */
#define A ".set push; .set mips3; .set noreorder; .set nomacro; .set noat\n\t"
#define Z "\n\t.set pop"

#define OPAQUE(x) ({ __typeof__(x) __v = (x); __asm__ __volatile__("" : "+r"(__v)); __v; })

extern char _scratch_start[];

static volatile u8 *scratch(void) { return (volatile u8 *)_scratch_start; }

/* A known byte ramp the offset tests index into: 0x10, 0x11, 0x12, ... */
static void fill_pattern(void)
{
    volatile u8 *p = scratch();
    int i;
    for (i = 0; i < 32; i++) p[i] = (u8)(0x10 + i);
    SYNC();
}

/* ── plain loads: width and sign ──────────────────────────────────────────── */

static void t_load_widths_and_sign(void)
{
    volatile u8 *p = scratch();
    u64 v;

    /* Big-endian: byte 0 is the most significant. */
    p[0] = 0x80; p[1] = 0x81; p[2] = 0x82; p[3] = 0x83;
    p[4] = 0x84; p[5] = 0x85; p[6] = 0x86; p[7] = 0x87;
    SYNC();

    __asm__ __volatile__(A "lb %0, 0(%1)" Z : "=r"(v) : "r"(p));
    CHECK_EQ(v, 0xFFFFFFFFFFFFFF80ull);          /* sign-extended */

    __asm__ __volatile__(A "lbu %0, 0(%1)" Z : "=r"(v) : "r"(p));
    CHECK_EQ(v, 0x0000000000000080ull);          /* zero-extended */

    __asm__ __volatile__(A "lh %0, 0(%1)" Z : "=r"(v) : "r"(p));
    CHECK_EQ(v, 0xFFFFFFFFFFFF8081ull);

    __asm__ __volatile__(A "lhu %0, 0(%1)" Z : "=r"(v) : "r"(p));
    CHECK_EQ(v, 0x0000000000008081ull);

    __asm__ __volatile__(A "lw %0, 0(%1)" Z : "=r"(v) : "r"(p));
    CHECK_EQ(v, 0xFFFFFFFF80818283ull);          /* LW sign-extends bit 31 */

    __asm__ __volatile__(A "lwu %0, 0(%1)" Z : "=r"(v) : "r"(p));
    CHECK_EQ(v, 0x0000000080818283ull);          /* LWU zero-extends */

    __asm__ __volatile__(A "ld %0, 0(%1)" Z : "=r"(v) : "r"(p));
    CHECK_EQ(v, 0x8081828384858687ull);
}

static void t_store_widths(void)
{
    volatile u8 *p = scratch();
    u64 val = OPAQUE(0x1122334455667788ull);
    int i;

    for (i = 0; i < 8; i++) p[i] = 0;
    SYNC();
    __asm__ __volatile__(A "sb %0, 0(%1)" Z :: "r"(val), "r"(p) : "memory");
    CHECK_EQ(p[0], 0x88u);          /* the LOW byte of the register */
    CHECK_EQ(p[1], 0x00u);

    for (i = 0; i < 8; i++) p[i] = 0;
    SYNC();
    __asm__ __volatile__(A "sh %0, 0(%1)" Z :: "r"(val), "r"(p) : "memory");
    CHECK_EQ(p[0], 0x77u);          /* big-endian: high byte of the halfword */
    CHECK_EQ(p[1], 0x88u);
    CHECK_EQ(p[2], 0x00u);

    for (i = 0; i < 8; i++) p[i] = 0;
    SYNC();
    __asm__ __volatile__(A "sw %0, 0(%1)" Z :: "r"(val), "r"(p) : "memory");
    CHECK_EQ(p[0], 0x55u); CHECK_EQ(p[1], 0x66u);
    CHECK_EQ(p[2], 0x77u); CHECK_EQ(p[3], 0x88u);
    CHECK_EQ(p[4], 0x00u);

    for (i = 0; i < 8; i++) p[i] = 0;
    SYNC();
    __asm__ __volatile__(A "sd %0, 0(%1)" Z :: "r"(val), "r"(p) : "memory");
    CHECK_EQ(p[0], 0x11u); CHECK_EQ(p[7], 0x88u);
}

/* Displacements are sign-extended 16-bit values. */
static void t_negative_displacement(void)
{
    volatile u8 *p = scratch();
    u64 v;
    const volatile u8 *base = p + 16;
    p[8] = 0xAB;
    SYNC();
    __asm__ __volatile__(A "lbu %0, -8(%1)" Z : "=r"(v) : "r"(base));
    CHECK_EQ(v, 0xABull);
}

/* ── LWL / LWR ────────────────────────────────────────────────────────────── */
/*
 * Big-endian semantics, for offset o = addr & 3:
 *   LWL rt, addr  loads the (4-o) bytes starting at addr into the HIGH end of
 *                 rt, leaving rt's low o bytes untouched.
 *   LWR rt, addr  loads the (o+1) bytes ending at addr into the LOW end.
 * The canonical pair `lwl rt, 0(a); lwr rt, 3(a)` reassembles a full word from
 * any alignment. The 32-bit result is then sign-extended from bit 31.
 */
static void t_lwl_all_offsets(void)
{
    volatile u8 *p = scratch();
    unsigned o;
    /* rt pre-loaded with 0xAAAAAAAA; memory is 0x10 0x11 0x12 0x13 at p[0..3]. */
    static const u32 want32[4] = {
        0x10111213u,   /* o=0: all four bytes */
        0x111213AAu,   /* o=1: three bytes, low byte of rt preserved */
        0x1213AAAAu,   /* o=2 */
        0x13AAAAAAu,   /* o=3: one byte */
    };
    fill_pattern();

    for (o = 0; o < 4; o++) {
        u64 v = OPAQUE(0x00000000AAAAAAAAull);
        const volatile u8 *a = p + o;
        __asm__ __volatile__(A "lwl %0, 0(%1)" Z : "+r"(v) : "r"(a));
        CHECK_EQ_AT("off", o, v, (u64)(s64)(s32)want32[o]);
    }
}

static void t_lwr_all_offsets(void)
{
    volatile u8 *p = scratch();
    unsigned o;
    static const u32 want32[4] = {
        0xAAAAAA10u,   /* o=0: one byte into the low end */
        0xAAAA1011u,   /* o=1 */
        0xAA101112u,   /* o=2 */
        0x10111213u,   /* o=3: all four */
    };
    fill_pattern();

    for (o = 0; o < 4; o++) {
        u64 orig = 0x00000000AAAAAAAAull;
        u64 v = OPAQUE(orig);
        const volatile u8 *a = p + o;
        u64 want;
        __asm__ __volatile__(A "lwr %0, 0(%1)" Z : "+r"(v) : "r"(a));
        /*
         * The two parts genuinely differ here, which is why this is gated.
         * On an R4400 a *partial* LWR merges into the low half and leaves the
         * upper half of rt untouched; only the complete four-byte load
         * sign-extends from bit 31. An R5000 sign-extends at every offset.
         * Both measured on real silicon — R4400 rev 6.0 and R5000 rev 1.0.
         * LWL sign-extends everywhere on both, because it always writes all
         * 32 bits.
         *
         * No R4600 has run this. Its integer unit is QED's, like the R5000's,
         * but that is a guess, not a measurement — so for the R4600 either
         * measured behaviour passes and the log says which one it showed.
         * Anything else (a partial load that corrupts the low half, say)
         * still fails.
         */
        {
            u64 upper_kept = (orig & 0xFFFFFFFF00000000ull) | want32[o];
            u64 sign_ext   = (u64)(s64)(s32)want32[o];

            if (is_r4600() && o != 3) {
                con_printf("\n      [lwr off=%u on R4600: %s]", o,
                           v == sign_ext   ? "sign-extends, as an R5000" :
                           v == upper_kept ? "keeps the upper half, as an R4400" :
                                             "neither");
                /* Either answer passes; a mismatch reports against the
                 * R5000's. */
                want = (v == upper_kept) ? upper_kept : sign_ext;
            } else {
                want = (o == 3 || is_r5000()) ? sign_ext : upper_kept;
            }
        }
        CHECK_EQ_AT("off", o, v, want);
    }
}

/* The idiom that matters: an unaligned word load at every alignment. */
static void t_lwl_lwr_unaligned_word(void)
{
    volatile u8 *p = scratch();
    unsigned o;
    fill_pattern();

    for (o = 0; o < 4; o++) {
        u64 v = OPAQUE(0ull);
        const volatile u8 *a = p + o;
        u32 want = ((u32)(0x10 + o) << 24) | ((u32)(0x11 + o) << 16) |
                   ((u32)(0x12 + o) << 8)  |  (u32)(0x13 + o);
        __asm__ __volatile__(A "lwl %0, 0(%1)\n\t"
                               "lwr %0, 3(%1)" Z : "+r"(v) : "r"(a));
        CHECK_EQ_AT("off", o, v, (u64)(s64)(s32)want);
    }
}

/* ── SWL / SWR ────────────────────────────────────────────────────────────── */

static void t_swl_all_offsets(void)
{
    volatile u8 *p = scratch();
    unsigned o, i;
    u64 val = OPAQUE(0x00000000AABBCCDDull);

    /* SWL writes the high (4-o) register bytes to addr .. addr+3-o. */
    for (o = 0; o < 4; o++) {
        volatile u8 *a = p + o;
        fill_pattern();
        __asm__ __volatile__(A "swl %0, 0(%1)" Z :: "r"(val), "r"(a) : "memory");
        SYNC();
        for (i = 0; i < 8; i++) {
            u8 want = (i >= o && i < 4) ? (u8)(0xAA + 0x11 * (i - o))
                                        : (u8)(0x10 + i);
            CHECK_EQ_AT("o*8+i", o * 8 + i, p[i], want);
        }
    }
}

static void t_swr_all_offsets(void)
{
    volatile u8 *p = scratch();
    unsigned o, i;
    u64 val = OPAQUE(0x00000000AABBCCDDull);

    /* SWR writes the low (o+1) register bytes ending at addr, so memory[0..o]
     * receives register bytes [3-o .. 3] and memory[o+1..3] is untouched. */
    for (o = 0; o < 4; o++) {
        volatile u8 *a = p + o;
        fill_pattern();
        __asm__ __volatile__(A "swr %0, 0(%1)" Z :: "r"(val), "r"(a) : "memory");
        SYNC();
        for (i = 0; i < 8; i++) {
            u8 want = (i <= o) ? (u8)(0xDD - 0x11 * (o - i))
                               : (u8)(0x10 + i);
            CHECK_EQ_AT("o*8+i", o * 8 + i, p[i], want);
        }
    }
}

static void t_swl_swr_unaligned_word(void)
{
    volatile u8 *p = scratch();
    unsigned o;
    u64 val = OPAQUE(0x00000000AABBCCDDull);

    for (o = 0; o < 4; o++) {
        volatile u8 *a = p + o;
        fill_pattern();
        __asm__ __volatile__(A "swl %0, 0(%1)\n\t"
                               "swr %0, 3(%1)" Z :: "r"(val), "r"(a) : "memory");
        SYNC();
        CHECK_EQ_AT("off", o, p[o + 0], 0xAAu);
        CHECK_EQ_AT("off", o, p[o + 1], 0xBBu);
        CHECK_EQ_AT("off", o, p[o + 2], 0xCCu);
        CHECK_EQ_AT("off", o, p[o + 3], 0xDDu);
        /* Neighbours untouched. */
        if (o > 0) CHECK_EQ_AT("off", o, p[o - 1], (u8)(0x10 + o - 1));
        CHECK_EQ_AT("off", o, p[o + 4], (u8)(0x10 + o + 4));
    }
}

/* ── LDL / LDR / SDL / SDR ────────────────────────────────────────────────── */

static void t_ldl_ldr_unaligned_doubleword(void)
{
    volatile u8 *p = scratch();
    unsigned o;
    fill_pattern();

    for (o = 0; o < 8; o++) {
        u64 v = OPAQUE(0ull), want = 0;
        const volatile u8 *a = p + o;
        unsigned i;
        for (i = 0; i < 8; i++) want = (want << 8) | (u64)(0x10 + o + i);
        __asm__ __volatile__(A "ldl %0, 0(%1)\n\t"
                               "ldr %0, 7(%1)" Z : "+r"(v) : "r"(a));
        CHECK_EQ_AT("off", o, v, want);
    }
}

/* At offset o, LDL loads 8-o bytes into the high end and leaves the low o
 * bytes of the destination register alone. */
static void t_ldl_preserves_low_bytes(void)
{
    volatile u8 *p = scratch();
    unsigned o;
    fill_pattern();

    for (o = 0; o < 8; o++) {
        u64 v = OPAQUE(0xA5A5A5A5A5A5A5A5ull), want = 0;
        const volatile u8 *a = p + o;
        unsigned i;
        for (i = 0; i < 8 - o; i++) want = (want << 8) | (u64)(0x10 + o + i);
        for (i = 0; i < o; i++)     want = (want << 8) | 0xA5ull;
        __asm__ __volatile__(A "ldl %0, 0(%1)" Z : "+r"(v) : "r"(a));
        CHECK_EQ_AT("off", o, v, want);
    }
}

static void t_sdl_sdr_unaligned_doubleword(void)
{
    volatile u8 *p = scratch();
    unsigned o, i;
    u64 val = OPAQUE(0x1122334455667788ull);

    for (o = 0; o < 8; o++) {
        volatile u8 *a = p + o;
        fill_pattern();
        __asm__ __volatile__(A "sdl %0, 0(%1)\n\t"
                               "sdr %0, 7(%1)" Z :: "r"(val), "r"(a) : "memory");
        SYNC();
        for (i = 0; i < 8; i++)
            CHECK_EQ_AT("o*8+i", o * 8 + i, p[o + i], (u8)(0x11 * (i + 1)));
        if (o > 0) CHECK_EQ_AT("off", o, p[o - 1], (u8)(0x10 + o - 1));
        CHECK_EQ_AT("off", o, p[o + 8], (u8)(0x10 + o + 8));
    }
}

/* ── alignment faults ─────────────────────────────────────────────────────── */

static void t_unaligned_lw_faults(void)
{
    volatile u8 *p = scratch();
    u64 v = OPAQUE(0x1234ull);
    const volatile u8 *a = p + 1;
    exc_clear();
    __asm__ __volatile__(A "lw %0, 0(%1)" Z : "+r"(v) : "r"(a));
    CHECK_EXC(EXC_ADEL);
    CHECK_EQ(exc.badvaddr, (u64)(s64)(long)a);
    CHECK_EQ(v, 0x1234ull);                  /* destination untouched */
}

static void t_unaligned_ld_faults(void)
{
    volatile u8 *p = scratch();
    u64 v = OPAQUE(0x5678ull);
    const volatile u8 *a = p + 4;            /* 4-aligned, but LD needs 8 */
    exc_clear();
    __asm__ __volatile__(A "ld %0, 0(%1)" Z : "+r"(v) : "r"(a));
    CHECK_EXC(EXC_ADEL);
    CHECK_EQ(exc.badvaddr, (u64)(s64)(long)a);
    CHECK_EQ(v, 0x5678ull);
}

static void t_unaligned_sw_faults(void)
{
    volatile u8 *p = scratch();
    volatile u8 *a = p + 2;
    u64 val = OPAQUE(0xFFFFFFFFull);
    fill_pattern();
    exc_clear();
    __asm__ __volatile__(A "sw %0, 0(%1)" Z :: "r"(val), "r"(a) : "memory");
    CHECK_EXC(EXC_ADES);
    CHECK_EQ(exc.badvaddr, (u64)(s64)(long)a);
    /* Memory must be untouched — a partial store is not allowed. */
    CHECK_EQ(p[2], 0x12u);
    CHECK_EQ(p[3], 0x13u);
}

static void t_unaligned_lh_faults(void)
{
    volatile u8 *p = scratch();
    u64 v = 0;
    const volatile u8 *a = p + 1;
    exc_clear();
    __asm__ __volatile__(A "lh %0, 0(%1)" Z : "+r"(v) : "r"(a));
    CHECK_EXC(EXC_ADEL);
}

/* LWL/LWR and friends are defined at any alignment and must never fault. */
static void t_unaligned_family_never_faults(void)
{
    volatile u8 *p = scratch();
    unsigned o;
    fill_pattern();
    exc_clear();
    for (o = 0; o < 4; o++) {
        u64 v = 0;
        const volatile u8 *a = p + o;
        __asm__ __volatile__(A "lwl %0, 0(%1)\n\t"
                               "lwr %0, 3(%1)" Z : "+r"(v) : "r"(a));
    }
    CHECK_NO_EXC();
}

/* ── address spaces ───────────────────────────────────────────────────────── */

/*
 * The same physical bytes seen through the cached (KSEG0) and uncached (KSEG1)
 * windows. Each direction needs a different cache operation, and using the
 * wrong one is itself the interesting case:
 *
 *   KSEG0 write → KSEG1 read: the dirty line must be written back first
 *                             (Hit_Writeback_Invalidate).
 *   KSEG1 write → KSEG0 read: the stale line must be dropped WITHOUT being
 *                             written back (Hit_Invalidate). A writeback here
 *                             would push the old cached value over the store
 *                             that just went straight to memory.
 */
static void t_kseg0_kseg1_alias(void)
{
    volatile u32 *k0 = (volatile u32 *)_scratch_start;
    volatile u32 *k1 = (volatile u32 *)K1_PTR(_scratch_start);

    /* The two views must differ in exactly bit 29. */
    CHECK_EQ((u32)(unsigned long)k0 ^ (u32)(unsigned long)k1, 0x20000000u);

    *k0 = 0xC0FFEE00u;
    SYNC();
    dcache_wb_invalidate_range(k0, 16);
    SYNC();
    CHECK_EQ(*k1, 0xC0FFEE00u);

    *k1 = 0x5EED0000u;
    SYNC();
    dcache_invalidate_range(k0, 16);
    CHECK_EQ(*k0, 0x5EED0000u);
}

/* ── a load, and the instructions right after it ──────────────────────────── */

/*
 * A pipelined CPU finds a load's value one stage later than any other result,
 * so the instruction immediately behind a load is where forwarding goes wrong:
 * using the loaded register at once, one instruction later, as the base of the
 * next load, in a branch, as the value of a store that is itself reloaded,
 * after a second load into the same register, and with the load in a branch
 * delay slot. None of this is CPU-specific - it is plain ISA semantics - but a
 * core that stalls every load hides a whole class of bugs that a core which
 * stalls only when it must can have, so the sequences are spelled out.
 *
 * Every sequence runs twice: once with its lines not in the D-cache (the load
 * waits on a fill) and once with them already cached (the load completes in
 * the cache's own time), because those are different paths through a core.
 */
static void t_load_then_use(void)
{
    volatile u64 *q = (volatile u64 *)(_scratch_start + 256);
    const u64 v0 = 0x0123456789ABCDEFull;
    const u64 v2 = 0x1122334455667788ull;
    u64 r;
    int pass;

    for (pass = 0; pass < 2; pass++) {
        q[0] = v0;
        q[1] = (u64)(s64)(s32)(unsigned long)&q[2];   /* a KSEG0 pointer, sign-extended */
        q[2] = v2;
        q[3] = 0x8000000000000001ull;
        q[4] = 0;
        q[5] = 0x80FF7F0100000000ull;                   /* bytes 80 FF 7F 01 ... */
        SYNC();
        dcache_wb_invalidate_range(q, 6 * 8);           /* in memory, not in the cache */
        SYNC();
        if (pass == 1) {
            u64 warm = q[0] ^ q[1] ^ q[2] ^ q[3] ^ q[4] ^ q[5];
            (void)warm;                                  /* now the lines are cached */
        }

        /* Used by the very next instruction. */
        __asm__ __volatile__(A "ld $8, 0(%1)\n\tdaddu %0, $8, $8" Z
                             : "=r"(r) : "r"(q) : "$8");
        CHECK_EQ_AT("next", pass, r, v0 + v0);

        /* Used two instructions later, with an unrelated one between. */
        __asm__ __volatile__(A "ld $8, 0(%1)\n\tdaddu $9, $zero, $zero\n\tdaddu %0, $8, $9" Z
                             : "=r"(r) : "r"(q) : "$8", "$9");
        CHECK_EQ_AT("after one", pass, r, v0);

        /* As the base of the next load: a pointer chase. */
        __asm__ __volatile__(A "ld $8, 8(%1)\n\tld %0, 0($8)" Z
                             : "=r"(r) : "r"(q) : "$8");
        CHECK_EQ_AT("chase", pass, r, v2);

        /* In the branch right behind it. The output is written only at the
         * end: GCC may give an "=r" output the same hard register as an
         * input, and the compare needs %2 intact. */
        __asm__ __volatile__(A "ld $8, 0(%1)\n\t"
                               "beq $8, %2, 1f\n\t"
                               "daddu $9, $zero, $zero\n\t"
                               "daddiu $9, $zero, 7\n\t"
                               "1: daddiu %0, $9, 1" Z
                             : "=r"(r) : "r"(q), "r"(OPAQUE((u64)v0)) : "$8", "$9");
        CHECK_EQ_AT("branch", pass, r, 1u);

        /* Stored at once, and the store reloaded at once. */
        __asm__ __volatile__(A "ld $8, 0(%1)\n\tsd $8, 32(%1)\n\tld %0, 32(%1)" Z
                             : "=r"(r) : "r"(q) : "$8", "memory");
        CHECK_EQ_AT("store reload", pass, r, v0);

        /* A second load into the same register wins. */
        __asm__ __volatile__(A "ld $8, 0(%1)\n\tld $8, 16(%1)\n\tdaddu %0, $8, $zero" Z
                             : "=r"(r) : "r"(q) : "$8");
        CHECK_EQ_AT("reload", pass, r, v2);

        /* Narrow loads extend before the next instruction sees them. */
        __asm__ __volatile__(A "lb $8, 40(%1)\n\tdaddu %0, $8, $zero" Z
                             : "=r"(r) : "r"(q) : "$8");
        CHECK_EQ_AT("lb", pass, r, 0xFFFFFFFFFFFFFF80ull);
        __asm__ __volatile__(A "lhu $8, 42(%1)\n\tdaddu %0, $8, $8" Z
                             : "=r"(r) : "r"(q) : "$8");
        CHECK_EQ_AT("lhu", pass, r, 0x7F01u + 0x7F01u);
        __asm__ __volatile__(A "lw $8, 4(%1)\n\tdsubu %0, $zero, $8" Z
                             : "=r"(r) : "r"(q) : "$8");
        CHECK_EQ_AT("lw", pass, r, 0x0000000076543211ull);

        /* The load in a delay slot, used at the branch target. */
        __asm__ __volatile__(A "beq $zero, $zero, 1f\n\t"
                               "ld $8, 0(%1)\n\t"
                               "daddu $8, $zero, $zero\n\t"
                               "1: daddu %0, $8, $8" Z
                             : "=r"(r) : "r"(q) : "$8");
        CHECK_EQ_AT("delay slot", pass, r, v0 + v0);
    }
}

static const struct test tests[] = {
    TEST("mem/load_widths_sign",      t_load_widths_and_sign,            CPU_ALL),
    TEST("mem/store_widths",          t_store_widths,                    CPU_ALL),
    TEST("mem/negative_displacement", t_negative_displacement,           CPU_ALL),
    TEST("mem/lwl_all_offsets",       t_lwl_all_offsets,                 CPU_ALL),
    TEST("mem/lwr_all_offsets",       t_lwr_all_offsets,                 CPU_ALL),
    TEST("mem/lwl_lwr_unaligned",     t_lwl_lwr_unaligned_word,          CPU_ALL),
    TEST("mem/swl_all_offsets",       t_swl_all_offsets,                 CPU_ALL),
    TEST("mem/swr_all_offsets",       t_swr_all_offsets,                 CPU_ALL),
    TEST("mem/swl_swr_unaligned",     t_swl_swr_unaligned_word,          CPU_ALL),
    TEST("mem/ldl_ldr_unaligned",     t_ldl_ldr_unaligned_doubleword,    CPU_ALL),
    TEST("mem/ldl_preserves_low",     t_ldl_preserves_low_bytes,         CPU_ALL),
    TEST("mem/sdl_sdr_unaligned",     t_sdl_sdr_unaligned_doubleword,    CPU_ALL),
    TEST("mem/unaligned_lw_faults",   t_unaligned_lw_faults,             CPU_ALL),
    TEST("mem/unaligned_ld_faults",   t_unaligned_ld_faults,             CPU_ALL),
    TEST("mem/unaligned_sw_faults",   t_unaligned_sw_faults,             CPU_ALL),
    TEST("mem/unaligned_lh_faults",   t_unaligned_lh_faults,             CPU_ALL),
    TEST("mem/unaligned_family_ok",   t_unaligned_family_never_faults,   CPU_ALL),
    TEST("mem/kseg0_kseg1_alias",     t_kseg0_kseg1_alias,               CPU_ALL),
    TEST("mem/load_then_use",         t_load_then_use,                   CPU_ALL),
};

const struct test_group group_mem = {
    "mem", tests, sizeof(tests) / sizeof(tests[0])
};

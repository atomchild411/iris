//! A cache that is visible to software but stays out of the data path.
//!
//! Every load, store and instruction fetch goes straight to memory, exactly as
//! in [`PassthroughCacheOf`](crate::mips_cache_v2::PassthroughCacheOf). What
//! this adds is a *shadow*: tag and data arrays that only the CACHE
//! instruction ever touches.
//!
//! The reasoning is that a cache's effect on a functional emulator is entirely
//! observational. Its timing is invisible to the guest, and its contents are
//! invisible too as long as they agree with memory — which here they trivially
//! do, because memory is the only store. What *is* visible is the CACHE
//! instruction, the geometry reported through CP0 Config, and the diagnostics
//! a PROM runs against both. So model those, and let the host CPU — which
//! already has real caches, real speculation and real out-of-order execution —
//! get on with it unimpeded.
//!
//! Two things fall out of this beyond speed:
//!
//! - **Coherency is exact and free.** There is no stale line to miss on a
//!   self-modifying store, no L1/L2 inclusion policy to get wrong, and no way
//!   for a missed writeback to lose data.
//! - **Diagnostics round-trip.** The shadow stores whatever bits are written
//!   to it and returns them unchanged, so a walking-1s test over tag or data
//!   SRAM passes without anyone having to know the hardware's field layout.
//!   Where the emulator *must* know the layout — to decide whether a line is
//!   valid, say — that is a separate question from storing the bits.
//!
//! The shadow is deliberately not consulted by `read`/`write`/`fetch`. If it
//! ever needs to be, this type is the wrong shape and should say so loudly
//! rather than growing a slow path.

use std::cell::UnsafeCell;
use std::sync::Arc;

use crate::mips_cache_v2::{
    cache_op_name, CpuModel, FetchInstrResult, MipsCache, C_ILT, C_IST, C_R10K_CBARRIER,
    C_R10K_ILD, C_R10K_ISD, CACH_PD, CACH_PI, CACH_SD, CACH_SI,
};
use crate::mips_exec::{DecodedInstr, FLAG_NOT_DECODED};
use crate::traits::{BusDevice, BusRead64};

/// Shadow tag and data arrays for one cache.
struct Shadow {
    /// One raw tag word per line, stored and returned verbatim.
    tags: Box<[u32]>,
    /// Data array in u64 slots. Empty where the model has no data shadow.
    data: Box<[u64]>,
}

impl Shadow {
    fn new(lines: usize, data_words: usize) -> Self {
        Self {
            tags: vec![0u32; lines.max(1)].into_boxed_slice(),
            data: vec![0u64; data_words].into_boxed_slice(),
        }
    }
}

/// A `MipsCache` whose contents are only ever observed through CACHE ops.
///
/// Const parameters carry the geometry that software can read back, and the
/// processor identity. Nothing here affects the speed of a load.
pub struct ShadowCache<
    const IC_SIZE: usize,
    const IC_LINE: usize,
    const DC_SIZE: usize,
    const DC_LINE: usize,
    const L2_SIZE: usize,
    const L2_LINE: usize,
    const MIPS4: bool,
    const PRID: u32,
    const FIR: u32,
    const TLB_ENTRIES: usize,
    const R10K_OPS: bool,
> {
    downstream: Arc<dyn BusDevice>,
    llbit: UnsafeCell<bool>,
    lladdr: UnsafeCell<u32>,
    /// Somewhere to decode into. Not a cache line — there is no caching.
    fetch_scratch: UnsafeCell<DecodedInstr>,
    ic: UnsafeCell<Shadow>,
    dc: UnsafeCell<Shadow>,
    l2: UnsafeCell<Shadow>,
}

// Safety: the CPU thread is the only accessor, as for every other cache model
// in this crate.
unsafe impl<
        const IC_SIZE: usize, const IC_LINE: usize, const DC_SIZE: usize, const DC_LINE: usize,
        const L2_SIZE: usize, const L2_LINE: usize, const MIPS4: bool, const PRID: u32,
        const FIR: u32, const TLB_ENTRIES: usize, const R10K_OPS: bool,
    > Send
    for ShadowCache<IC_SIZE, IC_LINE, DC_SIZE, DC_LINE, L2_SIZE, L2_LINE, MIPS4, PRID, FIR, TLB_ENTRIES, R10K_OPS>
{
}
unsafe impl<
        const IC_SIZE: usize, const IC_LINE: usize, const DC_SIZE: usize, const DC_LINE: usize,
        const L2_SIZE: usize, const L2_LINE: usize, const MIPS4: bool, const PRID: u32,
        const FIR: u32, const TLB_ENTRIES: usize, const R10K_OPS: bool,
    > Sync
    for ShadowCache<IC_SIZE, IC_LINE, DC_SIZE, DC_LINE, L2_SIZE, L2_LINE, MIPS4, PRID, FIR, TLB_ENTRIES, R10K_OPS>
{
}

impl<
        const IC_SIZE: usize, const IC_LINE: usize, const DC_SIZE: usize, const DC_LINE: usize,
        const L2_SIZE: usize, const L2_LINE: usize, const MIPS4: bool, const PRID: u32,
        const FIR: u32, const TLB_ENTRIES: usize, const R10K_OPS: bool,
    > ShadowCache<IC_SIZE, IC_LINE, DC_SIZE, DC_LINE, L2_SIZE, L2_LINE, MIPS4, PRID, FIR, TLB_ENTRIES, R10K_OPS>
{
    const IC_LINES: usize = if IC_LINE == 0 { 0 } else { IC_SIZE / IC_LINE };
    const DC_LINES: usize = if DC_LINE == 0 { 0 } else { DC_SIZE / DC_LINE };
    const L2_LINES: usize = if L2_LINE == 0 { 0 } else { L2_SIZE / L2_LINE };

    pub fn new(downstream: Arc<dyn BusDevice>) -> Self {
        Self {
            downstream,
            llbit: UnsafeCell::new(false),
            lladdr: UnsafeCell::new(0),
            fetch_scratch: UnsafeCell::new(DecodedInstr::default()),
            ic: UnsafeCell::new(Shadow::new(Self::IC_LINES, 0)),
            dc: UnsafeCell::new(Shadow::new(Self::DC_LINES, 0)),
            // Only the secondary keeps a data shadow: it is the one a PROM
            // walks with Index_Store_Data, and a 1 MB array is cheap once.
            l2: UnsafeCell::new(Shadow::new(Self::L2_LINES, L2_SIZE / 8)),
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn shadow(&self, sel: u32) -> &mut Shadow {
        unsafe {
            match sel {
                CACH_PI => &mut *self.ic.get(),
                CACH_PD => &mut *self.dc.get(),
                _ => &mut *self.l2.get(),
            }
        }
    }

    /// Line index for a CACHE index operation. The way-select bit that real
    /// two-way hardware keeps at bit 0 is below the line granularity and
    /// simply folds away, which is what makes a direct-mapped shadow able to
    /// answer for a set-associative part.
    fn line_index(&self, sel: u32, virt_addr: u64) -> usize {
        let (line, lines) = match sel {
            CACH_PI => (IC_LINE, Self::IC_LINES),
            CACH_PD => (DC_LINE, Self::DC_LINES),
            _ => (L2_LINE, Self::L2_LINES),
        };
        if line == 0 || lines == 0 {
            return 0;
        }
        ((virt_addr as usize) / line) % lines
    }
}

impl<
        const IC_SIZE: usize, const IC_LINE: usize, const DC_SIZE: usize, const DC_LINE: usize,
        const L2_SIZE: usize, const L2_LINE: usize, const MIPS4: bool, const PRID: u32,
        const FIR: u32, const TLB_ENTRIES: usize, const R10K_OPS: bool,
    > From<Arc<dyn BusDevice>>
    for ShadowCache<IC_SIZE, IC_LINE, DC_SIZE, DC_LINE, L2_SIZE, L2_LINE, MIPS4, PRID, FIR, TLB_ENTRIES, R10K_OPS>
{
    fn from(downstream: Arc<dyn BusDevice>) -> Self {
        Self::new(downstream)
    }
}

impl<
        const IC_SIZE: usize, const IC_LINE: usize, const DC_SIZE: usize, const DC_LINE: usize,
        const L2_SIZE: usize, const L2_LINE: usize, const MIPS4: bool, const PRID: u32,
        const FIR: u32, const TLB_ENTRIES: usize, const R10K_OPS: bool,
    > CpuModel
    for ShadowCache<IC_SIZE, IC_LINE, DC_SIZE, DC_LINE, L2_SIZE, L2_LINE, MIPS4, PRID, FIR, TLB_ENTRIES, R10K_OPS>
{
    const MIPS4: bool = MIPS4;
    const PRID: u32 = PRID;
    const FIR: u32 = FIR;
    const TLB_ENTRIES: usize = TLB_ENTRIES;
    const NAME: &'static str = "shadow";
    const R10K_CACHE_OPS: bool = R10K_OPS;
}

impl<
        const IC_SIZE: usize, const IC_LINE: usize, const DC_SIZE: usize, const DC_LINE: usize,
        const L2_SIZE: usize, const L2_LINE: usize, const MIPS4: bool, const PRID: u32,
        const FIR: u32, const TLB_ENTRIES: usize, const R10K_OPS: bool,
    > MipsCache
    for ShadowCache<IC_SIZE, IC_LINE, DC_SIZE, DC_LINE, L2_SIZE, L2_LINE, MIPS4, PRID, FIR, TLB_ENTRIES, R10K_OPS>
{
    const IC_SIZE: usize = IC_SIZE;
    const IC_LINE: usize = IC_LINE;
    const IC_WAYS: usize = 1;
    const DC_SIZE: usize = DC_SIZE;
    const DC_LINE: usize = DC_LINE;
    const DC_WAYS: usize = 1;
    const L2_SIZE: usize = L2_SIZE;
    const L2_LINE: usize = L2_LINE;

    fn fetch(&self, _virt_addr: u64, phys_addr: u64) -> FetchInstrResult {
        let r = self.downstream.read32(phys_addr as u32);
        if r.is_ok() {
            let slot = unsafe { &mut *self.fetch_scratch.get() };
            slot.flags = FLAG_NOT_DECODED;
            slot.raw = r.data;
            FetchInstrResult::hit(slot as *const DecodedInstr)
        } else {
            FetchInstrResult::exception(r.status)
        }
    }

    fn read<const SIZE: usize>(&self, _virt_addr: u64, phys_addr: u64) -> BusRead64 {
        const {
            assert!(SIZE == 1 || SIZE == 2 || SIZE == 4 || SIZE == 8, "invalid memory access SIZE")
        };
        let a = phys_addr as u32;
        if SIZE == 1 {
            let r = self.downstream.read8(a);
            BusRead64 { status: r.status, data: r.data as u64 }
        } else if SIZE == 2 {
            let r = self.downstream.read16(a);
            BusRead64 { status: r.status, data: r.data as u64 }
        } else if SIZE == 4 {
            let r = self.downstream.read32(a);
            BusRead64 { status: r.status, data: r.data as u64 }
        } else {
            self.downstream.read64(a)
        }
    }

    fn write<const SIZE: usize>(&self, _virt_addr: u64, phys_addr: u64, val: u64) -> u32 {
        const {
            assert!(SIZE == 1 || SIZE == 2 || SIZE == 4 || SIZE == 8, "invalid memory access SIZE")
        };
        let a = phys_addr as u32;
        if SIZE == 1 {
            self.downstream.write8(a, val as u8)
        } else if SIZE == 2 {
            self.downstream.write16(a, val as u16)
        } else if SIZE == 4 {
            self.downstream.write32(a, val as u32)
        } else {
            self.downstream.write64(a, val)
        }
    }

    fn write64_masked(&self, _virt_addr: u64, phys_addr: u64, val: u64, mask: u64) -> u32 {
        let aligned = (phys_addr & !7) as u32;
        let r = self.downstream.read64(aligned);
        if !r.is_ok() {
            return r.status;
        }
        self.downstream.write64(aligned, (r.data & !mask) | (val & mask))
    }

    /// The whole point of the type.
    ///
    /// Every operation that only *moves data between cache and memory* —
    /// invalidate, writeback, fill — is a genuine no-op here, because the
    /// cache and memory can never disagree. The operations that move data
    /// between the cache and a register are the ones with observable effects,
    /// and those are served from the shadow.
    fn cache_op(&self, cache_op: u32, virt_addr: u64, phys_addr: u64) -> u32 {
        let sel = cache_op & 3;
        let op = cache_op & 0x1C;

        if std::env::var_os("IRIS_SHADOW_CACHEOPS").is_some() {
            eprintln!(
                "shadow: {:<22} raw={cache_op:#04x} va={virt_addr:#018x} arg={phys_addr:#018x}",
                cache_op_name(cache_op)
            );
        }

        match op {
            // Index_Store_Tag / Index_Load_Tag. Stored and returned verbatim:
            // a tag test is a round trip, and round trips do not require
            // knowing what the bits mean.
            C_IST => {
                let idx = self.line_index(sel, virt_addr);
                let s = self.shadow(sel);
                if idx < s.tags.len() {
                    s.tags[idx] = phys_addr as u32;
                }
                0
            }
            C_ILT => {
                let idx = self.line_index(sel, virt_addr);
                let s = self.shadow(sel);
                if idx < s.tags.len() { s.tags[idx] } else { 0 }
            }

            // R10000 reassigns 5/6/7, scoped to particular cache selects.
            C_R10K_CBARRIER if R10K_OPS && sel == CACH_PI => 0,
            C_R10K_ILD if R10K_OPS && matches!(sel, CACH_PI | CACH_PD | CACH_SD) => {
                let s = self.shadow(sel);
                let slot = (virt_addr as usize) >> 3;
                if s.data.is_empty() { 0 } else { s.data[slot % s.data.len()] as u32 }
            }
            C_R10K_ISD if R10K_OPS && matches!(sel, CACH_SI | CACH_SD) => {
                let s = self.shadow(sel);
                let slot = (virt_addr as usize) >> 3;
                if !s.data.is_empty() {
                    let n = s.data.len();
                    s.data[slot % n] = phys_addr;
                }
                0
            }

            // Invalidate, writeback, fill, and the R4000 hit operations. All
            // no-ops: there is nothing held that could be stale or dirty.
            _ => 0,
        }
    }

    fn get_config(&self, cache_target: u32) -> (usize, usize) {
        match cache_target {
            CACH_PI => (IC_SIZE, IC_LINE),
            CACH_PD => (DC_SIZE, DC_LINE),
            _ => (L2_SIZE, L2_LINE),
        }
    }

    fn downstream(&self) -> Arc<dyn BusDevice> {
        self.downstream.clone()
    }

    fn check_and_clear_llbit(&self, _phys_addr: u64) {
        unsafe { *self.llbit.get() = false };
    }
    fn get_llbit(&self) -> bool {
        unsafe { *self.llbit.get() }
    }
    fn set_llbit(&self, val: bool) {
        unsafe { *self.llbit.get() = val };
    }
    fn get_lladdr(&self) -> u32 {
        unsafe { *self.lladdr.get() }
    }
    fn set_lladdr(&self, addr: u32) {
        unsafe { *self.lladdr.get() = addr };
    }
}

/// SGI Indigo2 IMPACT R10000 (IP28).
///
/// Real geometry as software reads it — 32 KB primaries with 64-byte
/// instruction lines and 32-byte data lines, a 1 MB secondary with 128-byte
/// lines — 64 TLB entries, MIPS IV, and the R10000 cache operation encodings.
/// Nothing of the microarchitecture: no ways, no LRU, no out-of-order.
pub type R10000ShadowCache =
    ShadowCache<32768, 64, 32768, 32, 1048576, 128, true, 0x0000_0900, 0x0000_0900, 64, true>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Memory;
    use crate::mips_cache_v2::{CACH_PD, CACH_SD};

    fn cache() -> R10000ShadowCache {
        let mem: Arc<dyn BusDevice> = Arc::new(Memory::new(1024 * 1024));
        R10000ShadowCache::from(mem)
    }

    /// A tag test is a round trip, so the shadow must return exactly the bits
    /// it was given — including any the real layout would not use. The cache
    /// model that preceded this one decoded TagLo into ptag/state/pidx fields
    /// and re-encoded on the way out, which silently dropped the low seven
    /// bits and every state code it did not recognise. A walking-1s test then
    /// failed on its very first bit.
    #[test]
    fn tags_round_trip_every_bit() {
        let c = cache();
        for bit in 0..32 {
            let v = 1u32 << bit;
            c.cache_op(C_IST | CACH_SD, 0, v as u64);
            assert_eq!(
                c.cache_op(C_ILT | CACH_SD, 0, 0),
                v,
                "bit {bit} did not survive a store/load tag round trip"
            );
        }
    }

    /// Index_Store_Data / Index_Load_Data likewise.
    #[test]
    fn secondary_data_round_trips() {
        let c = cache();
        c.cache_op(C_R10K_ISD | CACH_SD, 0x40, 0xdeadbeef);
        assert_eq!(c.cache_op(C_R10K_ILD | CACH_SD, 0x40, 0), 0xdeadbeef);
    }

    /// Separate lines must not alias onto one another.
    #[test]
    fn distinct_lines_hold_distinct_tags() {
        let c = cache();
        c.cache_op(C_IST | CACH_SD, 0, 0x1111_1111);
        c.cache_op(C_IST | CACH_SD, 128, 0x2222_2222);
        assert_eq!(c.cache_op(C_ILT | CACH_SD, 0, 0), 0x1111_1111);
        assert_eq!(c.cache_op(C_ILT | CACH_SD, 128, 0), 0x2222_2222);
    }

    /// The way-select bit real hardware keeps at bit 0 is below line
    /// granularity, so it folds away rather than selecting a second array.
    #[test]
    fn the_way_bit_folds_away() {
        let c = cache();
        c.cache_op(C_IST | CACH_SD, 0x100, 0xabcd);
        assert_eq!(c.cache_op(C_ILT | CACH_SD, 0x101, 0), 0xabcd);
    }

    /// Nothing is ever held, so a load must see the store that preceded it
    /// with no flush in between. This is the property that makes the design
    /// safe, not merely fast.
    #[test]
    fn memory_is_the_only_store() {
        let c = cache();
        c.write::<4>(0, 0x2000, 0x1234_5678);
        assert_eq!(c.read::<4>(0, 0x2000).data, 0x1234_5678);
        // An invalidate cannot lose it, and a writeback cannot be needed.
        c.cache_op(crate::mips_cache_v2::C_IINV | CACH_PD, 0x2000, 0);
        assert_eq!(c.read::<4>(0, 0x2000).data, 0x1234_5678);
    }

    /// Geometry is what CP0 Config is built from, so it must be the real
    /// part's even though the model holds nothing.
    #[test]
    fn reported_geometry_is_the_real_parts() {
        let c = cache();
        assert_eq!(c.get_config(CACH_PI), (32768, 64));
        assert_eq!(c.get_config(CACH_PD), (32768, 32));
        assert_eq!(c.get_config(CACH_SD), (1048576, 128));
    }
}

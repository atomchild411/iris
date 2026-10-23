//! The O2's R10000 cache: the full model until the kernel starts, the shadow
//! one after.
//!
//! The O2 PROM sizes memory with its stack in the data cache, before there is
//! any RAM behind it, and only a cache that holds data keeps that stack: the
//! shadow cache (tags only, loads and stores straight to memory) loses it and
//! POST returns to a garbage address. Everything after the PROM is the
//! opposite case. An OS manages the caches itself, and the shadow cache is the
//! model IP28 runs IRIX on: faster, and out of the data path, so a cache
//! modelling mistake cannot corrupt memory.
//!
//! So this holds both and forwards to one. [`HandoverCache::hand_over`]
//! writes every dirty line of the full model back to memory, carries the
//! LL/SC link across and switches to the shadow model for good.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::cpu::mips_cache_shadow::ShadowCache;
use crate::cpu::mips_cache_v2::{
    model, CpuCache, CpuModel, FetchInstrResult, JitDcGeometry, MipsCache,
};
use crate::cpu::mips_isa::{CACH_PD, CACH_PI, CACH_SD, C_IINV, C_IWBINV};
use crate::traits::{BusDevice, BusRead64};

/// SGI O2 R10000 (IP32): the IP28 model with 64-byte secondary-cache lines.
///
/// The O2 PROM (4.18 and earlier) handles only that line size. With Config's
/// SB bit set (128-byte lines, as on an IP28) its sizing code keeps the
/// secondary cache's *size* as the line size, then aligns every DMA buffer to
/// 1 MB, runs out of heap, and panics on the first SCSI command. So O2 R10000
/// modules have SB clear, and so does this model.
pub type R10000O2Cache = CpuCache<32768, 64, 1, 512,
                                  32768, 32, 1, 1024, 4096,
                                  1048576, 64, 16384, 131072, 262144, true,
                                  true, 0x0000_0925, 0x0000_0900, 64, { model::R10000 }>;

/// The IP28 shadow with the same 64-byte secondary lines. Not usable from
/// reset on its own (see the module notes), so [`HandoverCache`] switches to
/// it once a kernel starts.
pub type R10000O2ShadowCache =
    ShadowCache<32768, 64, 32768, 32, 1048576, 64, true, 0x0000_0925, 0x0000_0900, 64, { model::R10000 }>;

pub struct HandoverCache {
    full: R10000O2Cache,
    shadow: R10000O2ShadowCache,
    on_shadow: AtomicBool,
}

impl From<Arc<dyn BusDevice>> for HandoverCache {
    fn from(bus: Arc<dyn BusDevice>) -> Self {
        let full = R10000O2Cache::from(bus.clone());
        // The full model only ever runs the PROM, which keeps its stack in
        // the cache with nothing behind it, so it is never transparent.
        full.set_tcache_live(false);
        Self { full, shadow: R10000O2ShadowCache::from(bus), on_shadow: AtomicBool::new(false) }
    }
}

/// Forward to whichever model is live.
macro_rules! live {
    ($s:expr, $c:ident => $e:expr) => {
        if $s.on_shadow.load(Ordering::Relaxed) {
            let $c = &$s.shadow;
            $e
        } else {
            let $c = &$s.full;
            $e
        }
    };
}

impl HandoverCache {
    /// Whether the shadow model is live.
    pub fn on_shadow(&self) -> bool {
        self.on_shadow.load(Ordering::Relaxed)
    }

    /// Write the full model's dirty lines back, then switch to the shadow
    /// model. Index operations over every set of both ways, as an R10000
    /// kernel flushes: primary data, primary instruction, then secondary.
    pub fn hand_over(&self) {
        if self.on_shadow() {
            return;
        }
        let kseg0 = 0x8000_0000u64;
        let way_size = |size: usize| (size / 2) as u64;
        for va in (0..way_size(<R10000O2Cache as MipsCache>::DC_SIZE)).step_by(<R10000O2Cache as MipsCache>::DC_LINE) {
            for way in 0..2 {
                let a = kseg0 | va | way;
                self.full.cache_op(C_IWBINV | CACH_PD, a, a);
            }
        }
        for va in (0..way_size(<R10000O2Cache as MipsCache>::IC_SIZE)).step_by(<R10000O2Cache as MipsCache>::IC_LINE) {
            for way in 0..2 {
                let a = kseg0 | va | way;
                self.full.cache_op(C_IINV | CACH_PI, a, a);
            }
        }
        for va in (0..way_size(<R10000O2Cache as MipsCache>::L2_SIZE)).step_by(<R10000O2Cache as MipsCache>::L2_LINE) {
            for way in 0..2 {
                let a = kseg0 | va | way;
                self.full.cache_op(C_IWBINV | CACH_SD, a, a);
            }
        }
        self.shadow.set_llbit(self.full.get_llbit());
        self.shadow.set_lladdr(self.full.get_lladdr());
        // Which physical ranges are RAM (the direct-path bitmap): the memory
        // space maintains the full model's; without it the shadow model sent
        // every instruction fetch and helper access the slow way (indirect
        // calls ran at half speed). The bank layout is the PROM's, fixed
        // before the kernel starts.
        unsafe {
            let (from, to) = (self.full.tcache_bitmap_ptr(), self.shadow.tcache_bitmap_ptr());
            if !from.is_null() && !to.is_null() {
                *to = *from;
            }
        }
        self.on_shadow.store(true, Ordering::Relaxed);
    }

    /// Back to the full model, empty, for a PROM starting over (a reboot):
    /// POST needs a cache that holds its stack again.
    pub fn hand_back(&self) {
        if !self.on_shadow() {
            return;
        }
        self.full.power_on();
        self.full.set_llbit(self.shadow.get_llbit());
        self.full.set_lladdr(self.shadow.get_lladdr());
        self.on_shadow.store(false, Ordering::Relaxed);
    }
}

impl CpuModel for HandoverCache {
    const MIPS4: bool = <R10000O2Cache as CpuModel>::MIPS4;
    const PRID: u32 = <R10000O2Cache as CpuModel>::PRID;
    const FIR: u32 = <R10000O2Cache as CpuModel>::FIR;
    const TLB_ENTRIES: usize = <R10000O2Cache as CpuModel>::TLB_ENTRIES;
    const VA_BITS: u32 = <R10000O2Cache as CpuModel>::VA_BITS;
    const NAME: &'static str = <R10000O2Cache as CpuModel>::NAME;
    const R10K_CACHE_OPS: bool = <R10000O2Cache as CpuModel>::R10K_CACHE_OPS;
}

impl MipsCache for HandoverCache {
    const IC_SIZE: usize = <R10000O2Cache as MipsCache>::IC_SIZE;
    const IC_LINE: usize = <R10000O2Cache as MipsCache>::IC_LINE;
    const IC_WAYS: usize = <R10000O2Cache as MipsCache>::IC_WAYS;
    const DC_SIZE: usize = <R10000O2Cache as MipsCache>::DC_SIZE;
    const DC_LINE: usize = <R10000O2Cache as MipsCache>::DC_LINE;
    const DC_WAYS: usize = <R10000O2Cache as MipsCache>::DC_WAYS;
    const L2_SIZE: usize = <R10000O2Cache as MipsCache>::L2_SIZE;
    const L2_LINE: usize = <R10000O2Cache as MipsCache>::L2_LINE;

    fn set_l1i_counters(&mut self, hit: Arc<AtomicU64>, fetch: Arc<AtomicU64>) {
        self.full.set_l1i_counters(hit.clone(), fetch.clone());
        self.shadow.set_l1i_counters(hit, fetch);
    }
    fn fetch(&self, virt_addr: u64, phys_addr: u64) -> FetchInstrResult {
        live!(self, c => c.fetch(virt_addr, phys_addr))
    }
    fn fetch_is_pure(&self) -> bool {
        live!(self, c => c.fetch_is_pure())
    }
    fn read<const SIZE: usize>(&self, virt_addr: u64, phys_addr: u64) -> BusRead64 {
        live!(self, c => c.read::<SIZE>(virt_addr, phys_addr))
    }
    fn write<const SIZE: usize>(&self, virt_addr: u64, phys_addr: u64, val: u64) -> u32 {
        live!(self, c => c.write::<SIZE>(virt_addr, phys_addr, val))
    }
    fn write64_masked(&self, virt_addr: u64, phys_addr: u64, val: u64, mask: u64) -> u32 {
        live!(self, c => c.write64_masked(virt_addr, phys_addr, val, mask))
    }
    fn cache_op(&self, cache_op: u32, virt_addr: u64, phys_addr: u64) -> u64 {
        live!(self, c => c.cache_op(cache_op, virt_addr, phys_addr))
    }
    fn set_cache_ecc(&self, v: u32) {
        live!(self, c => c.set_cache_ecc(v))
    }
    fn set_error_control(&self, ce: bool, detect: bool) {
        live!(self, c => c.set_error_control(ce, detect))
    }
    fn take_cache_err(&self) -> u32 {
        live!(self, c => c.take_cache_err())
    }
    fn take_op_status(&self) -> u32 {
        live!(self, c => c.take_op_status())
    }
    fn cache_op_ecc(&self) -> u32 {
        live!(self, c => c.cache_op_ecc())
    }
    fn writeback(&self, virt_addr: Option<u64>, phys_addr: u64, size: u64) -> usize {
        live!(self, c => c.writeback(virt_addr, phys_addr, size))
    }
    fn get_config(&self, cache_target: u32) -> (usize, usize) {
        live!(self, c => c.get_config(cache_target))
    }
    fn downstream(&self) -> Arc<dyn BusDevice> {
        self.full.downstream()
    }
    fn check_and_clear_llbit(&self, phys_addr: u64) {
        live!(self, c => c.check_and_clear_llbit(phys_addr))
    }
    fn get_llbit(&self) -> bool {
        live!(self, c => c.get_llbit())
    }
    fn set_llbit(&self, val: bool) {
        live!(self, c => c.set_llbit(val))
    }
    fn get_lladdr(&self) -> u32 {
        live!(self, c => c.get_lladdr())
    }
    fn set_lladdr(&self, addr: u32) {
        live!(self, c => c.set_lladdr(addr))
    }
    fn debug_probe(&self, cache_name: &str, virt_addr: u64, phys_addr: u64) -> String {
        live!(self, c => c.debug_probe(cache_name, virt_addr, phys_addr))
    }
    fn debug_dump_line(&self, cache_name: &str, idx: usize) -> String {
        live!(self, c => c.debug_dump_line(cache_name, idx))
    }
    fn set_l2_enabled(&mut self, enabled: bool) {
        self.full.set_l2_enabled(enabled);
        self.shadow.set_l2_enabled(enabled);
    }
    fn power_on(&self) {
        self.full.power_on();
        self.shadow.power_on();
        self.on_shadow.store(false, Ordering::Relaxed);
    }
    // The ppmem windows go to both models; everything the JIT's inline
    // path reads comes from the live one, so code compiled after the
    // handover gets the shadow model's tagless path (window only).
    unsafe fn set_tcache_window(&self, base: *mut u8) {
        unsafe {
            self.full.set_tcache_window(base);
            self.shadow.set_tcache_window(base);
        }
    }
    #[cfg(feature = "jitv2")]
    unsafe fn set_tcache_gen_window(&self, gen_base: *mut AtomicU64) {
        unsafe {
            self.full.set_tcache_gen_window(gen_base);
            self.shadow.set_tcache_gen_window(gen_base);
        }
    }
    /// The full model's: the one the machine registers with the memory
    /// space, which keeps it up to date. `hand_over` copies it across.
    fn tcache_bitmap_ptr(&self) -> *mut u64 {
        self.full.tcache_bitmap_ptr()
    }
    fn tcache_base_ptr(&self) -> *mut u8 {
        live!(self, c => c.tcache_base_ptr())
    }
    #[cfg(feature = "jitv2")]
    fn tcache_gen_ptr(&self) -> *mut u8 {
        live!(self, c => c.tcache_gen_ptr())
    }
    fn jit_fast_read<const SIZE: usize>(&self, virt: u64, phys: u64) -> Option<u64> {
        live!(self, c => c.jit_fast_read::<SIZE>(virt, phys))
    }
    fn jit_fast_write<const SIZE: usize>(&self, virt: u64, phys: u64, val: u64) -> bool {
        live!(self, c => c.jit_fast_write::<SIZE>(virt, phys, val))
    }
    fn jit_l2_tags_ptr(&self) -> *mut u8 {
        live!(self, c => c.jit_l2_tags_ptr())
    }
    fn jit_dc_tags_ptr(&self) -> *mut u8 {
        live!(self, c => c.jit_dc_tags_ptr())
    }
    fn jit_dc_lru_ptr(&self) -> *mut u8 {
        live!(self, c => c.jit_dc_lru_ptr())
    }
    /// The handover is the switch: the full model holds data for the PROM,
    /// the shadow model is transparent for the kernel. Passing the machine's
    /// firmware-entry switch on as well wrote every line back twice at kernel
    /// entry and cleared and rewrote the idle full model on every firmware
    /// callback.
    fn set_tcache_live(&self, _on: bool) {}
    fn jit_dc_geometry(&self) -> JitDcGeometry {
        live!(self, c => c.jit_dc_geometry())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shadow stands in for the full model once the kernel runs, so
    /// everything software can read must be the full model's: the geometry
    /// CP0 Config reports (64-byte secondary lines, the one size the O2 PROM
    /// handles), PRId/FIR, ISA level and TLB size.
    #[test]
    fn the_o2_shadow_matches_the_o2_full_model() {
        type S = R10000O2ShadowCache;
        type F = R10000O2Cache;
        assert_eq!((S::IC_SIZE, S::IC_LINE, S::DC_SIZE, S::DC_LINE, S::L2_SIZE, S::L2_LINE),
                   (F::IC_SIZE, F::IC_LINE, F::DC_SIZE, F::DC_LINE, F::L2_SIZE, F::L2_LINE), "geometry");
        assert_eq!(<F as MipsCache>::L2_LINE, 64);
        assert_eq!((<S as CpuModel>::PRID, <S as CpuModel>::FIR, <S as CpuModel>::MIPS4,
                    <S as CpuModel>::TLB_ENTRIES, <S as CpuModel>::VA_BITS, <S as CpuModel>::R10K_CACHE_OPS),
                   (<F as CpuModel>::PRID, <F as CpuModel>::FIR, <F as CpuModel>::MIPS4,
                    <F as CpuModel>::TLB_ENTRIES, <F as CpuModel>::VA_BITS, <F as CpuModel>::R10K_CACHE_OPS),
                   "identity");
    }
}

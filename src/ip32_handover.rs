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

use crate::cpu::mips_cache_shadow::R10000O2ShadowCache;
use crate::cpu::mips_cache_v2::{
    CpuModel, FetchInstrResult, JitDcGeometry, MipsCache, R10000O2Cache,
};
use crate::cpu::mips_isa::{CACH_PD, CACH_PI, CACH_SD, C_IINV, C_IWBINV};
use crate::traits::{BusDevice, BusRead64};

pub struct HandoverCache {
    full: R10000O2Cache,
    shadow: R10000O2ShadowCache,
    on_shadow: AtomicBool,
}

impl From<Arc<dyn BusDevice>> for HandoverCache {
    fn from(bus: Arc<dyn BusDevice>) -> Self {
        Self {
            full: R10000O2Cache::from(bus.clone()),
            shadow: R10000O2ShadowCache::from(bus),
            on_shadow: AtomicBool::new(false),
        }
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
        self.on_shadow.store(true, Ordering::Relaxed);
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
    fn jit_dc_geometry(&self) -> JitDcGeometry {
        JitDcGeometry::unsupported()
    }
}

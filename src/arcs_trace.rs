//! Observe the guest calling ARCS firmware.
//!
//! Phase 0 of `docs/arcs-scoping.md`: before implementing an ARCS interface we
//! want to know which of its 35 entries a guest actually uses, with what
//! arguments. NetBSD's source answers that for NetBSD. It cannot answer it for
//! IRIX, whose source is not a reference we use — but the machine will answer
//! it directly, because the firmware vector table sits at a known address and
//! every call goes through it.
//!
//! The table is found the way it was found by hand (see
//! `rules/testing/arcs-console-from-bare-metal.md`): a System Parameter Block
//! at physical `0x1000` whose signature is `'ARCS'`, carrying a pointer to the
//! vector at offset `0x20`. Each entry is a 32-bit address of firmware code.
//! So a call is simply a jump whose target is one of those addresses.
//!
//! Arming is lazy and repeated until it succeeds, because the SPB does not
//! exist at reset — the PROM builds it during early startup.

use crate::traits::BusDevice;

/// Physical address of the System Parameter Block.
pub const SPB_PHYS: u32 = 0x0000_1000;
/// `'ARCS'` big-endian, and the byte-swapped spelling some firmware uses.
pub const SPB_SIGNATURE: u32 = 0x5343_5241;
pub const SPB_SIGNATURE_ALT: u32 = 0x4152_4353;
/// Offset of `FirmwareVector` within the SPB.
pub const SPB_FIRMWARE_VECTOR: u32 = 0x20;
/// Offset of `FirmwareVectorLength`, in bytes.
pub const SPB_FIRMWARE_VECTOR_LENGTH: u32 = 0x1c;

/// Entry names by index. SGI publishes 35; the last two exist in the ARC
/// specification and are absent here, which is why a real table measures
/// 0x8c bytes rather than 0x94.
pub const ARCS_NAMES: [&str; 37] = [
    "Load",
    "Invoke",
    "Execute",
    "Halt",
    "PowerDown",
    "Restart",
    "Reboot",
    "EnterInteractiveMode",
    "ReturnFromMain",
    "GetPeer",
    "GetChild",
    "GetParent",
    "GetConfigurationData",
    "AddChild",
    "DeleteComponent",
    "GetComponent",
    "SaveConfiguration",
    "GetSystemId",
    "GetMemoryDescriptor",
    "Signal",
    "GetTime",
    "GetRelativeTime",
    "GetDirectoryEntry",
    "Open",
    "Close",
    "Read",
    "GetReadStatus",
    "Write",
    "Seek",
    "Mount",
    "GetEnvironmentVariable",
    "SetEnvironmentVariable",
    "GetFileInformation",
    "SetFileInformation",
    "FlushAllCaches",
    "TestUnicode",
    "GetDisplayStatus",
];

/// The maximum number of entries we will read, even if the firmware claims
/// more. A wild `FirmwareVectorLength` should not turn into a huge read.
const MAX_ENTRIES: usize = 37;

pub struct ArcsTrace {
    /// Target address of each entry, indexed by entry number. `None` where the
    /// firmware leaves the slot null — SGI does that for `ReturnFromMain` and
    /// `Signal`, which is a useful sanity check that the table is really ARCS.
    entries: Vec<Option<u32>>,
    armed: bool,
    /// Attempts so far, so a guest that never builds an SPB stops costing
    /// a bus read on every jump.
    attempts: u32,
    counts: Vec<u64>,
    /// Log every call as it happens, not just the summary.
    verbose: bool,
    total: u64,
}

impl Default for ArcsTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl ArcsTrace {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            armed: false,
            attempts: 0,
            counts: vec![0; MAX_ENTRIES],
            verbose: !matches!(std::env::var("IRIS_ARCS_TRACE").as_deref(), Ok("summary")),
            total: 0,
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Give up arming after this many failed attempts, so the cost is bounded
    /// on a guest that has no ARCS at all.
    const MAX_ATTEMPTS: u32 = 2000;

    pub fn exhausted(&self) -> bool {
        !self.armed && self.attempts >= Self::MAX_ATTEMPTS
    }

    /// Try to read the vector table. Safe to call repeatedly; does nothing
    /// once armed.
    pub fn try_arm(&mut self, bus: &dyn BusDevice) {
        if self.armed || self.exhausted() {
            return;
        }
        self.attempts += 1;

        let sig = bus.read32(SPB_PHYS);
        if !sig.is_ok() || (sig.data != SPB_SIGNATURE && sig.data != SPB_SIGNATURE_ALT) {
            return;
        }
        let vec_ptr = bus.read32(SPB_PHYS + SPB_FIRMWARE_VECTOR);
        let vec_len = bus.read32(SPB_PHYS + SPB_FIRMWARE_VECTOR_LENGTH);
        if !vec_ptr.is_ok() || !vec_len.is_ok() || vec_ptr.data == 0 {
            return;
        }
        let count = ((vec_len.data / 4) as usize).min(MAX_ENTRIES);
        if count == 0 {
            return;
        }

        // The pointer is a virtual address in an unmapped window; the bus
        // wants the physical one.
        let base = vec_ptr.data & 0x1fff_ffff;
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let e = bus.read32(base + (i as u32) * 4);
            entries.push(if e.is_ok() && e.data != 0 { Some(e.data) } else { None });
        }

        let live = entries.iter().filter(|e| e.is_some()).count();
        eprintln!(
            "arcs: vector table at {:#010x} ({count} entries, {live} populated)",
            vec_ptr.data
        );
        for (i, e) in entries.iter().enumerate() {
            if e.is_none() {
                eprintln!("arcs:   entry {i:2} {:<24} null", ARCS_NAMES[i]);
            }
        }
        self.entries = entries;
        self.armed = true;
    }

    /// Which entry, if any, a jump to `target` enters.
    ///
    /// Compared on the low 29 bits so that a call made through KSEG0 matches a
    /// table built from KSEG1 addresses, or the reverse.
    pub fn entry_for(&self, target: u64) -> Option<usize> {
        if !self.armed {
            return None;
        }
        let t = (target as u32) & 0x1fff_ffff;
        self.entries
            .iter()
            .position(|e| matches!(e, Some(a) if (a & 0x1fff_ffff) == t))
    }

    pub fn note(&mut self, idx: usize, args: [u64; 4], ra: u64) {
        self.counts[idx] += 1;
        self.total += 1;
        if self.verbose {
            eprintln!(
                "arcs: {:<24} a0={:#018x} a1={:#018x} a2={:#018x} a3={:#018x}  ra={:#012x}",
                ARCS_NAMES[idx], args[0], args[1], args[2], args[3], ra
            );
        }
    }

    /// Which entries were used, most-used first. The point of Phase 0.
    pub fn report(&self) -> String {
        let mut out = format!("arcs: {} calls total\n", self.total);
        let mut rows: Vec<(usize, u64)> = self
            .counts
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(i, n)| (i, *n))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        for (i, n) in rows {
            out.push_str(&format!("arcs:   {n:8}  entry {i:2}  {}\n", ARCS_NAMES[i]));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Memory;
    use std::sync::Arc;

    /// Build a fake SPB and vector table in memory, the shape a real one has.
    fn machine_with_arcs(vector_va: u32, entries: &[u32]) -> Arc<Memory> {
        let mem = Arc::new(Memory::new(4));
        mem.write32(SPB_PHYS, SPB_SIGNATURE);
        mem.write32(SPB_PHYS + SPB_FIRMWARE_VECTOR, vector_va);
        mem.write32(
            SPB_PHYS + SPB_FIRMWARE_VECTOR_LENGTH,
            (entries.len() as u32) * 4,
        );
        let base = vector_va & 0x1fff_ffff;
        for (i, e) in entries.iter().enumerate() {
            mem.write32(base + (i as u32) * 4, *e);
        }
        mem
    }

    #[test]
    fn arms_from_a_well_formed_spb_and_finds_entries() {
        let mut ents = [0u32; 35];
        for (i, e) in ents.iter_mut().enumerate() {
            *e = 0x9fc0_0000 + (i as u32) * 0x40;
        }
        let mem = machine_with_arcs(0xa000_1800, &ents);
        let mut t = ArcsTrace::new();
        t.try_arm(mem.as_ref());
        assert!(t.is_armed());
        // Entry 27 is Write, and its name must line up with its index.
        assert_eq!(ARCS_NAMES[27], "Write");
        assert_eq!(t.entry_for(0x9fc0_0000 + 27 * 0x40), Some(27));
    }

    /// A call made through KSEG0 must match a table of KSEG1 addresses. The
    /// harness reaches the firmware through one window and the PROM publishes
    /// the other, so comparing full addresses would silently never match.
    #[test]
    fn a_call_matches_through_either_unmapped_window() {
        let mem = machine_with_arcs(0xa000_1800, &[0x9fc0_1234, 0x9fc0_5678]);
        let mut t = ArcsTrace::new();
        t.try_arm(mem.as_ref());
        // Physical 0x1fc05678 reached three ways: the KSEG0 address the
        // table holds, the KSEG1 alias, and KSEG1 sign-extended into 64 bits
        // as a register actually carries it.
        assert_eq!(t.entry_for(0x9fc0_5678), Some(1), "KSEG0, as published");
        assert_eq!(t.entry_for(0xbfc0_5678), Some(1), "KSEG1 alias");
        assert_eq!(t.entry_for(0xffff_ffff_bfc0_5678), Some(1), "sign-extended");
    }

    /// Null slots are not entries. SGI leaves ReturnFromMain and Signal null,
    /// so a naive table that treated 0 as an address would report every jump
    /// to address 0 as two different ARCS calls at once.
    #[test]
    fn null_slots_are_not_matched() {
        let mem = machine_with_arcs(0xa000_1800, &[0x9fc0_1000, 0, 0x9fc0_3000]);
        let mut t = ArcsTrace::new();
        t.try_arm(mem.as_ref());
        assert_eq!(t.entry_for(0), None);
        assert_eq!(t.entry_for(0x9fc0_3000), Some(2));
    }

    /// Before the PROM builds an SPB there is nothing to arm from, and the
    /// tracer must not claim otherwise or match arbitrary jumps.
    #[test]
    fn does_not_arm_without_a_signature() {
        let mem = Arc::new(Memory::new(4));
        let mut t = ArcsTrace::new();
        t.try_arm(mem.as_ref());
        assert!(!t.is_armed());
        assert_eq!(t.entry_for(0x9fc0_1234), None);
    }

    /// Arming attempts are bounded: a guest with no ARCS at all must stop
    /// costing a bus read on every jump it makes.
    #[test]
    fn arming_gives_up_eventually() {
        let mem = Arc::new(Memory::new(4));
        let mut t = ArcsTrace::new();
        for _ in 0..ArcsTrace::MAX_ATTEMPTS {
            t.try_arm(mem.as_ref());
        }
        assert!(t.exhausted());
    }
}

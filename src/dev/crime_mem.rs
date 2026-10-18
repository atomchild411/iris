//! O2 main memory: the DIMMs that are installed, and where CRIME's bank
//! controls put them.
//!
//! CRIME has eight memory banks, each 32 MB or 128 MB, and a control register
//! per bank that says where in the 1 GB memory window (from
//! [`RAM_BASE`](crate::ip32::RAM_BASE)) that bank answers and how large the
//! controller should take it to be: bits 0-4 are the base in 32 MB units, bit
//! 8 selects 128 MB. Nothing tells the firmware what is fitted; it finds out by
//! programming a bank and looking at what comes back, so the decode has to
//! behave the way the hardware does in the three cases that matter to it:
//!
//! - When more than one control matches an address, the lowest-numbered bank
//!   is the one that answers.
//! - A matching bank with nothing in it does not raise an error. It hands back
//!   whatever was last on the memory bus, which the firmware sets to zero
//!   before it looks.
//! - A bank told it is 128 MB when it is 32 MB ignores the address lines it
//!   does not have, so its 32 MB repeat four times across the range. That
//!   repetition is how the firmware tells the two sizes apart.
//!
//! An address no control matches is a bus error.
//!
//! The DIMMs go in pairs, four pairs (banks A to D in SGI's own manuals, which
//! are not CRIME's banks). A pair of single-sided DIMMs is one CRIME bank, a
//! pair of double-sided ones is two, so pair A is CRIME banks 0 and 1, pair B 2
//! and 3, and so on. SGI sold four kits:
//!
//! | kit    | DIMMs        | sides  | CRIME banks  |
//! |--------|--------------|--------|--------------|
//! | 32 MB  | 2 x 16 MB    | single | one of 32 MB |
//! | 64 MB  | 2 x 32 MB    | double | two of 32 MB |
//! | 128 MB | 2 x 64 MB    | single | one of 128 MB|
//! | 256 MB | 2 x 128 MB   | double | two of 128 MB|
//!
//! and the rules for fitting them: pair A is always populated, both DIMMs of a
//! pair match, pairs are filled in order without gaps, and the largest kit goes
//! in pair A (so no pair holds more than the one before it). That makes 32 MB
//! to 1 GB in 32 MB steps, though not every step: 992 MB would take five kits.
//!
//! The low 256 MB of the window also answers from physical address 0, which is
//! where the firmware and a 32-bit kernel do most of their work.

use crate::ip32::{LOW_ALIAS_SIZE, RAM_BASE, RAM_WINDOW_SIZE};

/// The unit the bank controls count in, and the size of a small bank.
const SLOT: u32 = 32 << 20;
/// The 128 MB bank size; bit 8 of a bank control selects it.
const BIG: u32 = 128 << 20;
const CTRL_128M: u64 = 0x100;
/// The page jitv2 tracks code by.
const PAGE: usize = 4096;
const CTRL_BASE: u64 = 0x1f;
/// The window in 32 MB slots.
const SLOTS: usize = (RAM_WINDOW_SIZE / SLOT) as usize;

/// The kits SGI sold, in MB per pair.
pub const KITS_MB: [u32; 4] = [32, 64, 128, 256];

/// What a 32 MB slot of the window decodes to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Slot {
    /// No bank control matches: a bus error.
    None,
    /// The first match is a bank with nothing fitted.
    Empty,
    /// Memory: `start` is the bank's first byte in `bytes`, `chunk` how far
    /// into the bank this slot begins, before wrapping, and `mask` the bank's
    /// real size less one.
    Ram { start: u32, chunk: u32, mask: u32 },
}

/// Where an address lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decode {
    /// A byte of `bytes`.
    Ram(usize),
    /// A bank control matches but no DIMM is there.
    Empty,
    /// Nothing answers.
    None,
}

pub struct O2Memory {
    /// The fitted banks back to back, in bank order, in guest byte order.
    bytes: Vec<u8>,
    /// Each CRIME bank's real size in bytes (0 for nothing fitted) and where
    /// it starts in `bytes`.
    size: [u32; 8],
    start: [u32; 8],
    /// The bank controls as last written.
    ctrl: [u64; 8],
    slots: [Slot; SLOTS],
    /// One generation counter per 4 KB page of `bytes`, bumped by every write
    /// that goes through [`Self::write`]: what jitv2 checks compiled code
    /// against (`BusDevice::gen_ptr`). Boxed, so a pointer to one stays valid
    /// for as long as the memory does.
    gens: Box<[std::sync::atomic::AtomicU64]>,
}

impl O2Memory {
    /// Memory fitted as `kits`, the size in MB of the kit in each DIMM pair
    /// from pair A.
    pub fn from_kits(kits: &[u32]) -> Result<Self, String> {
        check_kits(kits)?;
        let mut banks = [0u32; 8];
        for (pair, &kit) in kits.iter().enumerate() {
            let (a, b) = match kit {
                32 => (SLOT, 0),
                64 => (SLOT, SLOT),
                128 => (BIG, 0),
                _ => (BIG, BIG),
            };
            banks[2 * pair] = a;
            banks[2 * pair + 1] = b;
        }
        Ok(Self::from_banks(banks))
    }

    /// The usual way to fit `mb`: the fewest kits, largest first.
    pub fn with_total(mb: u32) -> Result<Self, String> {
        Self::from_kits(&kits_for(mb)?)
    }

    /// One bank 0 of `bytes` and nothing else. Not a configuration an O2 can
    /// have unless `bytes` is 32 or 128 MB: it is for device tests that want a
    /// little memory at the bottom of the window. `bytes` must be a power of
    /// two no larger than 128 MB.
    pub fn single(bytes: usize) -> Self {
        let mut banks = [0u32; 8];
        banks[0] = bytes as u32;
        Self::from_banks(banks)
    }

    fn from_banks(size: [u32; 8]) -> Self {
        let mut start = [0u32; 8];
        let mut total = 0u32;
        for i in 0..8 {
            assert!(size[i] == 0 || (size[i].is_power_of_two() && size[i] <= BIG), "bank {i}: {}", size[i]);
            start[i] = total;
            total += size[i];
        }
        let gens = (0..(total as usize).div_ceil(PAGE)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect();
        let mut m = Self { bytes: vec![0u8; total as usize], size, start, ctrl: [0; 8], slots: [Slot::None; SLOTS], gens };
        // At reset every control is zero: all eight banks at the bottom of
        // the window, 32 MB each, so bank 0 answers there.
        m.rebuild();
        m
    }

    /// Write `data` at byte offset `off` (from [`Self::offset`] or
    /// [`Self::dma_offset`]) and bump the generation of every page it touches.
    /// CPU stores and DMA both come here, so compiled code never outlives the
    /// bytes it was compiled from.
    pub fn write(&mut self, off: usize, data: &[u8]) {
        self.bytes[off..off + data.len()].copy_from_slice(data);
        let last = (off + data.len().max(1) - 1) / PAGE;
        for g in &self.gens[off / PAGE..=last] {
            g.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }

    /// The generation counter for the page holding byte offset `off`.
    pub fn gen_ptr(&self, off: usize) -> *const std::sync::atomic::AtomicU64 {
        &self.gens[off / PAGE]
    }

    /// Total memory fitted, in bytes.
    pub fn total(&self) -> u32 {
        self.bytes.len() as u32
    }

    /// Each CRIME bank's real size in bytes.
    pub fn bank_sizes(&self) -> [u32; 8] {
        self.size
    }

    /// Bank `i`'s control register was written.
    pub fn set_ctrl(&mut self, i: usize, val: u64) {
        if self.ctrl[i] != val {
            self.ctrl[i] = val;
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        self.slots = [Slot::None; SLOTS];
        // Highest bank first, so the lowest-numbered match is the one left.
        for i in (0..8).rev() {
            let base = (self.ctrl[i] & CTRL_BASE) as usize;
            let span = if self.ctrl[i] & CTRL_128M != 0 { (BIG / SLOT) as usize } else { 1 };
            for j in 0..span {
                let Some(slot) = self.slots.get_mut(base + j) else { break };
                *slot = if self.size[i] == 0 {
                    Slot::Empty
                } else {
                    Slot::Ram { start: self.start[i], chunk: j as u32 * SLOT, mask: self.size[i] - 1 }
                };
            }
        }
    }

    /// Where a physical address lands: in the window from `RAM_BASE`, or in
    /// its low 256 MB from 0.
    pub fn decode(&self, addr: u32) -> Decode {
        let off = if addr < LOW_ALIAS_SIZE {
            addr
        } else if addr >= RAM_BASE && addr - RAM_BASE < RAM_WINDOW_SIZE {
            addr - RAM_BASE
        } else {
            return Decode::None;
        };
        match self.slots[(off / SLOT) as usize] {
            Slot::None => Decode::None,
            Slot::Empty => Decode::Empty,
            Slot::Ram { start, chunk, mask } => Decode::Ram((start + ((chunk + off % SLOT) & mask)) as usize),
        }
    }

    /// The byte offset of a physical address, if memory is there.
    pub fn offset(&self, addr: u32) -> Option<usize> {
        match self.decode(addr) {
            Decode::Ram(i) => Some(i),
            _ => None,
        }
    }

    /// The same for an address a DMA master (a PCI device, the Ethernet)
    /// hands the memory controller. Below 1 GB a bus address is an offset
    /// into the whole memory window, not just its first 256 MB: the CPU's
    /// low alias stops there because CRIME and MACE sit at 0x14000000 and up,
    /// but nothing on the bus does. OpenBSD programs buffers above 256 MB this
    /// way (0x3fffb000 for the top of a 1 GB machine); from `RAM_BASE` up an
    /// address is read as the CPU reads it, which is how Linux does it.
    pub fn dma_offset(&self, addr: u32) -> Option<usize> {
        if addr < RAM_WINDOW_SIZE {
            return self.offset(RAM_BASE + addr);
        }
        self.offset(addr)
    }
}

/// `bytes` is the memory itself, so a device can index it once it has an
/// offset.
impl std::ops::Deref for O2Memory {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}


/// Device tests set memory up by hand. Everything else writes through
/// [`O2Memory::write`], which keeps the page generations honest; the lib
/// built without `cfg(test)` has no other way in.
#[cfg(test)]
impl std::ops::DerefMut for O2Memory {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

/// Whether `kits` is a fitting SGI's rules allow.
pub fn check_kits(kits: &[u32]) -> Result<(), String> {
    if kits.is_empty() {
        return Err("pair A must be populated".into());
    }
    if kits.len() > 4 {
        return Err(format!("{} kits, but an O2 has four DIMM pairs", kits.len()));
    }
    for (pair, &k) in kits.iter().enumerate() {
        let name = (b'A' + pair as u8) as char;
        if !KITS_MB.contains(&k) {
            return Err(format!("pair {name}: no {k} MB kit (SGI sold 32, 64, 128 and 256 MB)"));
        }
        if pair > 0 && k > kits[pair - 1] {
            return Err(format!("pair {name} ({k} MB) is larger than the pair before it; the largest kit goes in pair A"));
        }
    }
    Ok(())
}

/// The fewest kits that make `mb`, largest first.
pub fn kits_for(mb: u32) -> Result<Vec<u32>, String> {
    if mb == 0 || mb % 32 != 0 || mb > 1024 {
        return Err(format!("{mb} MB: an O2 holds 32 MB to 1 GB in steps of 32 MB"));
    }
    let mut kits = Vec::new();
    let mut left = mb;
    for &k in KITS_MB.iter().rev() {
        while left >= k {
            kits.push(k);
            left -= k;
        }
    }
    if kits.len() > 4 {
        return Err(format!("{mb} MB would take {} kits, but an O2 has four DIMM pairs", kits.len()));
    }
    Ok(kits)
}

/// Every fitting SGI's rules allow, as kit lists from pair A.
pub fn all_fittings() -> Vec<Vec<u32>> {
    let mut out = Vec::new();
    fn walk(prefix: &mut Vec<u32>, out: &mut Vec<Vec<u32>>) {
        if !prefix.is_empty() {
            out.push(prefix.clone());
        }
        if prefix.len() == 4 {
            return;
        }
        for &k in KITS_MB.iter() {
            if prefix.last().is_none_or(|&l| k <= l) {
                prefix.push(k);
                walk(prefix, out);
                prefix.pop();
            }
        }
    }
    walk(&mut Vec::new(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(m: &mut O2Memory, addr: u32, v: u64) {
        let i = m.offset(addr).unwrap();
        m[i..i + 8].copy_from_slice(&v.to_be_bytes());
    }

    fn get(m: &O2Memory, addr: u32) -> Option<u64> {
        match m.decode(addr) {
            Decode::Ram(i) => Some(u64::from_be_bytes(m[i..i + 8].try_into().unwrap())),
            Decode::Empty => Some(0),
            Decode::None => None,
        }
    }

    /// The firmware's own sizing, as SGI's POST does it: every bank from the
    /// one being sized upwards pointed at the probe address as 128 MB, four
    /// patterns written (base, base + 32 MB - 8, base + 32 MB, base + 128 MB -
    /// 8), the bus cleared with zeros, and the four read back. Then the
    /// 128 MB banks are placed from the bottom of the window, the 32 MB ones
    /// after them, and the empty banks given bank 0's control.
    fn size_like_post(m: &mut O2Memory) -> ([u32; 8], u32) {
        let pat = [(0, 0xd0u64), (SLOT - 8, 0xd1), (SLOT, 0xd2), (BIG - 8, 0xd3)];
        let mut found = [0u32; 8];
        for bank in 0..8 {
            let at = bank as u32 * BIG;
            for b in bank..8 {
                m.set_ctrl(b, ((at >> 25) & 0x1f) as u64 | CTRL_128M);
            }
            for &(o, v) in &pat {
                if m.offset(RAM_BASE + at + o).is_some() {
                    put(m, RAM_BASE + at + o, v);
                }
            }
            let r: Vec<_> = pat.iter().map(|&(o, _)| get(m, RAM_BASE + at + o)).collect();
            found[bank] = if r == [Some(0xd2), Some(0xd3), Some(0xd2), Some(0xd3)] {
                32
            } else if r == [Some(0xd0), Some(0xd1), Some(0xd2), Some(0xd3)] {
                128
            } else {
                0
            };
        }
        let mut next = 0u32;
        for size in [128, 32] {
            for bank in 0..8 {
                if found[bank] == size {
                    let flag = if size == 128 { CTRL_128M } else { 0 };
                    m.set_ctrl(bank, ((next >> 25) & 0x1f) as u64 | flag);
                    next += size << 20;
                }
            }
        }
        let c0 = m.ctrl[0];
        for bank in 0..8 {
            if found[bank] == 0 {
                m.set_ctrl(bank, c0);
            }
        }
        (found, next)
    }

    #[test]
    fn there_are_sixty_nine_fittings_from_32_mb_to_1_gb() {
        let all = all_fittings();
        assert_eq!(all.len(), 69, "multisets of 1 to 4 kits from 4 sizes");
        let totals: std::collections::BTreeSet<u32> = all.iter().map(|k| k.iter().sum()).collect();
        assert_eq!(totals.first(), Some(&32));
        assert_eq!(totals.last(), Some(&1024));
        assert!(!totals.contains(&992), "992 MB needs five kits");
        for k in &all {
            assert!(check_kits(k).is_ok(), "{k:?}");
        }
    }

    #[test]
    fn the_fitting_rules_are_sgis() {
        assert!(check_kits(&[]).is_err(), "pair A is never empty");
        assert!(check_kits(&[64, 128]).is_err(), "largest kit first");
        assert!(check_kits(&[48]).is_err(), "no such kit");
        assert!(check_kits(&[32; 5]).is_err(), "four pairs");
        assert_eq!(kits_for(1024).unwrap(), vec![256; 4]);
        assert_eq!(kits_for(224).unwrap(), vec![128, 64, 32]);
        assert!(kits_for(992).is_err());
        assert!(kits_for(100).is_err());
    }

    /// Every fitting an O2 can have sizes to what is fitted, bank by bank,
    /// and once placed, the whole of it is distinct memory from the bottom of
    /// the window up.
    #[test]
    fn every_fitting_sizes_the_way_post_sizes_it() {
        for kits in all_fittings() {
            let mut m = O2Memory::from_kits(&kits).unwrap();
            let (found, total) = size_like_post(&mut m);
            let want: Vec<u32> = m.bank_sizes().iter().map(|s| s >> 20).collect();
            assert_eq!(found.to_vec(), want, "{kits:?}");
            assert_eq!(total, m.total(), "{kits:?}");
            assert_eq!(total >> 20, kits.iter().sum::<u32>(), "{kits:?}");

            // Each 32 MB of the placed window is a different 32 MB of memory.
            let firsts: std::collections::BTreeSet<usize> =
                (0..total / SLOT).map(|s| m.offset(RAM_BASE + s * SLOT).unwrap()).collect();
            assert_eq!(firsts.len() as u32, total / SLOT, "{kits:?}: placed banks overlap");
            assert_eq!(m.decode(RAM_BASE + total), Decode::None, "{kits:?}: nothing above the top");
            // The last byte is there, and the low alias is the window's start.
            assert!(m.offset(RAM_BASE + total - 1).is_some(), "{kits:?}");
            assert_eq!(m.offset(0x1000), m.offset(RAM_BASE + 0x1000));
        }
    }

    #[test]
    fn the_low_alias_stops_at_256_mb() {
        let mut m = O2Memory::with_total(1024).unwrap();
        size_like_post(&mut m);
        assert_eq!(m.offset(LOW_ALIAS_SIZE - 8), m.offset(RAM_BASE + LOW_ALIAS_SIZE - 8));
        assert_eq!(m.decode(LOW_ALIAS_SIZE), Decode::None, "CRIME and MACE live above it");
        assert!(m.offset(RAM_BASE + (1 << 30) - 8).is_some(), "the top of a full machine");
    }

    #[test]
    fn dma_reaches_all_of_memory_below_1_gb() {
        let mut m = O2Memory::with_total(1024).unwrap();
        size_like_post(&mut m);
        let top = (1u32 << 30) - 0x5000;
        assert_eq!(m.dma_offset(top), m.offset(RAM_BASE + top), "OpenBSD's bus address for high memory");
        assert_eq!(m.dma_offset(0x1000), m.offset(0x1000), "and the low 256 MB as before");
        assert_eq!(m.dma_offset(RAM_BASE + 0x2000_0000), m.offset(RAM_BASE + 0x2000_0000), "and the CPU's own addresses");
        assert!(m.offset(0x3fff_b000).is_none(), "which the CPU cannot use: that is not its alias");
    }

    #[test]
    fn a_small_bank_repeats_across_a_large_setting() {
        let mut m = O2Memory::with_total(32).unwrap();
        m.set_ctrl(0, CTRL_128M);
        assert_eq!(m.offset(RAM_BASE + 8), m.offset(RAM_BASE + 3 * SLOT + 8));
    }
}

//! GIO expansion slots: who holds one, and cards supplied from outside.
//!
//! An Indy decodes three GIO apertures — the graphics slot and two expansion
//! slots — and more than one device wants each. The test device
//! (`--test-device`) and the Ultra64 dev board both decode expansion slot 0,
//! and each new claimant so far has meant another pairwise check: with N
//! devices that is N(N-1)/2 of them, each behind its own `#[cfg]`, which is
//! why `check_testdev_slot_free` took a feature-gated argument.
//!
//! [`SlotClaims`] replaces that with one ledger. A device claims its slot as
//! it is built, a second claimant is refused by name, and adding a device
//! means one `claim` call rather than editing every other device's check.
//!
//! [`ExpansionCard`] is the other half: a card the emulator did not build
//! itself. Anything that is a [`BusDevice`] can be installed into a free slot
//! before [`crate::machine::Machine::new`], and the bus maps it like any
//! built-in device. That is what lets a paravirtual device live outside this
//! crate entirely.
//!
//! # Snapshots
//!
//! An installed card is **not** part of a snapshot. Save and restore do not
//! see it, and the determinism validator does not know it is there. A card
//! holding state the guest can observe will therefore not survive a restore.
//! Making that work means a card declaring its own serialised state and the
//! snapshot format carrying it, which is a larger design question than this
//! module answers — so for now a card should either hold no guest-visible
//! state across a snapshot, or the run should not be snapshotted.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::traits::BusDevice;

/// A GIO aperture. Order matches `ioc::GIO_SLOT_BASES`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ExpansionSlot {
    /// The graphics slot, 4 MB at `0x1F000000` (Newport on a stock Indy).
    Graphics,
    /// Expansion slot 0, 2 MB at `0x1F400000`. Empty on a stock Indy.
    Slot0,
    /// Expansion slot 1, 4 MB at `0x1F600000`. Empty on a stock Indy.
    Slot1,
}

impl ExpansionSlot {
    /// First address the slot decodes.
    pub const fn base(self) -> u32 {
        match self {
            Self::Graphics => 0x1F00_0000,
            Self::Slot0 => 0x1F40_0000,
            Self::Slot1 => 0x1F60_0000,
        }
    }

    /// One past the last address the slot decodes.
    pub const fn end(self) -> u32 {
        match self {
            Self::Graphics => 0x1F40_0000,
            Self::Slot0 => 0x1F60_0000,
            Self::Slot1 => 0x1FA0_0000,
        }
    }

    /// How the slot is named in diagnostics, matching IRIX's own convention.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Graphics => "GIO graphics slot",
            Self::Slot0 => "GIO expansion slot 0",
            Self::Slot1 => "GIO expansion slot 1",
        }
    }

    /// Every slot, for iterating.
    pub const ALL: [ExpansionSlot; 3] = [Self::Graphics, Self::Slot0, Self::Slot1];
}

/// Two devices wanted the same slot.
#[derive(Debug)]
pub struct SlotConflict {
    pub slot: ExpansionSlot,
    /// The device already holding it.
    pub held_by: &'static str,
    /// The device that asked for it second.
    pub wanted_by: &'static str,
}

impl std::fmt::Display for SlotConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} and {} both claim {}",
            self.wanted_by,
            self.held_by,
            self.slot.name()
        )
    }
}

/// Which device holds which slot, for one machine.
///
/// Built empty in `Machine::new`; each expansion device claims its slot as it
/// is constructed.
#[derive(Default)]
pub struct SlotClaims {
    held: [Option<&'static str>; 3],
}

impl SlotClaims {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take `slot` for `who`, or report who has it already.
    pub fn claim(&mut self, slot: ExpansionSlot, who: &'static str) -> Result<(), SlotConflict> {
        match self.held[slot as usize] {
            Some(held_by) => Err(SlotConflict { slot, held_by, wanted_by: who }),
            None => {
                self.held[slot as usize] = Some(who);
                Ok(())
            }
        }
    }

    /// What holds `slot`, if anything.
    pub fn holder(&self, slot: ExpansionSlot) -> Option<&'static str> {
        self.held[slot as usize]
    }
}

/// A card in a GIO slot that this crate did not build.
///
/// The bus maps `base()..base() + size()` to it, which must lie inside the
/// slot it was installed into. Addresses reach it as they do any other
/// [`BusDevice`] — absolute, not slot-relative.
pub trait ExpansionCard: BusDevice {
    /// For diagnostics and slot-conflict messages.
    fn name(&self) -> &'static str;
    /// First address this card answers.
    fn base(&self) -> u32;
    /// How many bytes from `base()`. Rounded up to the bus's 64 KB mapping
    /// granule, so a smaller card still takes a whole granule.
    fn size(&self) -> u32;
}

static INSTALLED: Mutex<[Option<Arc<dyn ExpansionCard>>; 3]> = Mutex::new([None, None, None]);

/// Put `card` in `slot`, to be picked up by the next `Machine::new`.
///
/// Fails if the slot already holds an installed card, or if the card decodes
/// outside the slot. A machine already built is unaffected.
pub fn install(slot: ExpansionSlot, card: Arc<dyn ExpansionCard>) -> Result<(), SlotConflict> {
    let (base, size) = (card.base(), card.size());
    assert!(
        base >= slot.base() && size > 0 && base.saturating_add(size) <= slot.end(),
        "{} decodes {:#010x}..{:#010x}, outside {} ({:#010x}..{:#010x})",
        card.name(),
        base,
        base.saturating_add(size),
        slot.name(),
        slot.base(),
        slot.end(),
    );
    let mut installed = INSTALLED.lock();
    if let Some(held) = installed[slot as usize].as_ref() {
        return Err(SlotConflict { slot, held_by: held.name(), wanted_by: card.name() });
    }
    installed[slot as usize] = Some(card);
    Ok(())
}

/// Remove whatever was installed in `slot`. Returns whether there was one.
/// A machine already holding the card keeps it.
pub fn remove(slot: ExpansionSlot) -> bool {
    INSTALLED.lock()[slot as usize].take().is_some()
}

/// The card installed in `slot`, if any.
pub fn installed(slot: ExpansionSlot) -> Option<Arc<dyn ExpansionCard>> {
    INSTALLED.lock()[slot as usize].clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{BusRead32, BusDevice};

    struct Card(&'static str, u32);
    impl BusDevice for Card {
        fn read32(&self, _addr: u32) -> BusRead32 {
            BusRead32::err()
        }
    }
    impl ExpansionCard for Card {
        fn name(&self) -> &'static str {
            self.0
        }
        fn base(&self) -> u32 {
            self.1
        }
        fn size(&self) -> u32 {
            0x10000
        }
    }

    #[test]
    fn a_slot_is_held_by_one_device_and_the_second_is_named() {
        let mut claims = SlotClaims::new();
        assert!(claims.claim(ExpansionSlot::Slot0, "--test-device").is_ok());
        let conflict = claims
            .claim(ExpansionSlot::Slot0, "the ultra64 dev board")
            .expect_err("slot 0 is already held");
        // The message names both, which the pairwise checks could not do
        // without each device knowing about every other.
        assert_eq!(
            conflict.to_string(),
            "the ultra64 dev board and --test-device both claim GIO expansion slot 0"
        );
        assert!(claims.claim(ExpansionSlot::Slot1, "the ultra64 dev board").is_ok());
    }

    #[test]
    fn installing_twice_reports_the_card_already_there() {
        let slot = ExpansionSlot::Slot1;
        remove(slot);
        install(slot, Arc::new(Card("first", slot.base()))).expect("slot was free");
        let conflict = install(slot, Arc::new(Card("second", slot.base())))
            .expect_err("slot is taken");
        assert_eq!(conflict.held_by, "first");
        assert_eq!(conflict.wanted_by, "second");
        assert!(remove(slot));
        assert!(!remove(slot));
    }

    #[test]
    #[should_panic(expected = "outside GIO expansion slot 0")]
    fn a_card_may_not_decode_outside_its_slot() {
        let _ = install(
            ExpansionSlot::Slot0,
            Arc::new(Card("astray", ExpansionSlot::Slot0.end())),
        );
    }
}

//! Deliberate faults, for confirming a contract by watching it break.
//!
//! Every entry here re-introduces a bug that was once real, so that the
//! failure it caused can be produced on demand. A contract that only ever
//! holds is a belief; one you can switch off and watch the guest reject is a
//! *measurement*, and the difference matters most for the contracts that were
//! inferred rather than read off a datasheet.
//!
//! `IRIS_BREAK=<name>[,<name>...]` turns them on. Nothing here is reachable
//! without that variable, and an unrecognised name is fatal — a typo that
//! silently gave you a control run would destroy the whole point of the
//! exercise, which is knowing which of two runs you are looking at.
//!
//! The active set is announced on stderr once, for the same reason.
//!
//! Not everything worth breaking needs an entry: the MC's SYSID revision is
//! already sweepable through `IRIS_IP28_MCREV`, and a knob that exists is a
//! better instrument than one added for the experiment.

use std::sync::OnceLock;

/// Name, and what turning it on does.
pub const FAULTS: &[(&str, &str)] = &[
    (
        "mru-per-way",
        "secondary-cache MRU recorded per way (reported only on the way that \
         was marked) instead of one bit shared by the set",
    ),
    (
        "l2-ecc-not-stored",
        "Index_Load_Data returns check bits of zero instead of the ones \
         Index_Store_Data took from CP0 ECC",
    ),
    (
        "mru-read-at-both",
        "the MRU bit reads back at TagHi[31] *and* TagHi[0], to find out \
         whether the PROM requires the second position to be clear",
    ),
    (
        "mru-read-at-taghi0",
        "the MRU bit reads back at TagHi[0] (bit 32) rather than at TagHi[31] \
         (bit 63), the position it is written at",
    ),
];

fn active() -> &'static [&'static str] {
    static ACTIVE: OnceLock<Vec<&'static str>> = OnceLock::new();
    ACTIVE.get_or_init(|| {
        let raw = match std::env::var("IRIS_BREAK") {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut on: Vec<&'static str> = Vec::new();
        for want in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match FAULTS.iter().find(|(name, _)| *name == want) {
                Some((name, _)) => on.push(name),
                None => {
                    eprintln!("IRIS_BREAK: no such fault {want:?}. Known faults:");
                    for (name, what) in FAULTS {
                        eprintln!("  {name:<20} {what}");
                    }
                    std::process::exit(2);
                }
            }
        }
        if !on.is_empty() {
            eprintln!("=== IRIS_BREAK ACTIVE — this is NOT a normal run ===");
            for name in &on {
                let what = FAULTS.iter().find(|(n, _)| n == name).unwrap().1;
                eprintln!("  {name}: {what}");
            }
            eprintln!("====================================================");
        }
        on
    })
}

/// Parse and announce `IRIS_BREAK` now, at startup.
///
/// Without this the first call to `broken` is what validates the variable,
/// and that call happens deep inside a cache operation the run may never
/// reach — so a misspelled fault name would look exactly like a control run
/// until something happened to consult it. An instrument that can silently
/// become its own control is worse than no instrument.
pub fn init() {
    let _ = active();
}

/// Is this named fault switched on?
pub fn broken(name: &str) -> bool {
    debug_assert!(
        FAULTS.iter().any(|(n, _)| *n == name),
        "asked about an unregistered fault {name:?}",
    );
    active().contains(&name)
}

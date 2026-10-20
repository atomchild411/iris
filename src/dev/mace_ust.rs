//! MACE's UST counter: the O2's guest-visible time base.

use std::sync::Mutex;

use crate::traits::{BusDevice, BusRead32, BusRead64, BUS_OK};

/// MACE's UST/MSC counter: unadjusted system time, free-running.
///
/// Driven by executed instructions, not by reads.
///
/// Advancing on read was enough to stop delay loops hanging, but it makes
/// duration meaningless: a loop that reads twice as often "takes" twice as
/// long. Anything that decodes a *pulse width* — 1-Wire above all — needs
/// elapsed time to mean something, so the harness advances this from its step
/// count and the rate below fixes the relationship.
pub struct MaceUst {
    ticks: Mutex<u64>,
    /// When set, the clock is the host's: microseconds since this instant,
    /// read live. The 1-Wire ID chip's slots are tens of microseconds, so a
    /// clock stepped from another thread cannot time them.
    host: std::sync::OnceLock<std::time::Instant>,
    /// The guest's own view of time: see [`MaceUst::guest_us`].
    guest: Mutex<GuestClock>,
}

/// [`MaceUst::guest_us`]'s state, in host microseconds.
#[derive(Default)]
struct GuestClock {
    /// The host time at the last guest clock read.
    last: u64,
    /// Host time the guest clock has dropped: stalls during 1-Wire work.
    lost: u64,
    /// The 1-Wire bus counts as busy until then.
    onewire_until: u64,
}

/// While the PROM bit-banges 1-Wire, the longest step the guest clock takes
/// between two of its reads. The PROM times each slot by spinning on the
/// clock; a host stall in the middle (a JIT pause, a busy host) would land in
/// the slot. 8 us is far above one spin of the loop and far below the 15 us
/// a 1-Wire "1" may last.
const GUEST_CLOCK_STEP_CAP_US: u64 = 8;

/// How long after the last 1-Wire access the bus counts as busy.
const ONEWIRE_BUSY_US: u64 = 5000;

/// UST ticks per emulated instruction, as a fraction: `TICKS_NUM / TICKS_DEN`.
///
/// The PROM's delay loops are written in microseconds, and its 1-Wire slots
/// are tens of microseconds, so a tick is taken to be 1 µs. At roughly 100
/// emulated instructions per microsecond a bring-up run stays fast while pulse
/// widths keep their proportions.
pub const UST_TICKS_NUM: u64 = 1;

/// Nanoseconds per count of MACE's UST counter.
pub const MACE_UST_PERIOD_NS: u64 = 960;
pub const UST_TICKS_DEN: u64 = 100;

impl Default for MaceUst {
    fn default() -> Self { Self::new() }
}

impl MaceUst {
    pub fn new() -> Self { Self { ticks: Mutex::new(0), host: std::sync::OnceLock::new(), guest: Mutex::new(GuestClock::default()) } }

    /// Run from the host clock from now on (a machine, not the harness).
    pub fn use_host_clock(&self) {
        let _ = self.host.set(std::time::Instant::now());
    }

    /// Set the clock to `us` microseconds, for a machine that keeps time by
    /// the host clock rather than by instructions run.
    pub fn set_us(&self, us: u64) {
        *self.ticks.lock().unwrap() = us;
    }

    /// Advance the clock to match `instructions` executed so far.
    pub fn advance_to(&self, instructions: u64) {
        *self.ticks.lock().unwrap() = instructions * UST_TICKS_NUM / UST_TICKS_DEN;
    }

    /// The counter as MACE presents it: one count per 960 ns (the 33.3 MHz
    /// PCI clock over 32, spec 7.2.2), which IRIX converts back at 960 ns a
    /// count. Counted in microseconds it ran 4% slow against the audio the
    /// codec plays, and programs that time sound by UST drifted.
    pub fn counter(&self) -> u64 {
        self.guest_us() * 1000 / MACE_UST_PERIOD_NS
    }

    /// The time the guest reads (UST, CRIME's timer): the host's, less what
    /// host stalls cost while the PROM was bit-banging 1-Wire. Then each step
    /// between two guest reads counts at most [`GUEST_CLOCK_STEP_CAP_US`],
    /// so after a stall the PROM goes on spinning to its own deadline and
    /// every slot comes out as wide as it meant. Timed on the host clock, a
    /// stall inside a "1" read as a "0" (or a reset), and on some boots the
    /// PROM reported `ds2502_read_rom failed` and IRIX had no Ethernet
    /// address. Capping a device-side clock alone was not enough: a stall
    /// early in a "0" slot let the PROM see its deadline pass and release,
    /// and the slot read short. Outside 1-Wire work nothing is dropped, and
    /// the guest's time never runs backwards.
    pub fn guest_us(&self) -> u64 {
        let now = self.now();
        let mut g = self.guest.lock().unwrap();
        if g.last != 0 && now < g.onewire_until {
            let step = now.saturating_sub(g.last);
            if step > GUEST_CLOCK_STEP_CAP_US {
                g.lost += step - GUEST_CLOCK_STEP_CAP_US;
            }
        }
        g.last = now;
        now - g.lost
    }

    /// The PROM touched the 1-Wire bus: hold stalls out of the guest's clock
    /// for a while.
    pub fn onewire_busy(&self) {
        let now = self.now();
        self.guest.lock().unwrap().onewire_until = now + ONEWIRE_BUSY_US;
    }

    pub fn now(&self) -> u64 {
        match self.host.get() {
            Some(t0) => t0.elapsed().as_micros() as u64,
            None => *self.ticks.lock().unwrap(),
        }
    }
}

impl BusDevice for MaceUst {
    fn read32(&self, addr: u32) -> BusRead32 {
        let t = self.counter();
        // 64-bit counter presented as two 32-bit halves.
        BusRead32::ok(if addr & 4 == 0 { (t >> 32) as u32 } else { t as u32 })
    }
    fn read64(&self, _addr: u32) -> BusRead64 { BusRead64::ok(self.counter()) }
    fn write32(&self, _a: u32, _v: u32) -> u32 { BUS_OK }
    fn write64(&self, _a: u32, _v: u64) -> u32 { BUS_OK }
}

//! MACE's two PS/2 ports, around the shared `Ps2Controller`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use crate::ip32::*;

/// The pair of PS/2 ports, with nothing plugged into either.
///
/// We drive this machine on the serial console, so an empty port is the
/// honest model — but "empty" is not the same as "dead". A real controller
/// still drains its transmit register whether or not a keyboard is listening,
/// and the PROM waits for exactly that. Reporting zero makes every probe take
/// its full 20834-iteration timeout instead of returning at once.
pub struct MacePs2 {
    ports: [Mutex<Ps2Port>; 2],
    /// The keyboard (port 0) and, when `mouse` is set, the mouse (port 1),
    /// as one PS/2 device model. None: nothing plugged in, which is how the
    /// serial-console machines run.
    dev: Option<std::sync::Arc<crate::dev::ps2::Ps2Controller>>,
    mouse: bool,
    /// Keyboard STATUS reads with nothing to read since a byte last arrived:
    /// a guest sitting at a prompt runs this up fast.
    idle_polls: AtomicU32,
    /// The last bytes through either port, `(port, to the device?, byte)`,
    /// for working out a conversation that stalled.
    traffic: Mutex<std::collections::VecDeque<(u8, bool, u8, std::time::Instant)>>,
}

struct Ps2Port {
    control: u64,
    /// The receive latch: it keeps the last byte, which a read returns again
    /// when nothing new has arrived (IRIX reads the ID twice that way).
    rx: u8,
    rx_full: bool,
    /// A byte written to TX that has not gone out yet (TxEN clear).
    tx: Option<u8>,
    /// Bytes the guest has sent, kept so a test can see the transmit path ran.
    sent: Vec<u8>,
}

impl Default for Ps2Port {
    fn default() -> Self {
        Self { control: ps2_ctl::CLKEN, rx: 0, rx_full: false, tx: None, sent: Vec::new() }
    }
}

/// CONTROL bits of a MACE PS/2 port.
pub mod ps2_ctl {
    pub const TX_ENABLE: u64 = 1 << 1;
    pub const TX_INT: u64 = 1 << 2;
    pub const RX_INT: u64 = 1 << 3;
    /// Clock enabled: while clear the device may not send.
    pub const CLKEN: u64 = 1 << 4;
    pub const RESET: u64 = 1 << 5;
}

impl MacePs2 {
    pub fn new() -> Self {
        Self {
            ports: [Mutex::new(Ps2Port::default()), Mutex::new(Ps2Port::default())],
            dev: None,
            mouse: false,
            idle_polls: AtomicU32::new(0),
            traffic: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// With a keyboard on port 0 (and a mouse on port 1 if `mouse`).
    pub fn with_devices(mouse: bool) -> Self {
        let dev = std::sync::Arc::new(crate::dev::ps2::Ps2Controller::new(None));
        crate::traits::Device::start(&*dev);
        Self { dev: Some(dev), mouse, ..Self::new() }
    }

    /// The device model, for the host side to push keys into.
    pub fn devices(&self) -> Option<&std::sync::Arc<crate::dev::ps2::Ps2Controller>> {
        self.dev.as_ref()
    }

    /// The device on port `p`, if one is plugged in.
    fn attached(&self, p: usize) -> Option<&std::sync::Arc<crate::dev::ps2::Ps2Controller>> {
        if p == 1 && !self.mouse { None } else { self.dev.as_ref() }
    }

    /// `(port, register)` for an address in the block, if it names one.
    fn decode(addr: u32) -> (usize, u32) {
        let off = addr - MACE_PS2;
        (((off >> 5) & 1) as usize, (off >> 3) & 3)
    }

    pub fn sent(&self, port: usize) -> Vec<u8> {
        self.ports[port].lock().unwrap().sent.clone()
    }

    /// Move the device's next byte into an empty latch, if the clock lets it.
    fn latch(&self, p: usize, port: &mut Ps2Port) {
        if port.rx_full || port.control & ps2_ctl::CLKEN == 0 || port.control & ps2_ctl::RESET != 0 {
            return;
        }
        if let Some(b) = self.attached(p).and_then(|d| d.pop_from(p == 1)) {
            port.rx = b;
            self.note(p, false, b);
            port.rx_full = true;
        }
    }

    /// Send the pending byte if the transmitter is enabled.
    fn transmit(&self, p: usize, port: &mut Ps2Port) {
        if port.control & ps2_ctl::TX_ENABLE == 0 || port.control & ps2_ctl::RESET != 0 {
            return;
        }
        if let Some(b) = port.tx.take() {
            self.note(p, true, b);
            if let Some(d) = self.attached(p) {
                d.write_to(b, p == 1);
            }
        }
    }

    pub fn read(&self, addr: u32) -> u64 {
        let (p, reg) = Self::decode(addr);
        let mut port = self.ports[p].lock().unwrap();
        self.latch(p, &mut port);
        match reg {
            1 => {
                let v = port.rx as u64;
                port.rx_full = false;
                self.latch(p, &mut port);
                v
            }
            2 => port.control,
            3 => {
                if p == 0 {
                    if port.rx_full {
                        self.idle_polls.store(0, Ordering::Relaxed);
                    } else {
                        self.idle_polls.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let mut st = ps2_bit::CLOCK_SIGNAL;
                if port.control & ps2_ctl::CLKEN != 0 && port.control & ps2_ctl::RESET == 0 {
                    st |= ps2_bit::CLOCK_INHIBIT;
                }
                if port.tx.is_none() {
                    st |= ps2_bit::TX_EMPTY;
                }
                if port.rx_full {
                    st |= ps2_bit::RX_FULL;
                }
                st
            }
            _ => 0,
        }
    }

    pub fn write(&self, addr: u32, val: u64) {
        let (p, reg) = Self::decode(addr);
        let mut port = self.ports[p].lock().unwrap();
        match reg {
            0 => {
                if port.sent.len() < 64 {
                    port.sent.push(val as u8);
                }
                if port.control & ps2_ctl::RESET == 0 {
                    port.tx = Some(val as u8);
                    self.transmit(p, &mut port);
                }
            }
            2 => {
                port.control = val & 0x3f;
                if port.control & ps2_ctl::RESET != 0 {
                    port.tx = None;
                    port.rx_full = false;
                }
                self.transmit(p, &mut port);
                self.latch(p, &mut port);
            }
            _ => {}
        }
    }

    fn note(&self, p: usize, out: bool, b: u8) {
        let mut t = self.traffic.lock().unwrap();
        if t.len() >= 4000 {
            t.pop_front();
        }
        t.push_back((p as u8, out, b, std::time::Instant::now()));
    }

    /// The recent traffic as text: `>` to a device, `<` from it, per port.
    pub fn traffic(&self) -> String {
        self.traffic
            .lock()
            .unwrap()
            .iter()
            .map(|&(p, out, b, _)| format!("{p}{}{b:02x}", if out { '>' } else { '<' }))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The recent traffic one byte per line, with host milliseconds since
    /// the first byte kept: to tell a periodic exchange from a reaction.
    pub fn traffic_timed(&self) -> String {
        let t = self.traffic.lock().unwrap();
        let Some(&(_, _, _, t0)) = t.front() else { return String::new() };
        t.iter()
            .map(|&(p, out, b, at)| {
                format!("{:9.1} {p}{}{b:02x}\n", at.duration_since(t0).as_secs_f64() * 1000.0, if out { '>' } else { '<' })
            })
            .collect()
    }

    /// Keyboard status reads with nothing waiting, since the last byte.
    pub fn idle_polls(&self) -> u32 {
        self.idle_polls.load(Ordering::Relaxed)
    }

    pub fn reset_idle(&self) {
        self.idle_polls.store(0, Ordering::Relaxed);
    }

    /// The PS/2 bits of MACE's ISA interrupt status: 9 and 11 are the ports'
    /// interrupt requests (receive full with RxIEN, transmit empty with
    /// TxIEN), 10 and 12 their unmasked receive-full flags.
    pub fn isa_bits(&self) -> u64 {
        let mut bits = 0;
        for p in 0..2 {
            let mut port = self.ports[p].lock().unwrap();
            self.latch(p, &mut port);
            let irq = (port.control & ps2_ctl::RX_INT != 0 && port.rx_full)
                || (port.control & ps2_ctl::TX_INT != 0 && port.tx.is_none());
            if irq {
                bits |= 1 << (9 + 2 * p);
            }
            if port.rx_full {
                bits |= 1 << (10 + 2 * p);
            }
        }
        bits
    }
}

impl Default for MacePs2 {
    fn default() -> Self {
        Self::new()
    }
}

//! The O2's 1-Wire bus and its DS2502 Ethernet-address chip.

use std::sync::Mutex;


/// The Dallas 1-Wire identity chip: 64 bits of ROM holding a family code, a
/// 48-bit serial number and a CRC. SGI machines keep the Ethernet address here.
///
/// The bus is one wire, so everything is encoded in how long the master holds
/// it low. Durations are read from the UST clock, which is why that had to
/// become time-based first.
pub struct OneWireId {
    rom: [u8; 8],
    /// The EPROM data area. Erased cells read as 0xff on a real part, so an
    /// unprogrammed chip is all-ones rather than all-zeros — a distinction
    /// firmware notices.
    data: Vec<u8>,
    /// The status/redirection bytes, likewise erased.
    status: Vec<u8>,
    st: Mutex<OneWireState>,
}

/// What the device is doing between slots. It decides how the next slot is
/// read: while the master still has things to say a slot is a write and is
/// classified by width, and once the device is answering every slot is a read.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum OwPhase {
    /// Waiting for a command byte.
    #[default]
    Idle,
    /// Collecting the address bytes a memory command needs.
    WantAddr,
    /// Streaming a reply.
    Sending,
}

#[derive(Default)]
struct OneWireState {
    /// Time the master last pulled the line low.
    low_since: Option<u64>,
    /// Bits received from the master in the current command byte.
    in_bits: u8,
    in_count: u8,
    /// Command being executed, once a whole byte has arrived.
    command: Option<u8>,
    /// Bit position within [`OneWireState::out`].
    out_pos: usize,
    phase: OwPhase,
    /// The memory command waiting for its address bytes.
    pending: u8,
    addr: [u8; 2],
    addr_count: usize,
    /// The bytes the device is sending, LSB first within each.
    out: Vec<u8>,
    /// True when the reply has a definite end after which the device listens
    /// for another command — READ ROM does, a memory read does not.
    out_then_idle: bool,
    /// Decoded protocol events, for the bring-up trace.
    log: Vec<String>,
    /// True once the master has opened a read slot, so a bit is on the line.
    presenting: bool,
    /// The presence pulse, as a window in UST time. Equal bounds mean none.
    presence_from: u64,
    presence_to: u64,
}

pub mod onewire_cmd {
    pub const READ_ROM: u8 = 0x33;
    pub const SKIP_ROM: u8 = 0xcc;
    /// DS2502 memory read: two address bytes follow, then the device sends a
    /// CRC of what it was told and streams data from that address.
    pub const READ_MEMORY: u8 = 0xf0;
    /// As above, but the device also emits a CRC after each page.
    pub const READ_DATA_CRC: u8 = 0xc3;
    /// DS2502 status (the redirection/write-protect bytes), same shape.
    pub const READ_STATUS: u8 = 0xaa;
}

/// Microsecond thresholds. A reset is held for at least 480 µs; within a time
/// slot, a short low is a 1 and a long low is a 0.
/// After the master releases a reset, the device waits this long and then
/// pulls the line low for [`OW_PRESENCE_LEN_US`]. The master samples somewhere
/// in that window; holding the pulse for a fixed time rather than "until
/// somebody looks" is what makes it independent of how often it polls.
/// The part SGI fits, and the one the firmware insists on: a DS2502, family
/// code 0x09. Getting this wrong is not subtle but it is quiet — the firmware
/// reads the first eight bits of the ROM, sees a family it does not recognise,
/// abandons the read and reports `ds2502_read_rom failed`, with the other
/// fifty-six bits never requested. A DS1990A's 0x01 is the obvious wrong
/// answer because it is the one every 1-Wire example uses.
pub const OW_FAMILY_DS2502: u8 = 0x09;

pub const OW_PRESENCE_DELAY_US: u64 = 20;
pub const OW_PRESENCE_LEN_US: u64 = 150;

/// Thresholds, and a lesson about calibration.
///
/// These were first set from the widths the PROM produced, which were
/// `32 / 357 / 1980` microseconds — four to six times the 1-Wire spec's
/// `6 / 60 / 480`. That looked like the PROM simply bit-banging slowly, and
/// the thresholds were moved out to match. They were in fact compensating for
/// CRIME's counter, which advanced once per *read* instead of with time, so
/// every delay the firmware asked for came out stretched.
///
/// With [`CRIME_TICKS_PER_US`] driving that counter from the same clock as
/// the UST, the measured widths are `8 / 90 / 500` and the datasheet numbers
/// work unmodified. A device model that needs constants tuned away from the
/// specification is usually telling you something about the clock, not about
/// the device.
pub const OW_RESET_US: u64 = 300;
pub const OW_WRITE0_US: u64 = 30;

impl OneWireId {
    /// Build a ROM from a MAC address: the family code, the six address bytes
    /// as the serial number, and a CRC-8 over the first seven.
    pub fn from_mac(mac: [u8; 6]) -> Self {
        let mut rom = [0u8; 8];
        rom[0] = OW_FAMILY_DS2502;
        rom[1..7].copy_from_slice(&mac);
        rom[7] = crc8_dallas(&rom[..7]);
        // The firmware reads six bytes from offset 0 and reverses them to
        // form the address, so store it backwards.
        let mut data = vec![0xff; Self::DATA_LEN];
        for (i, b) in mac.iter().rev().enumerate() {
            data[i] = *b;
        }
        Self {
            rom,
            data,
            status: vec![0xff; Self::STATUS_LEN],
            st: Mutex::new(Default::default()),
        }
    }

    /// 1 kbit of EPROM, in four 32-byte pages.
    pub const DATA_LEN: usize = 128;
    pub const STATUS_LEN: usize = 8;

    pub fn rom(&self) -> [u8; 8] {
        self.rom
    }

    /// Program the EPROM data area.
    pub fn set_data(&mut self, at: usize, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            if at + i < self.data.len() {
                self.data[at + i] = *b;
            }
        }
    }

    /// The bit the device is currently presenting, LSB first within each byte.
    /// Past the end of the reply the line simply floats high, which is what a
    /// real part does.
    fn reply_bit(&self, st: &OneWireState) -> bool {
        let i = st.out_pos;
        match st.out.get(i / 8) {
            Some(b) => (b >> (i % 8)) & 1 != 0,
            None => true,
        }
    }

    /// A whole byte arrived from the master.
    fn take_byte(&self, st: &mut OneWireState, b: u8) {
        match st.phase {
            OwPhase::Idle => {
                st.command = Some(b);
                self.note(st, format!("cmd 0x{b:02x}"));
                match b {
                    onewire_cmd::READ_ROM => {
                        st.out = self.rom.to_vec();
                        st.out_pos = 0;
                        // Exactly 64 bits, and then the master may issue a
                        // memory command without an intervening reset.
                        st.out_then_idle = true;
                        st.phase = OwPhase::Sending;
                    }
                    onewire_cmd::READ_MEMORY
                    | onewire_cmd::READ_DATA_CRC
                    | onewire_cmd::READ_STATUS => {
                        st.pending = b;
                        st.addr_count = 0;
                        st.phase = OwPhase::WantAddr;
                    }
                    // SKIP_ROM and anything else: nothing to say.
                    _ => st.phase = OwPhase::Idle,
                }
            }
            OwPhase::WantAddr => {
                if st.addr_count < 2 {
                    st.addr[st.addr_count] = b;
                    st.addr_count += 1;
                }
                if st.addr_count == 2 {
                    let a = u16::from_le_bytes(st.addr) as usize;
                    // The device answers with a CRC over everything it was
                    // told, then streams memory from that address.
                    // The firmware reads exactly 1 + (len - addr) + 1 bytes:
                    // a CRC over what it just sent, the rest of the memory
                    // from that address, and a CRC over that data. Omitting
                    // the trailing CRC costs nothing visible — the master
                    // simply reads one byte past the end, gets the floating
                    // 0xff, and rejects the whole transfer.
                    let mut out = vec![crc8_dallas(&[st.pending, st.addr[0], st.addr[1]])];
                    let src: &[u8] = if st.pending == onewire_cmd::READ_STATUS {
                        &self.status
                    } else {
                        &self.data
                    };
                    let body: Vec<u8> = src.iter().skip(a).copied().collect();
                    let body_crc = crc8_dallas(&body);
                    out.extend(body);
                    out.push(body_crc);
                    self.note(st, format!("{} from 0x{a:04x}",
                        if st.pending == onewire_cmd::READ_STATUS { "status" } else { "memory" }));
                    st.out = out;
                    st.out_pos = 0;
                    // A memory read streams until the master resets.
                    st.out_then_idle = false;
                    st.phase = OwPhase::Sending;
                }
            }
            // Once the device is talking, the master listens.
            OwPhase::Sending => {}
        }
    }

    fn note(&self, st: &mut OneWireState, what: String) {
        if st.log.len() < 64 {
            st.log.push(what);
        }
    }

    /// The protocol the device saw, decoded — resets, commands, addresses.
    pub fn protocol_log(&self) -> Vec<String> {
        self.st.lock().unwrap().log.clone()
    }

    /// The master changed the line. `low` is true when it is pulling low.
    ///
    /// Everything is decided by how long the master held the line down, which
    /// is why the UST has to be a real clock: a reset, a write-0, a write-1 and
    /// a read slot differ only in duration.
    pub fn master_drive(&self, now: u64, low: bool) {
        let mut st = self.st.lock().unwrap();
        if low {
            if st.low_since.is_none() {
                // Falling edge. It opens the next slot, so the bit the last
                // slot presented is finished with now — advance past it here
                // rather than on release, where the master has not yet had a
                // chance to sample it.
                if st.phase == OwPhase::Sending && st.presenting {
                    st.out_pos += 1;
                    st.presenting = false;
                    // A finite reply ends, and the device goes back to
                    // listening. Without this the master's next command is
                    // read as more read slots and silently lost.
                    if st.out_then_idle && st.out_pos >= st.out.len() * 8 {
                        st.phase = OwPhase::Idle;
                        st.in_bits = 0;
                        st.in_count = 0;
                    }
                }
                st.low_since = Some(now);
            }
            return;
        }
        // Rising edge: classify by how long it was held.
        let Some(since) = st.low_since.take() else { return };
        let held = now.saturating_sub(since);
        if held >= OW_RESET_US {
            // How much of the reply the master actually took before giving up
            // is the difference between "it hated the CRC" and "it hated the
            // contents".
            if st.phase == OwPhase::Sending {
                let (got, total) = (st.out_pos, st.out.len() * 8);
                self.note(&mut st, format!("master took {got} of {total} bits"));
            }
            let log = std::mem::take(&mut st.log);
            *st = OneWireState {
                presence_from: now + OW_PRESENCE_DELAY_US,
                presence_to: now + OW_PRESENCE_DELAY_US + OW_PRESENCE_LEN_US,
                log,
                ..Default::default()
            };
            self.note(&mut st, "reset".into());
            return;
        }
        if st.phase == OwPhase::Sending {
            // A read slot: the master pulsed briefly, and for the rest of the
            // slot the device owns the line.
            st.presenting = true;
            return;
        }
        // A write slot, LSB first.
        let bit = held < OW_WRITE0_US;
        st.in_bits |= (bit as u8) << st.in_count;
        st.in_count += 1;
        if st.in_count == 8 {
            let b = st.in_bits;
            st.in_bits = 0;
            st.in_count = 0;
            self.take_byte(&mut st, b);
        }
    }

    /// What the master sees on the line: true is high (idle), false is low.
    pub fn line_level(&self, now: u64) -> bool {
        let st = self.st.lock().unwrap();
        if now >= st.presence_from && now < st.presence_to {
            return false;
        }
        if st.low_since.is_some() {
            // The master is holding it down itself.
            return false;
        }
        if st.phase == OwPhase::Sending && st.presenting {
            return self.reply_bit(&st);
        }
        true
    }
}

/// Dallas/Maxim CRC-8, polynomial x^8 + x^5 + x^4 + 1 reflected (0x8c).
pub fn crc8_dallas(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        let mut byte = b;
        for _ in 0..8 {
            let mix = (crc ^ byte) & 1;
            crc >>= 1;
            if mix != 0 {
                crc ^= 0x8c;
            }
            byte >>= 1;
        }
    }
    crc
}

/// Records what the PROM does to the 1-Wire register and when, so the protocol
/// can be reconstructed from its own behaviour rather than guessed at.
pub struct NicTrace {
    /// (host us, guest-clock us, write, value)
    events: Mutex<Vec<(u64, u64, bool, u8)>>,
}

impl Default for NicTrace {
    fn default() -> Self { Self::new() }
}

impl NicTrace {
    pub fn new() -> Self { Self { events: Mutex::new(Vec::new()) } }
    pub fn record(&self, t: u64, guest: u64, write: bool, val: u8) {
        let mut e = self.events.lock().unwrap();
        if e.len() < 65536 {
            e.push((t, guest, write, val));
        }
    }
    pub fn events(&self) -> Vec<(u64, bool, u8)> {
        self.events.lock().unwrap().iter().map(|&(t, _, w, v)| (t, w, v)).collect()
    }
    /// One line per access: host us, guest-clock us, R/W, value.
    pub fn text(&self) -> String {
        self.events.lock().unwrap().iter().map(|&(t, g, w, v)| format!("{t} {g} {} {v:02x}\n", if w { 'W' } else { 'R' })).collect()
    }
}

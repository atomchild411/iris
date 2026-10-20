//! The O2's two 16550 UARTs in MACE's ISA space.

use std::sync::Mutex;

use crate::traits::{BusDevice, BusRead8, BusRead16, BusRead32, BusRead64, BUS_OK};
use crate::ip32::*;

/// A 16550 with just enough behaviour to be written to.
///
/// Register `n` lives at `base + (n << 8) + 7`: MACE spaces its ISA registers
/// 256 bytes apart and puts the byte in the last lane of the 64-bit slot
/// (`sgimips/bus.c`: `h + (o << 8) + 7`). Getting this wrong is silent — every
/// register reads back zero and the firmware simply never transmits.
///
/// Transmit only. The line status register always reports the holding register
/// empty, so the PROM never waits on us, and bytes are appended to a buffer
/// rather than going anywhere.
pub struct Com16550 {
    out: Mutex<Vec<u8>>,
    /// A live copy of everything transmitted, flushed as it arrives.
    /// A guest that takes half an hour to boot cannot be watched through a
    /// dump printed at the end of the run.
    log: Mutex<Option<std::fs::File>>,
    /// Characters waiting to be read by the guest. The PROM polls LSR for
    /// data-ready and then reads the receive register, so an empty queue simply
    /// means "nobody has typed anything".
    input: std::sync::Arc<Mutex<std::collections::VecDeque<u8>>>,
    regs: Mutex<[u8; 8]>,
    /// The client of a TCP console (`serve_tcp`), which gets every byte
    /// transmitted and whose keystrokes are queued as input.
    /// Every connected console client: each sees all output and may type.
    tcp: std::sync::Arc<Mutex<Vec<std::net::TcpStream>>>,
}

pub mod com_reg {
    /// Transmit holding / receive buffer (and divisor low when DLAB is set).
    pub const THR: u32 = 0;
    pub const IER: u32 = 1;
    pub const IIR_FCR: u32 = 2;
    pub const LCR: u32 = 3;
    pub const MCR: u32 = 4;
    pub const LSR: u32 = 5;
    pub const MSR: u32 = 6;
    pub const SCR: u32 = 7;

    /// IER: interrupt on received data, and on the transmitter draining.
    pub const IER_RX: u8 = 0x01;
    pub const IER_TX: u8 = 0x02;
    /// IIR: bit 0 clear means "something to service"; the cause is above it.
    pub const IIR_NONE: u8 = 0x01;
    pub const IIR_TX_EMPTY: u8 = 0x02;
    pub const IIR_RX_DATA: u8 = 0x04;
    /// The two high bits a 16550 sets once its FIFOs are enabled.
    pub const IIR_FIFO: u8 = 0xc0;

    /// LCR bit 7: the next accesses to 0 and 1 are the baud divisor.
    pub const LCR_DLAB: u8 = 0x80;
    /// LSR: a received character is waiting.
    pub const LSR_DR: u8 = 0x01;
    /// LSR: transmit holding register empty.
    pub const LSR_THRE: u8 = 0x20;
    /// LSR: transmitter completely empty.
    pub const LSR_TEMT: u8 = 0x40;

    /// MCR bit 4: loopback. The transmitter is wired back to the receiver and
    /// the modem control outputs to the modem status inputs. post1's serial
    /// self-test runs entirely in this mode, so a UART without it fails POST.
    pub const MCR_LOOP: u8 = 0x10;
    /// MCR outputs that loop back to MSR while `MCR_LOOP` is set.
    pub const MCR_DTR: u8 = 0x01;
    pub const MCR_RTS: u8 = 0x02;
    pub const MCR_OUT1: u8 = 0x04;
    pub const MCR_OUT2: u8 = 0x08;
    /// MSR inputs, in the order the loopback wires them.
    pub const MSR_CTS: u8 = 0x10;
    pub const MSR_DSR: u8 = 0x20;
    pub const MSR_RI: u8 = 0x40;
    pub const MSR_DCD: u8 = 0x80;
}

impl Default for Com16550 {
    fn default() -> Self { Self::new() }
}

impl Com16550 {
    pub fn new() -> Self {
        Self {
            out: Mutex::new(Vec::new()),
            log: Mutex::new(None),
            input: Default::default(),
            regs: Mutex::new([0u8; 8]),
            tcp: Default::default(),
        }
    }

    /// A live console on `127.0.0.1:port`, as IRIS's other machines have on
    /// their serial ports: one client at a time (a new one replaces it), in
    /// telnet character mode -- the same handshake and filter the Z85C30's
    /// TCP backend uses, so `telnet` and `nc` both work. Everything the guest
    /// transmits from now on goes to the client too; what the client types is
    /// queued for the guest, as `feed` queues a script's input.
    /// Port 0 takes any free port; the one bound is returned.
    pub fn serve_tcp(&self, port: u16) -> std::io::Result<u16> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
        let bound = listener.local_addr()?.port();
        let input = self.input.clone();
        let client = self.tcp.clone();
        std::thread::Builder::new().name(format!("ip32-com0-tcp-{port}")).spawn(move || {
            // One thread per client, so a person and a script can share the
            // console: both see everything, and whatever either types goes in.
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_nodelay(true);
                if stream.write_all(&crate::net::telnet::TelnetFilter::initial_handshake()).is_err() {
                    continue;
                }
                let Ok(writer) = stream.try_clone() else { continue };
                client.lock().unwrap().push(writer);
                let input = input.clone();
                let _ = std::thread::Builder::new().name(format!("ip32-com0-client-{port}")).spawn(move || {
                    let mut telnet = crate::net::telnet::TelnetFilter::new();
                    let mut buf = [0u8; 256];
                    loop {
                        let n = match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        let mut reply = Vec::new();
                        let mut typed = Vec::new();
                        for &b in &buf[..n] {
                            if let Some(d) = telnet.feed(b, &mut reply) {
                                typed.push(d);
                            }
                        }
                        if !reply.is_empty() && stream.write_all(&reply).is_err() {
                            break;
                        }
                        input.lock().unwrap().extend(typed);
                    }
                    // A client that has gone is dropped on the next write
                    // that fails.
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                });
            }
        })?;
        Ok(bound)
    }

    /// Mirror everything transmitted to a file as it happens.
    pub fn log_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        *self.log.lock().unwrap() = Some(std::fs::File::create(path)?);
        Ok(())
    }

    /// Queue characters for the guest to read.
    pub fn feed(&self, bytes: &[u8]) {
        self.input.lock().unwrap().extend(bytes.iter().copied());
    }

    /// Characters still unread by the guest.
    pub fn pending_input(&self) -> usize {
        self.input.lock().unwrap().len()
    }

    /// Everything written to the transmit register so far.
    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).to_string()
    }

    pub fn bytes_out(&self) -> usize {
        self.out.lock().unwrap().len()
    }

    /// The last `n` bytes written, without copying the whole history.
    pub fn output_tail(&self, n: usize) -> String {
        let o = self.out.lock().unwrap();
        String::from_utf8_lossy(&o[o.len().saturating_sub(n)..]).to_string()
    }

    /// Decode a MACE ISA address into a 16550 register number, if it names one.
    fn reg_of(addr: u32) -> Option<u32> {
        // Only the +7 lane carries the byte.
        if addr & 7 != 7 {
            return None;
        }
        let n = (addr >> 8) & 7;
        Some(n)
    }

    fn dlab(&self) -> bool {
        self.regs.lock().unwrap()[com_reg::LCR as usize] & com_reg::LCR_DLAB != 0
    }

    /// True while MCR selects internal loopback.
    fn looped(&self) -> bool {
        self.regs.lock().unwrap()[com_reg::MCR as usize] & com_reg::MCR_LOOP != 0
    }

    /// What the chip wants serviced, in the form the driver reads it.
    ///
    /// A kernel does not poll: it fills the transmit FIFO and waits to be
    /// told it has drained. Reporting "nothing pending" here leaves it
    /// waiting forever, and the visible symptom is output that stops in the
    /// middle of a word.
    fn iir(&self) -> u8 {
        let ier = self.regs.lock().unwrap()[com_reg::IER as usize];
        if ier & com_reg::IER_RX != 0 && !self.input.lock().unwrap().is_empty() {
            return com_reg::IIR_RX_DATA;
        }
        if ier & com_reg::IER_TX != 0 {
            // The transmitter is always drained here; there is no wire.
            return com_reg::IIR_TX_EMPTY;
        }
        com_reg::IIR_NONE
    }

    /// True while the chip is asking for attention.
    pub fn interrupting(&self) -> bool {
        self.iir() & com_reg::IIR_NONE == 0
    }

    pub fn read_reg(&self, addr: u32) -> u8 {
        let Some(n) = Self::reg_of(addr) else { return 0 };
        match n {
            // Reading IIR is how the driver asks what to do next, and on a
            // 16550 it also acknowledges a transmit interrupt.
            com_reg::IIR_FCR => {
                let fifo = if self.regs.lock().unwrap()[com_reg::IIR_FCR as usize] & 1 != 0 {
                    com_reg::IIR_FIFO
                } else {
                    0
                };
                self.iir() | fifo
            }
            // Always ready to take another byte, and data-ready whenever
            // something has been typed.
            com_reg::LSR => {
                let mut v = com_reg::LSR_THRE | com_reg::LSR_TEMT;
                if !self.input.lock().unwrap().is_empty() {
                    v |= com_reg::LSR_DR;
                }
                v
            }
            // Register 0 reads the receive buffer (unless DLAB selects the
            // divisor, which nothing reads back).
            com_reg::THR if !self.dlab() => {
                self.input.lock().unwrap().pop_front().unwrap_or(0)
            }
            // In loopback the modem control outputs appear as the status
            // inputs. Nothing in POST depends on this, but a half-wired
            // loopback is the kind of thing that misleads the next reader.
            com_reg::MSR if self.looped() => {
                let mcr = self.regs.lock().unwrap()[com_reg::MCR as usize];
                let mut v = 0;
                for (out, inp) in [
                    (com_reg::MCR_RTS, com_reg::MSR_CTS),
                    (com_reg::MCR_DTR, com_reg::MSR_DSR),
                    (com_reg::MCR_OUT1, com_reg::MSR_RI),
                    (com_reg::MCR_OUT2, com_reg::MSR_DCD),
                ] {
                    if mcr & out != 0 {
                        v |= inp;
                    }
                }
                v
            }
            _ => self.regs.lock().unwrap()[n as usize],
        }
    }

    pub fn write_reg(&self, addr: u32, val: u8) {
        let Some(n) = Self::reg_of(addr) else { return };
        if n == com_reg::THR && !self.dlab() {
            self.transmit(val);
            return;
        }
        self.regs.lock().unwrap()[n as usize] = val;
    }

    /// Send one character, as a write of the transmit register does. MACE's
    /// serial DMA engine comes here too, so a byte sent from the ring reaches
    /// the log and the TCP console by exactly the same road as one the
    /// firmware writes by hand.
    pub fn transmit(&self, val: u8) {
        // Loopback: the byte never leaves the chip, it arrives back in
        // the receive register. post1 writes 0..254 this way and insists
        // on reading the same sequence back.
        if self.looped() {
            self.input.lock().unwrap().push_back(val);
            return;
        }
        self.out.lock().unwrap().push(val);
        if let Some(f) = self.log.lock().unwrap().as_mut() {
            use std::io::Write;
            let _ = f.write_all(&[val]);
            let _ = f.flush();
        }
        let mut tcp = self.tcp.lock().unwrap();
        if !tcp.is_empty() {
            use std::io::Write;
            let mut buf = Vec::with_capacity(2);
            crate::net::telnet::escape_byte(val, &mut buf);
            tcp.retain_mut(|t| t.write_all(&buf).is_ok());
        }
    }

    /// Set the modem control register, for a DMA pair that names it rather
    /// than the transmitter.
    pub fn write_mcr(&self, val: u8) {
        self.regs.lock().unwrap()[com_reg::MCR as usize] = val;
    }

    /// Hand up to `buf.len()` received characters to the DMA engine, oldest
    /// first, and say how many. They leave the queue, so the PIO path never
    /// sees them as well.
    pub fn take_input(&self, buf: &mut [u8]) -> usize {
        let mut q = self.input.lock().unwrap();
        let n = buf.len().min(q.len());
        for (b, c) in buf.iter_mut().zip(q.drain(..n)) {
            *b = c;
        }
        n
    }

    /// True when nothing has been received and not yet taken.
    pub(crate) fn input_empty(&self) -> bool {
        self.input.lock().unwrap().is_empty()
    }
}

impl BusDevice for Com16550 {
    // Every width must be answered. An unimplemented one falls through to the
    // trait default, which reports a bus error — and a DBE inside POST looks
    // like a missing device rather than a missing method.
    fn read8(&self, addr: u32) -> BusRead8 { BusRead8::ok(self.read_reg(addr)) }
    fn read16(&self, addr: u32) -> BusRead16 { BusRead16::ok(self.read_reg(addr | 7) as u16) }
    fn write16(&self, addr: u32, val: u16) -> u32 { self.write_reg(addr | 7, val as u8); BUS_OK }
    fn write8(&self, addr: u32, val: u8) -> u32 { self.write_reg(addr, val); BUS_OK }
    fn read32(&self, addr: u32) -> BusRead32 { BusRead32::ok(self.read_reg(addr | 7) as u32) }
    fn write32(&self, addr: u32, val: u32) -> u32 { self.write_reg(addr | 7, val as u8); BUS_OK }
    fn read64(&self, addr: u32) -> BusRead64 { BusRead64::ok(self.read_reg(addr | 7) as u64) }
    fn write64(&self, addr: u32, val: u64) -> u32 { self.write_reg(addr | 7, val as u8); BUS_OK }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TCP console: a client gets the telnet handshake, then every byte
    /// the guest transmits, and what it types becomes the guest's input --
    /// with telnet's own commands taken out on the way.
    #[test]
    fn a_tcp_console_carries_both_directions() {
        use std::io::{Read, Write};
        let c = Com16550::new();
        let port = c.serve_tcp(0).expect("bind");
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut hs = [0u8; 12];
        client.read_exact(&mut hs).expect("handshake");
        assert_eq!(hs, crate::net::telnet::TelnetFilter::initial_handshake());
        // Typed, with a telnet command (IAC DO SGA, which it already offered) in between.
        client.write_all(&[b'l', 0xff, 0xfd, 0x03, b's', b'\r']).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while c.pending_input() < 3 && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.read8(MACE_COM0 + 7).data, b'l');
        assert_eq!(c.read8(MACE_COM0 + 7).data, b's');
        assert_eq!(c.read8(MACE_COM0 + 7).data, b'\r');
        assert_eq!(c.pending_input(), 0, "the telnet command is not input");
        // Transmitted: reaches the client (once it is registered as the client).
        while c.tcp.lock().unwrap().is_empty() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for &b in b"ok" {
            c.write8(MACE_COM0 + 7, b);
        }
        let mut got = [0u8; 2];
        client.read_exact(&mut got).expect("output");
        assert_eq!(&got, b"ok");
        assert_eq!(c.output(), "ok", "the console buffer still has it too");
    }
}

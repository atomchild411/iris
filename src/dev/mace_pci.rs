//! MACE's PCI host bridge on the O2: configuration space and the PCI
//! functions behind it.

use std::sync::Mutex;

use crate::traits::{BusDevice, BusRead8, BusRead16, BusRead32, BusRead64, BUS_OK};
use crate::ip32::*;

/// MACE's PCI host bridge, with nothing plugged into it.
///
/// Config cycles go through an address/data register pair, exactly like a PC's
/// 0xcf8/0xcfc. The detail that matters for an empty bus: a config read of an
/// absent device must return **all ones**, not zero. Zero reads back as vendor
/// ID 0x0000, which firmware takes for a device that is present but broken;
/// 0xffffffff is the architectural "nobody home".
/// One PCI function's configuration space.
///
/// Enough of it to be discovered and configured: identity, class, command and
/// status, and base address registers that answer a size probe. Writing all
/// ones to a BAR and reading it back is how firmware learns how much space a
/// device wants, so a BAR that stores whatever it is given reports a size of
/// four gigabytes and the bus layout falls apart.
pub struct PciFunction {
    pub name: &'static str,
    config: Mutex<[u32; 64]>,
    /// Size of each BAR in bytes; zero means the BAR is not implemented.
    bar_size: [u32; 6],
    /// Every config register touched, in first-touch order, with direction.
    touched: Mutex<Vec<(u8, bool)>>,
    /// The device's own registers, behind its memory BAR, and a log of what
    /// the guest did to them. At this stage the log is the point: it says
    /// which parts of the chip the firmware insists on.
    mem: Mutex<std::collections::BTreeMap<u32, u32>>,
    mem_log: Mutex<Vec<(u32, bool, u32, u32, u32)>>,
    /// The device's behaviour, if it has any beyond storing what it is told.
    ops: Option<std::sync::Arc<dyn PciDeviceOps>>,
}

/// What a PCI device does when the guest touches its memory BAR.
pub trait PciDeviceOps: Send + Sync {
    fn read(&self, off: u32, width: usize) -> u32;
    fn write(&self, off: u32, val: u32, width: usize);
}

impl PciFunction {
    /// `class` is the 24-bit class/subclass/interface, as it sits in the top
    /// three bytes of register 0x08.
    pub fn new(
        name: &'static str,
        vendor: u16,
        device: u16,
        class: u32,
        revision: u8,
        bar_size: [u32; 6],
    ) -> Self {
        let mut c = [0u32; 64];
        c[0] = ((device as u32) << 16) | vendor as u32;
        c[2] = (class << 8) | revision as u32;
        // Single-function, ordinary device header.
        c[3] = 0;
        // Interrupt pin A; the line is left for firmware to assign.
        c[15] = 0x0000_0100;
        Self {
            name,
            config: Mutex::new(c),
            bar_size,
            touched: Mutex::new(Vec::new()),
            mem: Mutex::new(std::collections::BTreeMap::new()),
            mem_log: Mutex::new(Vec::new()),
            ops: None,
        }
    }

    fn note(&self, reg: u8, write: bool) {
        let mut t = self.touched.lock().unwrap();
        if !t.iter().any(|&(r, w)| r == reg && w == write) {
            t.push((reg, write));
        }
    }

    /// Config registers touched so far: `(byte offset, was_write)`.
    pub fn touched(&self) -> Vec<(u8, bool)> {
        self.touched.lock().unwrap().clone()
    }

    /// The base address firmware programmed into a BAR, with the type bits
    /// masked off.
    pub fn bar(&self, i: usize) -> u32 {
        let v = self.config.lock().unwrap()[4 + i];
        if v & 1 != 0 { v & !3 } else { v & !15 }
    }

    /// Accesses to the device's memory BAR: `(offset, was_write, value, pc)`.
    pub fn mem_log(&self) -> Vec<(u32, bool, u32, u32, u32)> {
        self.mem_log.lock().unwrap().clone()
    }

    fn log_mem(&self, off: u32, write: bool, val: u32, pc: (u32, u32)) {
        let mut l = self.mem_log.lock().unwrap();
        if l.len() < 20000 {
            l.push((off, write, val, pc.0, pc.1));
        }
    }

    /// Give the function real behaviour instead of a register file.
    pub fn with_ops(mut self, ops: std::sync::Arc<dyn PciDeviceOps>) -> Self {
        self.ops = Some(ops);
        self
    }

    pub fn mem_read(&self, off: u32, width: usize, pc: (u32, u32)) -> u32 {
        if let Some(ops) = &self.ops {
            let v = ops.read(off, width);
            self.log_mem(off, false, v, pc);
            return v;
        }
        let m = self.mem.lock().unwrap();
        let mut v = 0u32;
        for i in 0..width as u32 {
            v = (v << 8) | m.get(&(off + i)).copied().unwrap_or(0);
        }
        drop(m);
        self.log_mem(off, false, v, pc);
        v
    }

    pub fn mem_write(&self, off: u32, val: u32, width: usize, pc: (u32, u32)) {
        self.log_mem(off, true, val, pc);
        if let Some(ops) = &self.ops {
            ops.write(off, val, width);
            return;
        }
        let mut m = self.mem.lock().unwrap();
        for i in 0..width as u32 {
            m.insert(off + i, (val >> (8 * (width as u32 - 1 - i))) as u8 as u32);
        }
    }

    /// True when the device has been told it may decode memory accesses.
    pub fn memory_enabled(&self) -> bool {
        self.config.lock().unwrap()[1] & 0x2 != 0
    }

    pub fn read(&self, reg: usize) -> u32 {
        self.note((reg * 4) as u8, false);
        match reg {
            // Status: report that we are a fast device with no errors.
            1 => (self.config.lock().unwrap()[1] & 0xffff) | 0x0200_0000,
            _ => self.config.lock().unwrap()[reg],
        }
    }

    pub fn write(&self, reg: usize, val: u32) {
        self.note((reg * 4) as u8, true);
        let mut c = self.config.lock().unwrap();
        match reg {
            // Identity, class and header type are read-only.
            0 | 2 | 3 => {}
            // Command register; status bits are write-one-to-clear and we
            // never set any.
            1 => c[1] = val & 0xffff,
            4..=9 => {
                let i = reg - 4;
                let size = self.bar_size[i];
                if size == 0 {
                    c[reg] = 0;
                } else {
                    // Bit 0 selects I/O space. Keep the type bits we
                    // advertise and let the address bits above the size be
                    // written; everything below reads back as zero, which is
                    // what tells firmware the size.
                    let io = i == 0;
                    let mask = !(size - 1);
                    let type_bits = if io { 1 } else { 0 };
                    c[reg] = (val & mask) | type_bits;
                }
            }
            _ => c[reg] = val,
        }
    }
}

pub struct MacePci {
    config_addr: Mutex<u32>,
    regs: Mutex<[u32; (MACEPCI_SIZE / 4) as usize]>,
    /// Distinct config addresses the guest selected, in order.
    probed: Mutex<Vec<u32>>,
    /// Devices by slot number on bus 0.
    slots: Mutex<std::collections::BTreeMap<u8, std::sync::Arc<PciFunction>>>,
}

pub mod macepci_reg {
    pub const ERROR_ADDR: u32 = 0x0000;
    pub const ERROR_FLAGS: u32 = 0x0004;
    pub const CONTROL: u32 = 0x0008;
    pub const REVISION: u32 = 0x000c;
    pub const CONFIG_ADDR: u32 = 0x0cf8;
    pub const CONFIG_DATA: u32 = 0x0cfc;
}

/// What a MACE PCI bridge reports for itself. Zero here would make the PROM
/// conclude the bridge is missing.
pub const MACEPCI_REVISION_VALUE: u32 = 1;

/// The SCSI controller an O2 has on its PCI bus: an Adaptec AIC-7880.
pub const PCI_VENDOR_ADAPTEC: u16 = 0x9004;
pub const PCI_DEVICE_AIC7880: u16 = 0x8078;
/// Mass storage controller, SCSI, no specific programming interface.
pub const PCI_CLASS_SCSI: u32 = 0x0100_00;
/// The two SCSI controllers an O2 has on its motherboard. CRIME carries a
/// separate interrupt line for each -- `CRIME_INT_PCI_SCSI0` and
/// `CRIME_INT_PCI_SCSI1` -- which is the clearest evidence that both are
/// expected to be there.
pub const IP32_SCSI_SLOT: u8 = 1;
pub const IP32_SCSI_SLOT_B: u8 = 2;
/// ISA ring base and reset register; bit 4 reads as the MACE revision.
pub const MACE_ISA_RING_BASE: u32 = MACE_BASE + 0x0031_0000;
pub const MACE_REV2_BIT: u64 = 0x10;
/// The serial DMA channels' registers: port 0 at the start, port 1 16 KB on.
pub const MACE_SDMA: u32 = MACE_BASE + 0x0031_8000;
pub const MACE_SDMA_SIZE: u32 = 0x0000_8000;

impl Default for MacePci {
    fn default() -> Self { Self::new() }
}

impl MacePci {
    pub fn new() -> Self {
        Self {
            config_addr: Mutex::new(0),
            regs: Mutex::new([0u32; (MACEPCI_SIZE / 4) as usize]),
            probed: Mutex::new(Vec::new()),
            slots: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Put a device in a slot on bus 0.
    pub fn attach(&self, slot: u8, f: std::sync::Arc<PciFunction>) {
        self.slots.lock().unwrap().insert(slot, f);
    }

    pub fn slot(&self, slot: u8) -> Option<std::sync::Arc<PciFunction>> {
        self.slots.lock().unwrap().get(&slot).cloned()
    }

    /// The device decoding a PCI memory address, and the offset within its
    /// BAR, if any device claims it.
    pub fn mem_target(&self, pci_addr: u32) -> Option<(std::sync::Arc<PciFunction>, u32)> {
        for f in self.slots.lock().unwrap().values() {
            if !f.memory_enabled() {
                continue;
            }
            for i in 0..6 {
                let size = f.bar_size[i];
                let base = f.bar(i);
                if size != 0 && base != 0 && pci_addr >= base && pci_addr < base + size {
                    return Some((f.clone(), pci_addr - base));
                }
            }
        }
        None
    }

    /// Split a CONFIG_ADDR value into the function it selects and the
    /// register within it. Only bus 0 exists here.
    fn selected(&self) -> Option<(std::sync::Arc<PciFunction>, usize)> {
        // Bit 31 is the enable bit of the standard PCI mechanism, and MACE's
        // bridge does not insist on it: the PROM sets it (0x80000800) and
        // NetBSD's driver does not (0x00000800). Requiring it makes the whole
        // bus invisible to one of them, which is exactly how the kernel came
        // to probe and find nothing.
        let a = *self.config_addr.lock().unwrap() & 0x7fff_ffff;
        if (a >> 16) & 0xff != 0 {
            return None;
        }
        let dev = ((a >> 11) & 0x1f) as u8;
        let func = (a >> 8) & 7;
        if func != 0 {
            return None;
        }
        let f = self.slots.lock().unwrap().get(&dev).cloned()?;
        Some((f, ((a >> 2) & 0x3f) as usize))
    }

    /// Config addresses selected so far, in first-touch order.
    pub fn probed(&self) -> Vec<u32> {
        self.probed.lock().unwrap().clone()
    }
}

impl BusDevice for MacePci {
    fn read32(&self, addr: u32) -> BusRead32 {
        let off = addr & (MACEPCI_SIZE - 1) & !3;
        match off {
            macepci_reg::CONFIG_DATA => BusRead32::ok(match self.selected() {
                Some((f, reg)) => f.read(reg),
                // An absent device: the bus floats high.
                None => 0xffff_ffff,
            }),
            macepci_reg::CONFIG_ADDR => BusRead32::ok(*self.config_addr.lock().unwrap()),
            macepci_reg::REVISION => BusRead32::ok(MACEPCI_REVISION_VALUE),
            _ => BusRead32::ok(self.regs.lock().unwrap()[(off / 4) as usize]),
        }
    }

    fn write32(&self, addr: u32, val: u32) -> u32 {
        let off = addr & (MACEPCI_SIZE - 1) & !3;
        match off {
            macepci_reg::CONFIG_ADDR => {
                *self.config_addr.lock().unwrap() = val;
                let mut p = self.probed.lock().unwrap();
                if val != 0 && !p.contains(&val) {
                    p.push(val);
                }
            }
            macepci_reg::CONFIG_DATA => {
                if let Some((f, reg)) = self.selected() {
                    f.write(reg, val);
                }
            }
            _ => self.regs.lock().unwrap()[(off / 4) as usize] = val,
        }
        BUS_OK
    }

    fn read64(&self, addr: u32) -> BusRead64 {
        let hi = self.read32(addr).data as u64;
        let lo = self.read32(addr.wrapping_add(4)).data as u64;
        BusRead64::ok((hi << 32) | lo)
    }

    fn write64(&self, addr: u32, val: u64) -> u32 {
        self.write32(addr, (val >> 32) as u32);
        self.write32(addr.wrapping_add(4), val as u32)
    }
}

/// The PCI memory window with nothing behind it. Reads all-ones, absorbs
/// writes, and counts both so we can tell whether the PROM keeps poking.
pub struct PciNativeView {
    reads: Mutex<u64>,
    writes: Mutex<u64>,
}

impl Default for PciNativeView {
    fn default() -> Self { Self::new() }
}

impl PciNativeView {
    pub fn new() -> Self { Self { reads: Mutex::new(0), writes: Mutex::new(0) } }
    pub fn counts(&self) -> (u64, u64) {
        (*self.reads.lock().unwrap(), *self.writes.lock().unwrap())
    }
}

impl BusDevice for PciNativeView {
    fn read8(&self, _a: u32) -> BusRead8 { *self.reads.lock().unwrap() += 1; BusRead8::ok(0xff) }
    fn read16(&self, _a: u32) -> BusRead16 { *self.reads.lock().unwrap() += 1; BusRead16::ok(0xffff) }
    fn read32(&self, _a: u32) -> BusRead32 { *self.reads.lock().unwrap() += 1; BusRead32::ok(0xffff_ffff) }
    fn read64(&self, _a: u32) -> BusRead64 { *self.reads.lock().unwrap() += 1; BusRead64::ok(u64::MAX) }
    fn write8(&self, _a: u32, _v: u8) -> u32 { *self.writes.lock().unwrap() += 1; BUS_OK }
    fn write16(&self, _a: u32, _v: u16) -> u32 { *self.writes.lock().unwrap() += 1; BUS_OK }
    fn write32(&self, _a: u32, _v: u32) -> u32 { *self.writes.lock().unwrap() += 1; BUS_OK }
    fn write64(&self, _a: u32, _v: u64) -> u32 { *self.writes.lock().unwrap() += 1; BUS_OK }
}

impl crate::traits::Device for MacePci {
    fn step(&self, _cycles: u64) {}
    fn stop(&self) {}
    fn start(&self) {}
    fn is_running(&self) -> bool { true }
    fn get_clock(&self) -> u64 { 0 }

    fn register_commands(&self) -> Vec<(String, String)> {
        vec![("pci".to_string(), "MACE PCI bridge: pci status | pci config <slot>".to_string())]
    }

    fn execute_command(&self, cmd: &str, args: &[&str], mut w: Box<dyn std::io::Write + Send>) -> Result<(), String> {
        use std::io::Write;
        if cmd != "pci" {
            return Err("Command not found".to_string());
        }
        match args.first().copied().unwrap_or("status") {
            "status" => {
                let r = self.regs.lock().unwrap();
                let reg = |off: u32| r[(off / 4) as usize];
                writeln!(w, "MACE PCI: control {:#010x} revision {:#x} error flags {:#x} at {:#010x}, config address {:#010x}",
                    reg(macepci_reg::CONTROL), reg(macepci_reg::REVISION), reg(macepci_reg::ERROR_FLAGS),
                    reg(macepci_reg::ERROR_ADDR), *self.config_addr.lock().unwrap()).ok();
                for (slot, f) in self.slots.lock().unwrap().iter() {
                    let id = f.read(0);
                    let bars: Vec<String> = (0..6).map(|i| format!("{:#x}", f.bar(i))).filter(|b| b != "0x0").collect();
                    writeln!(w, "  slot {slot}: {} {:04x}:{:04x}, command {:#06x}, BARs [{}]",
                        f.name, id & 0xffff, id >> 16, f.read(1) & 0xffff, bars.join(", ")).ok();
                }
            }
            "config" => {
                let slot: u8 = args.get(1).and_then(|v| v.parse().ok()).ok_or("Usage: pci config <slot>")?;
                let f = self.slot(slot).ok_or(format!("pci: nothing in slot {slot}"))?;
                for row in 0..16 {
                    let v: Vec<String> = (0..4).map(|k| format!("{:08x}", f.read(row * 4 + k))).collect();
                    writeln!(w, "  {:02x}: {}", row * 16, v.join(" ")).ok();
                }
            }
            _ => return Err("Usage: pci status | config <slot>".to_string()),
        }
        Ok(())
    }
}

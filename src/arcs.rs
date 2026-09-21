//! An ARCS firmware interface, so a kernel can boot with no PROM.
//!
//! Phase 1 of `docs/arcs-scoping.md`. The guest finds firmware by looking for
//! a System Parameter Block at physical `0x1000` whose signature is `'ARCS'`;
//! the SPB points at a vector of 32-bit code addresses, and calling firmware
//! means jumping to one of them. This module builds both in guest memory and
//! answers the calls.
//!
//! The entries do not point at MIPS code. They point into a reserved range
//! ([`TRAP_BASE`]) that the executor recognises: when the program counter
//! arrives there, the call is serviced in Rust, `v0` is set, and control
//! returns to `ra`. That keeps the firmware out of the guest's address space
//! entirely — there is no code to disassemble, relocate or accidentally
//! overwrite, and no calling convention to implement twice.
//!
//! Interception happens when the PC *arrives*, not when the jump executes,
//! because MIPS has a branch delay slot: the instruction after `jalr` runs
//! before control transfers, and skipping it would corrupt the caller.
//!
//! ## What has to be right
//!
//! Which entries a guest actually uses was measured rather than assumed — see
//! the Phase 0 section of the scoping doc. IRIX uses seven, NetBSD thirteen,
//! and the two sets differ in both directions: IRIX needs `FlushAllCaches`,
//! which nothing in NetBSD calls, and never calls `GetEnvironmentVariable`,
//! which is NetBSD's hottest entry.
//!
//! The enum values below are the **sgimips** ones. SGI renumbered the memory
//! types and the component classes relative to the ARC standard, and picking
//! the wrong set makes a kernel either panic on an unknown descriptor or —
//! worse, because it is silent — count free RAM as firmware-reserved.

use crate::traits::BusDevice;
use std::collections::BTreeMap;

/// Physical address of the System Parameter Block. Hard-coded in every guest
/// that looks for ARCS, so this is not a choice.
pub const SPB_PHYS: u32 = 0x0000_1000;
/// `'ARCS'`.
pub const SPB_SIGNATURE: u32 = 0x5343_5241;
/// Where we put the vector table. Anywhere in the SPB's page will do; this
/// matches what a real SGI PROM publishes, which keeps traces comparable.
pub const VECTOR_PHYS: u32 = 0x0000_1800;
/// Unmapped window the vector entries point into. Reads of this range never
/// happen — the executor intercepts arrival before any fetch.
pub const TRAP_BASE: u32 = 0x1fbf_0000;
/// Bytes per trap slot. One instruction's worth is enough; the space is only
/// there to make each entry a distinct address.
pub const TRAP_STRIDE: u32 = 4;

/// Scratch for firmware-owned structures: descriptors, component nodes and
/// environment strings. Reported to the guest as `FirmwarePermanent` so it is
/// not handed out as free memory.
pub const DATA_PHYS: u32 = 0x0000_2000;
pub const DATA_SIZE: u32 = 0x0000_2000;

/// Entry count SGI publishes. Two of these are null on real hardware
/// (`ReturnFromMain` and `Signal`), which is a useful fingerprint.
pub const ENTRY_COUNT: usize = 35;

pub mod entry {
    pub const LOAD: usize = 0;
    pub const INVOKE: usize = 1;
    pub const EXECUTE: usize = 2;
    pub const HALT: usize = 3;
    pub const POWER_DOWN: usize = 4;
    pub const RESTART: usize = 5;
    pub const REBOOT: usize = 6;
    pub const ENTER_INTERACTIVE_MODE: usize = 7;
    pub const RETURN_FROM_MAIN: usize = 8;
    pub const GET_PEER: usize = 9;
    pub const GET_CHILD: usize = 10;
    pub const GET_PARENT: usize = 11;
    pub const GET_CONFIGURATION_DATA: usize = 12;
    pub const ADD_CHILD: usize = 13;
    pub const DELETE_COMPONENT: usize = 14;
    pub const GET_COMPONENT: usize = 15;
    pub const SAVE_CONFIGURATION: usize = 16;
    pub const GET_SYSTEM_ID: usize = 17;
    pub const GET_MEMORY_DESCRIPTOR: usize = 18;
    pub const SIGNAL: usize = 19;
    pub const GET_TIME: usize = 20;
    pub const GET_RELATIVE_TIME: usize = 21;
    pub const GET_DIRECTORY_ENTRY: usize = 22;
    pub const OPEN: usize = 23;
    pub const CLOSE: usize = 24;
    pub const READ: usize = 25;
    pub const GET_READ_STATUS: usize = 26;
    pub const WRITE: usize = 27;
    pub const SEEK: usize = 28;
    pub const MOUNT: usize = 29;
    pub const GET_ENVIRONMENT_VARIABLE: usize = 30;
    pub const SET_ENVIRONMENT_VARIABLE: usize = 31;
    pub const GET_FILE_INFORMATION: usize = 32;
    pub const SET_FILE_INFORMATION: usize = 33;
    pub const FLUSH_ALL_CACHES: usize = 34;
}

/// Memory descriptor types, **sgimips numbering**.
pub mod mem_type {
    pub const EXCEPTION_BLOCK: u32 = 0;
    pub const SYSTEM_PARAMETER_BLOCK: u32 = 1;
    pub const FREE_CONTIGUOUS: u32 = 2;
    pub const FREE_MEMORY: u32 = 3;
    pub const BAD_MEMORY: u32 = 4;
    pub const LOADED_PROGRAM: u32 = 5;
    pub const FIRMWARE_TEMPORARY: u32 = 6;
    pub const FIRMWARE_PERMANENT: u32 = 7;
}

/// Component classes and types, **sgimips numbering**.
pub mod component {
    pub const CLASS_SYSTEM: u32 = 0;
    pub const CLASS_PROCESSOR: u32 = 1;
    pub const TYPE_ARC: u32 = 0;
    pub const TYPE_CPU: u32 = 1;
    /// Bytes per `arcbios_component`.
    pub const SIZE: u32 = 36;
}

/// Status codes. Only success is ever tested by the guests we care about, but
/// returning a plausible failure beats returning success for a call that did
/// nothing.
pub const ESUCCESS: u64 = 0;
pub const EINVAL: u64 = 6;
pub const ENOENT: u64 = 12;

pub const ARCBIOS_PAGESIZE: u32 = 4096;

/// One memory descriptor as the guest sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemDescriptor {
    pub mem_type: u32,
    pub base_page: u32,
    pub page_count: u32,
}

/// The firmware's model of the machine.
pub struct Arcs {
    /// Descriptors in ascending base order — the guest is entitled to assume
    /// that, and at least one kernel says so in a comment.
    descriptors: Vec<MemDescriptor>,
    /// Guest address of each descriptor, parallel to `descriptors`. Real
    /// firmware hands back pointers to distinct static structures rather than
    /// one reused buffer, so we do the same.
    descriptor_addrs: Vec<u32>,
    /// Guest address of each component node, root first.
    component_addrs: Vec<u32>,
    env: BTreeMap<String, String>,
    /// Guest address of each environment value, filled in on demand: a
    /// returned string has to live somewhere the guest can read.
    env_addrs: BTreeMap<String, u32>,
    /// Next free byte of the scratch area.
    alloc: u32,
    vendor: [u8; 8],
    product: [u8; 8],
    sysid_addr: u32,
    /// Entries we have already complained about, so one unimplemented call
    /// in a loop does not bury the console.
    unimplemented: std::collections::BTreeSet<usize>,
}

impl Arcs {
    /// `ram_bytes` is the contiguous RAM the machine has at physical 0.
    pub fn new(ram_bytes: u64) -> Self {
        let mut a = Self {
            descriptors: Vec::new(),
            descriptor_addrs: Vec::new(),
            component_addrs: Vec::new(),
            env: BTreeMap::new(),
            env_addrs: BTreeMap::new(),
            alloc: DATA_PHYS,
            vendor: *b"SGI     ",
            product: *b"IRIS    ",
            sysid_addr: 0,
            unimplemented: std::collections::BTreeSet::new(),
        };
        a.build_memory_map(ram_bytes);
        a
    }

    /// The map a guest is handed. Low memory is carved up so that nothing we
    /// own is offered as free: the exception block, the SPB page holding the
    /// vector table, and our scratch area.
    fn build_memory_map(&mut self, ram_bytes: u64) {
        let page = |addr: u32| addr / ARCBIOS_PAGESIZE;
        let total_pages = (ram_bytes / ARCBIOS_PAGESIZE as u64) as u32;

        let mut d = Vec::new();
        d.push(MemDescriptor {
            mem_type: mem_type::EXCEPTION_BLOCK,
            base_page: 0,
            page_count: page(SPB_PHYS),
        });
        d.push(MemDescriptor {
            mem_type: mem_type::SYSTEM_PARAMETER_BLOCK,
            base_page: page(SPB_PHYS),
            page_count: page(DATA_PHYS) - page(SPB_PHYS),
        });
        d.push(MemDescriptor {
            mem_type: mem_type::FIRMWARE_PERMANENT,
            base_page: page(DATA_PHYS),
            page_count: DATA_SIZE / ARCBIOS_PAGESIZE,
        });
        let free_base = page(DATA_PHYS + DATA_SIZE);
        if total_pages > free_base {
            d.push(MemDescriptor {
                mem_type: mem_type::FREE_MEMORY,
                base_page: free_base,
                page_count: total_pages - free_base,
            });
        }
        self.descriptors = d;
    }

    pub fn descriptors(&self) -> &[MemDescriptor] {
        &self.descriptors
    }

    pub fn set_env(&mut self, name: &str, value: &str) {
        self.env.insert(name.to_ascii_lowercase(), value.to_string());
    }

    /// Lookups are case-insensitive, which is what real firmware does and what
    /// guests rely on — `OSLoadPartition` and `osloadpartition` are the same
    /// variable.
    pub fn env_get(&self, name: &str) -> Option<&str> {
        self.env.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }

    /// Address of the trap slot for an entry.
    pub fn trap_addr(index: usize) -> u32 {
        TRAP_BASE + (index as u32) * TRAP_STRIDE
    }

    /// Entry index for a trap address, if it is one.
    pub fn entry_for_pc(pc: u64) -> Option<usize> {
        let p = (pc as u32) & 0x1fff_ffff;
        let base = TRAP_BASE & 0x1fff_ffff;
        let end = base + (ENTRY_COUNT as u32) * TRAP_STRIDE;
        if p < base || p >= end || (p - base) % TRAP_STRIDE != 0 {
            return None;
        }
        Some(((p - base) / TRAP_STRIDE) as usize)
    }

    fn alloc(&mut self, len: u32) -> u32 {
        let at = (self.alloc + 3) & !3;
        self.alloc = at + len;
        at
    }

    /// Write the SPB, the vector table and the firmware-owned structures into
    /// guest memory. Call once, before the guest runs.
    pub fn install(&mut self, bus: &dyn BusDevice) {
        // Descriptors, each at its own address.
        self.descriptor_addrs.clear();
        for i in 0..self.descriptors.len() {
            let d = self.descriptors[i];
            let at = self.alloc(12);
            bus.write32(at, d.mem_type);
            bus.write32(at + 4, d.base_page);
            bus.write32(at + 8, d.page_count);
            self.descriptor_addrs.push(at);
        }

        // System id, returned by GetSystemId as two 8-byte unterminated fields.
        let sysid = self.alloc(16);
        for (i, b) in self.vendor.iter().enumerate() {
            bus.write8(sysid + i as u32, *b);
        }
        for (i, b) in self.product.iter().enumerate() {
            bus.write8(sysid + 8 + i as u32, *b);
        }
        self.sysid_addr = sysid;

        // A two-node component tree: one System, one Processor. That is what a
        // kernel walks for its model string and its CPU count, and more nodes
        // would be invention rather than modelling.
        self.component_addrs.clear();
        let sys_ident = self.put_cstr(bus, "SGI-IRIS");
        let cpu_ident = self.put_cstr(bus, "MIPS-R4400");
        let sys = self.put_component(
            bus,
            component::CLASS_SYSTEM,
            component::TYPE_ARC,
            sys_ident,
            "SGI-IRIS".len() as u32,
        );
        let cpu = self.put_component(
            bus,
            component::CLASS_PROCESSOR,
            component::TYPE_CPU,
            cpu_ident,
            "MIPS-R4400".len() as u32,
        );
        self.component_addrs.push(sys);
        self.component_addrs.push(cpu);

        // Vector table. Every slot gets a trap address except the two SGI
        // leaves null — a guest that checks for them would otherwise conclude
        // it is not talking to SGI firmware.
        for i in 0..ENTRY_COUNT {
            let v = if i == entry::RETURN_FROM_MAIN || i == entry::SIGNAL {
                0
            } else {
                Self::trap_addr(i)
            };
            bus.write32(VECTOR_PHYS + (i as u32) * 4, v);
        }

        // SPB last: a guest polling for the signature must not see one before
        // the vector it points at exists.
        bus.write32(SPB_PHYS + 0x04, 0x48); // SPBLength
        bus.write32(SPB_PHYS + 0x08, 0x0001_0000); // Version 1, Revision 0
        bus.write32(SPB_PHYS + 0x10, 0); // DebugBlock: no kernel debugger
        bus.write32(SPB_PHYS + 0x1c, (ENTRY_COUNT as u32) * 4);
        bus.write32(SPB_PHYS + 0x20, 0xa000_0000 | VECTOR_PHYS);
        bus.write32(SPB_PHYS, SPB_SIGNATURE);
    }

    fn put_cstr(&mut self, bus: &dyn BusDevice, s: &str) -> u32 {
        let at = self.alloc(s.len() as u32 + 1);
        for (i, b) in s.bytes().enumerate() {
            bus.write8(at + i as u32, b);
        }
        bus.write8(at + s.len() as u32, 0);
        at
    }

    fn put_component(
        &mut self,
        bus: &dyn BusDevice,
        class: u32,
        ctype: u32,
        ident: u32,
        ident_len: u32,
    ) -> u32 {
        let at = self.alloc(component::SIZE);
        bus.write32(at, class);
        bus.write32(at + 4, ctype);
        bus.write32(at + 8, 0); // Flags
        bus.write32(at + 12, 0); // Version:Revision
        bus.write32(at + 16, 0); // Key
        bus.write32(at + 20, 0); // AffinityMask
        bus.write32(at + 24, 0); // ConfigurationDataSize
        bus.write32(at + 28, ident_len);
        bus.write32(at + 32, 0xa000_0000 | ident);
        at
    }

    /// Read a NUL-terminated string from guest memory.
    fn read_cstr(bus: &dyn BusDevice, addr: u32, max: usize) -> String {
        let mut out = Vec::new();
        for i in 0..max {
            let r = bus.read8(addr.wrapping_add(i as u32));
            if !r.is_ok() || r.data == 0 {
                break;
            }
            out.push(r.data);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Guest addresses are handed to us in whatever window the caller used;
    /// the bus wants the physical address.
    fn phys(addr: u64) -> u32 {
        (addr as u32) & 0x1fff_ffff
    }

    fn system_node(&self) -> u32 {
        self.component_addrs.first().copied().unwrap_or(0)
    }
    fn cpu_node(&self) -> u32 {
        self.component_addrs.get(1).copied().unwrap_or(0)
    }

    /// Service a firmware call. `args` is `a0`-`a3`; the return is `v0`.
    ///
    /// Anything not implemented returns a failure status and says so once, so
    /// an unexpected call is a finding rather than a guest wandering off with
    /// a plausible-looking zero.
    pub fn dispatch(&mut self, index: usize, args: [u64; 4], bus: &dyn BusDevice) -> CallResult {
        use entry::*;
        match index {
            GET_MEMORY_DESCRIPTOR => {
                // NULL asks for the first; otherwise the argument is the
                // descriptor we last returned and the answer is the next.
                let next = if args[0] == 0 {
                    self.descriptor_addrs.first().copied()
                } else {
                    let cur = Self::phys(args[0]);
                    self.descriptor_addrs
                        .iter()
                        .position(|a| *a == cur)
                        .and_then(|i| self.descriptor_addrs.get(i + 1).copied())
                };
                CallResult::Value(next.map(|a| 0xa000_0000u64 | a as u64).unwrap_or(0))
            }

            GET_CHILD => {
                let a = Self::phys(args[0]);
                let r = if args[0] == 0 {
                    self.system_node()
                } else if a == self.system_node() {
                    self.cpu_node()
                } else {
                    0
                };
                CallResult::Value(if r == 0 { 0 } else { 0xa000_0000u64 | r as u64 })
            }

            // Neither node has a sibling in a two-node tree.
            GET_PEER => CallResult::Value(0),

            GET_SYSTEM_ID => CallResult::Value(0xa000_0000u64 | self.sysid_addr as u64),

            GET_ENVIRONMENT_VARIABLE => {
                let name = Self::read_cstr(bus, Self::phys(args[0]), 64);
                match self.env_value_addr(bus, &name) {
                    Some(a) => CallResult::Value(0xa000_0000u64 | a as u64),
                    None => CallResult::Value(0),
                }
            }

            WRITE => {
                let len = args[2] as u32;
                let base = Self::phys(args[1]);
                let mut out = Vec::with_capacity(len as usize);
                for i in 0..len {
                    let r = bus.read8(base.wrapping_add(i));
                    out.push(if r.is_ok() { r.data } else { 0 });
                }
                if args[3] != 0 {
                    bus.write32(Self::phys(args[3]), len);
                }
                CallResult::Console(out)
            }

            // No console input is wired up yet: report zero bytes read rather
            // than an error, which is what a guest polling an idle tty expects.
            READ => {
                if args[3] != 0 {
                    bus.write32(Self::phys(args[3]), 0);
                }
                CallResult::Value(ESUCCESS)
            }
            GET_READ_STATUS => CallResult::Value(ENOENT),

            // Nothing to do: this emulator's caches cannot hold anything the
            // guest has not already published to memory.
            FLUSH_ALL_CACHES => CallResult::Value(ESUCCESS),

            REBOOT | POWER_DOWN | ENTER_INTERACTIVE_MODE | HALT | RESTART => CallResult::Halt,

            _ => {
                if self.unimplemented.insert(index) {
                    eprintln!(
                        "arcs: unimplemented entry {index} ({}) called — a0={:#x} a1={:#x}",
                        crate::arcs_trace::ARCS_NAMES[index], args[0], args[1]
                    );
                }
                CallResult::Value(EINVAL)
            }
        }
    }

    /// Address of an environment value in guest memory, published on first ask.
    fn env_value_addr(&mut self, bus: &dyn BusDevice, name: &str) -> Option<u32> {
        let key = name.to_ascii_lowercase();
        if let Some(a) = self.env_addrs.get(&key) {
            return Some(*a);
        }
        let value = self.env.get(&key)?.clone();
        let at = self.put_cstr(bus, &value);
        self.env_addrs.insert(key, at);
        Some(at)
    }
}

/// What a firmware call produced.
pub enum CallResult {
    /// Put this in `v0` and return to the caller.
    Value(u64),
    /// Console output; the caller routes it and returns success.
    Console(Vec<u8>),
    /// The guest asked to stop.
    Halt,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Memory;
    use std::sync::Arc;

    fn arcs_on(ram: u64) -> (Arcs, Arc<Memory>) {
        let mem = Arc::new(Memory::new(16));
        let mut a = Arcs::new(ram);
        a.install(mem.as_ref());
        (a, mem)
    }

    /// A guest finds firmware by the signature and the vector pointer. If
    /// either is wrong it concludes there is no ARCS at all and, on the ports
    /// that have one, silently falls back to its own emulation.
    #[test]
    fn a_guest_can_find_the_firmware() {
        let (_a, mem) = arcs_on(128 * 1024 * 1024);
        assert_eq!(mem.read32(SPB_PHYS).data, SPB_SIGNATURE);
        assert_eq!(mem.read32(SPB_PHYS + 0x1c).data, (ENTRY_COUNT as u32) * 4);
        assert_eq!(mem.read32(SPB_PHYS + 0x20).data, 0xa000_0000 | VECTOR_PHYS);
    }

    /// The two entries SGI leaves null are the fingerprint that this is SGI
    /// firmware rather than generic ARC. Real hardware shows exactly these two
    /// and no others — it is how the tracer confirmed the table three times.
    #[test]
    fn the_vector_table_has_sgis_two_null_entries_and_no_others() {
        let (_a, mem) = arcs_on(128 * 1024 * 1024);
        let mut nulls = Vec::new();
        for i in 0..ENTRY_COUNT {
            if mem.read32(VECTOR_PHYS + (i as u32) * 4).data == 0 {
                nulls.push(i);
            }
        }
        assert_eq!(nulls, vec![entry::RETURN_FROM_MAIN, entry::SIGNAL]);
    }

    /// Every live entry must map back to itself, or a call lands on the wrong
    /// implementation — which would be silent, since they all return integers.
    #[test]
    fn every_entry_address_decodes_to_its_own_index() {
        for i in 0..ENTRY_COUNT {
            assert_eq!(Arcs::entry_for_pc(Arcs::trap_addr(i) as u64), Some(i), "entry {i}");
        }
        // And through the other unmapped window, as a call actually arrives.
        let kseg1 = 0xffff_ffff_a000_0000u64 | Arcs::trap_addr(entry::READ) as u64;
        assert_eq!(Arcs::entry_for_pc(kseg1), Some(entry::READ));
    }

    /// Ordinary addresses must not be mistaken for firmware calls.
    #[test]
    fn addresses_outside_the_trap_window_are_not_entries() {
        assert_eq!(Arcs::entry_for_pc(0x8000_0000), None);
        assert_eq!(Arcs::entry_for_pc(TRAP_BASE as u64 - 4), None);
        let past_end = TRAP_BASE as u64 + (ENTRY_COUNT as u64) * TRAP_STRIDE as u64;
        assert_eq!(Arcs::entry_for_pc(past_end), None);
    }

    /// Nothing the firmware owns may be offered as free memory, and the map
    /// must ascend — a kernel is entitled to assume both.
    #[test]
    fn the_memory_map_ascends_and_reserves_what_firmware_owns() {
        let (a, _mem) = arcs_on(128 * 1024 * 1024);
        let d = a.descriptors();
        assert!(d.len() >= 4);
        for w in d.windows(2) {
            assert!(w[0].base_page < w[1].base_page, "descriptors must ascend");
            assert_eq!(
                w[0].base_page + w[0].page_count,
                w[1].base_page,
                "descriptors must not leave gaps"
            );
        }
        // The SPB page and the scratch area are not free.
        let spb_page = SPB_PHYS / ARCBIOS_PAGESIZE;
        let data_page = DATA_PHYS / ARCBIOS_PAGESIZE;
        for d in d {
            let covers = |p: u32| p >= d.base_page && p < d.base_page + d.page_count;
            if covers(spb_page) || covers(data_page) {
                assert_ne!(d.mem_type, mem_type::FREE_MEMORY, "firmware memory offered as free");
                assert_ne!(d.mem_type, mem_type::FREE_CONTIGUOUS);
            }
        }
    }

    /// The memory types are SGI's, not the ARC standard's. Getting this wrong
    /// makes a kernel count free RAM as firmware-reserved, silently.
    #[test]
    fn memory_types_use_the_sgi_numbering() {
        assert_eq!(mem_type::FREE_CONTIGUOUS, 2);
        assert_eq!(mem_type::FREE_MEMORY, 3);
        assert_eq!(mem_type::FIRMWARE_PERMANENT, 7);
    }

    /// Iterating the memory map: NULL asks for the first descriptor, and each
    /// answer is the token for the next. It must terminate, and it must visit
    /// every descriptor exactly once — a kernel walks this to find its RAM.
    #[test]
    fn the_memory_map_iterates_to_completion() {
        let (mut a, mem) = arcs_on(128 * 1024 * 1024);
        let expected = a.descriptors().len();
        let mut seen = Vec::new();
        let mut tok = 0u64;
        for _ in 0..expected + 4 {
            match a.dispatch(entry::GET_MEMORY_DESCRIPTOR, [tok, 0, 0, 0], mem.as_ref()) {
                CallResult::Value(0) => break,
                CallResult::Value(v) => {
                    assert!(!seen.contains(&v), "descriptor {v:#x} returned twice");
                    seen.push(v);
                    tok = v;
                }
                _ => panic!("GetMemoryDescriptor must return a value"),
            }
        }
        assert_eq!(seen.len(), expected, "walk did not visit every descriptor");
        // And the first one really is the exception block, read back from
        // guest memory rather than from our own copy.
        let first = (seen[0] as u32) & 0x1fff_ffff;
        assert_eq!(mem.read32(first).data, mem_type::EXCEPTION_BLOCK);
    }

    /// `GetChild(NULL)` is the root; the CPU hangs off it; nothing has a peer.
    /// A tree that never terminates hangs the kernel walking it.
    #[test]
    fn the_component_tree_is_walkable_and_terminates() {
        let (mut a, mem) = arcs_on(128 * 1024 * 1024);
        let root = match a.dispatch(entry::GET_CHILD, [0, 0, 0, 0], mem.as_ref()) {
            CallResult::Value(v) => v,
            _ => panic!("GetChild"),
        };
        assert_ne!(root, 0, "there must be a root component");
        let cpu = match a.dispatch(entry::GET_CHILD, [root, 0, 0, 0], mem.as_ref()) {
            CallResult::Value(v) => v,
            _ => panic!("GetChild"),
        };
        assert_ne!(cpu, 0, "the root must have a child");
        // The walk ends.
        assert!(matches!(
            a.dispatch(entry::GET_CHILD, [cpu, 0, 0, 0], mem.as_ref()),
            CallResult::Value(0)
        ));
        assert!(matches!(
            a.dispatch(entry::GET_PEER, [cpu, 0, 0, 0], mem.as_ref()),
            CallResult::Value(0)
        ));
        // The root is a System node and its child is a CPU, which is what a
        // kernel matches on to find its model name and count processors.
        let r = (root as u32) & 0x1fff_ffff;
        let c = (cpu as u32) & 0x1fff_ffff;
        assert_eq!(mem.read32(r).data, component::CLASS_SYSTEM);
        assert_eq!(mem.read32(c).data, component::CLASS_PROCESSOR);
        assert_eq!(mem.read32(c + 4).data, component::TYPE_CPU);
    }

    /// A variable's value has to be readable *by the guest*, so the call must
    /// return a pointer into guest memory holding the NUL-terminated string —
    /// not merely report that the variable exists.
    #[test]
    fn getenv_returns_a_pointer_the_guest_can_read() {
        let (mut a, mem) = arcs_on(128 * 1024 * 1024);
        a.set_env("OSLoadOptions", "auto");
        let name_at = 0x9000u32;
        for (i, b) in b"OSLoadOptions".iter().enumerate() {
            mem.write8(name_at + i as u32, *b);
        }
        mem.write8(name_at + 13, 0);
        let p = match a.dispatch(entry::GET_ENVIRONMENT_VARIABLE, [name_at as u64, 0, 0, 0], mem.as_ref()) {
            CallResult::Value(v) => v,
            _ => panic!("GetEnvironmentVariable"),
        };
        assert_ne!(p, 0);
        assert_eq!(Arcs::read_cstr(mem.as_ref(), Arcs::phys(p), 32), "auto");
    }

    /// An absent variable is NULL, not a pointer to an empty string — guests
    /// branch on the null.
    #[test]
    fn getenv_returns_null_for_an_unset_variable() {
        let (mut a, mem) = arcs_on(128 * 1024 * 1024);
        let name_at = 0x9000u32;
        for (i, b) in b"nosuchvar".iter().enumerate() {
            mem.write8(name_at + i as u32, *b);
        }
        mem.write8(name_at + 9, 0);
        assert!(matches!(
            a.dispatch(entry::GET_ENVIRONMENT_VARIABLE, [name_at as u64, 0, 0, 0], mem.as_ref()),
            CallResult::Value(0)
        ));
    }

    /// Write takes a counted buffer and must report how much it consumed, via
    /// a pointer. A guest that buffers per line depends on the count.
    #[test]
    fn write_emits_the_buffer_and_reports_the_count() {
        let (mut a, mem) = arcs_on(128 * 1024 * 1024);
        let buf = 0x9100u32;
        for (i, b) in b"hello".iter().enumerate() {
            mem.write8(buf + i as u32, *b);
        }
        let count_at = 0x9200u32;
        match a.dispatch(entry::WRITE, [1, buf as u64, 5, count_at as u64], mem.as_ref()) {
            CallResult::Console(out) => assert_eq!(out, b"hello"),
            _ => panic!("Write must produce console output"),
        }
        assert_eq!(mem.read32(count_at).data, 5);
    }

    /// The entries that stop the machine must say so rather than returning.
    #[test]
    fn the_halting_entries_halt() {
        let (mut a, mem) = arcs_on(128 * 1024 * 1024);
        for e in [entry::REBOOT, entry::POWER_DOWN, entry::ENTER_INTERACTIVE_MODE, entry::HALT] {
            assert!(matches!(a.dispatch(e, [0; 4], mem.as_ref()), CallResult::Halt), "entry {e}");
        }
    }

    /// Environment lookups are case-insensitive.
    #[test]
    fn environment_lookup_ignores_case() {
        let (mut a, _mem) = arcs_on(128 * 1024 * 1024);
        a.set_env("OSLoadPartition", "scsi(0)disk(1)rdisk(0)partition(0)");
        assert!(a.env_get("osloadpartition").is_some());
        assert!(a.env_get("OSLOADPARTITION").is_some());
        assert_eq!(a.env_get("nosuchvariable"), None);
    }

    /// All of RAM above the firmware's own area is offered, and the total adds
    /// up — an off-by-one page here is memory the guest never sees.
    #[test]
    fn the_whole_of_ram_is_accounted_for() {
        let ram = 128 * 1024 * 1024u64;
        let (a, _mem) = arcs_on(ram);
        let pages: u32 = a.descriptors().iter().map(|d| d.page_count).sum();
        assert_eq!(pages as u64, ram / ARCBIOS_PAGESIZE as u64);
    }
}

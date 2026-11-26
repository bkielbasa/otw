//! What the firmware's ACPI tables say about the machine: its CPUs (MADT).

use alloc::vec::Vec;

#[derive(Default)]
pub struct Machine {
    /// APIC ids of the CPUs that can run.
    pub cpus: Vec<u32>,
    pub io_apics: Vec<u64>,
}

unsafe fn read<T: Copy>(address: u64) -> T {
    core::ptr::read_unaligned(address as *const T)
}

/// Find the tables from the root pointer (RSDP) the firmware gave.
pub fn parse(rsdp: u64) -> Machine {
    let mut machine = Machine::default();
    if rsdp == 0 {
        return machine;
    }
    unsafe {
        if &read::<[u8; 8]>(rsdp) != b"RSD PTR " {
            return machine;
        }
        let revision: u8 = read(rsdp + 15);
        let (root, entry_size) = if revision >= 2 {
            (read::<u64>(rsdp + 24), 8)
        } else {
            (read::<u32>(rsdp + 16) as u64, 4)
        };
        let length: u32 = read(root + 4);
        let mut at = root + 36;
        while at < root + length as u64 {
            let table = if entry_size == 8 { read::<u64>(at) } else { read::<u32>(at) as u64 };
            at += entry_size;
            if &read::<[u8; 4]>(table) == b"APIC" {
                madt(table, &mut machine);
            }
        }
    }
    machine
}

unsafe fn madt(table: u64, machine: &mut Machine) {
    let length: u32 = read(table + 4);
    let mut at = table + 44;
    while at + 2 <= table + length as u64 {
        let kind: u8 = read(at);
        let size: u8 = read(at + 1);
        if size < 2 {
            break;
        }
        match kind {
            // Local APIC: processor id, APIC id, flags (enabled, or can be).
            0 => {
                let id: u8 = read(at + 3);
                let flags: u32 = read(at + 4);
                if flags & 3 != 0 && !machine.cpus.contains(&(id as u32)) {
                    machine.cpus.push(id as u32);
                }
            }
            1 => machine.io_apics.push(read::<u32>(at + 4) as u64),
            // Local x2APIC: APIC id, flags.
            9 => {
                let id: u32 = read(at + 4);
                let flags: u32 = read(at + 8);
                if flags & 3 != 0 && !machine.cpus.contains(&id) {
                    machine.cpus.push(id);
                }
            }
            _ => {}
        }
        at += size as u64;
    }
}

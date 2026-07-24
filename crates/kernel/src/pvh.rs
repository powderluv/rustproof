//! Minimal PVH boot-protocol parsing (x86-64): read the `hvm_start_info` the loader
//! passed in `%ebx` and extract the physical memory map. Low RAM is identity-mapped, so
//! physical addresses are dereferenced directly.
use abi::{MemoryKind, MemoryRegion};

const PVH_MAGIC: u32 = 0x336e_c578;
/// The boot trampoline identity-maps the low 1 GiB; clip the allocator to it.
pub const IDENTITY_LIMIT: u64 = 0x4000_0000;

#[repr(C)]
struct HvmStartInfo {
    magic: u32,
    version: u32,
    flags: u32,
    nr_modules: u32,
    modlist_paddr: u64,
    cmdline_paddr: u64,
    rsdp_paddr: u64,
    memmap_paddr: u64,
    memmap_entries: u32,
    reserved: u32,
}

#[repr(C)]
struct HvmMemmapEntry {
    addr: u64,
    size: u64,
    typ: u32,
    reserved: u32,
}

fn kind_of(typ: u32) -> MemoryKind {
    match typ {
        1 => MemoryKind::Usable,
        2 => MemoryKind::Reserved,
        3 => MemoryKind::AcpiReclaimable,
        4 => MemoryKind::AcpiNvs,
        _ => MemoryKind::Unusable,
    }
}

/// Parse the PVH memory map into `out`, clipped to the identity window. Returns the count.
pub fn memory_map(start_info: u64, out: &mut [MemoryRegion]) -> usize {
    if start_info == 0 {
        return 0;
    }
    let si = unsafe { &*(start_info as *const HvmStartInfo) };
    if si.magic != PVH_MAGIC || si.version < 1 || si.memmap_paddr == 0 {
        return 0;
    }
    let entries = si.memmap_entries as usize;
    let table = si.memmap_paddr as *const HvmMemmapEntry;
    let mut count = 0;
    for i in 0..entries {
        if count >= out.len() {
            break;
        }
        let e = unsafe { &*table.add(i) };
        let start = e.addr;
        let end = e.addr.saturating_add(e.size).min(IDENTITY_LIMIT);
        if end <= start {
            continue;
        }
        out[count] = MemoryRegion {
            start,
            len: end - start,
            kind: kind_of(e.typ),
        };
        count += 1;
    }
    count
}

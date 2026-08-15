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

/// Largest memory map this will walk, whatever the hypervisor declares.
///
/// `hvm_start_info.memmap_entries` is a `u32` supplied from outside the TCB, and the walk
/// below dereferences `table.add(i)` for each one. The `count >= out.len()` break does NOT
/// bound it: that fires only when an entry is ACCEPTED, so a map whose entries are all
/// rejected (`end <= start`, trivially arranged with zero sizes) walks the full declared
/// count — up to 2^32-1 dereferences marching off the end of any real table.
pub(crate) const MAX_MAP_ENTRIES: usize = 256;

/// Bytes per `HvmMemmapEntry`. Asserted against the real layout below.
const ENTRY_SIZE: u64 = 24;

const _: () = assert!(core::mem::size_of::<HvmMemmapEntry>() as u64 == ENTRY_SIZE);

/// How many map entries it is safe to dereference, given a hypervisor-supplied table address
/// and count.
///
/// Two independent bounds, and neither was present: a hard cap on the COUNT, and the
/// requirement that the whole table lie inside the identity-mapped window the boot trampoline
/// actually established. Dereferencing outside it is not merely a fault — low physical memory
/// holds device MMIO, and a read there can have side effects.
///
/// Pure arithmetic, so it is host-testable; the walk that uses it is not.
pub(crate) fn walkable_entries(memmap_paddr: u64, declared: u32) -> usize {
    if memmap_paddr == 0 || memmap_paddr >= IDENTITY_LIMIT {
        return 0;
    }
    let capped = (declared as usize).min(MAX_MAP_ENTRIES);
    let room = (IDENTITY_LIMIT - memmap_paddr) / ENTRY_SIZE;
    capped.min(room as usize)
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
    let entries = walkable_entries(si.memmap_paddr, si.memmap_entries);
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

/// The ACPI RSDP the hypervisor placed, or `None` if this boot has no ACPI.
///
/// `hvm_start_info` has carried this field since the struct was written and nothing has ever
/// read it. It is the only route to the AMD-Vi register base on a `-kernel`/PVH boot: the
/// unit's PCI capability leaves that base UNPROGRAMMED because no firmware ran, so the address
/// has to come from the IVRS table this pointer leads to.
///
/// # Safety
/// Dereferences the boot-info pointer the loader supplied in `%ebx`, same as [`memory_map`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn rsdp(start_info: u64) -> Option<u64> {
    if start_info == 0 {
        return None;
    }
    let si = &*(start_info as *const HvmStartInfo);
    if si.magic != PVH_MAGIC || si.version < 1 {
        return None;
    }
    // Must be inside the window the trampoline identity-maps, for the same reason the memory
    // map must be: dereferencing outside it is not merely a fault, low physical memory holds
    // device MMIO and a read there can have side effects.
    match si.rsdp_paddr {
        0 => None,
        p if p >= IDENTITY_LIMIT => None,
        p => Some(p),
    }
}

/// The raw `rsdp_paddr` field, for diagnosing why [`rsdp`] refused it.
///
/// # Safety
/// See [`rsdp`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn rsdp_raw(start_info: u64) -> u64 {
    if start_info == 0 {
        return 0;
    }
    let si = &*(start_info as *const HvmStartInfo);
    if si.magic != PVH_MAGIC {
        return 0;
    }
    si.rsdp_paddr
}

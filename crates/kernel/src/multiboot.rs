//! Multiboot 1 boot information — the FIRMWARE boot path.
//!
//! Used instead of [`crate::pvh`] when the image advertises a multiboot header, which is how
//! this nucleus gets SeaBIOS to run first. That matters for exactly one reason: firmware
//! assigns PCI BARs and the PVH path has none, so under PVH every device's registers are
//! unreachable (measured: `edu` reports `BAR0 at 0xffffffffffffffff` under PVH,
//! `0xfea00000` after SeaBIOS).
//!
//! The two protocols agree by accident on the only thing the trampoline needs — 32-bit
//! protected mode with the info pointer in `%ebx` — so `_start` is shared and only the parsing
//! differs.

use abi::{MemoryKind, MemoryRegion};

/// `%eax` at entry when a multiboot loader jumped to us.
pub const BOOTLOADER_MAGIC: u32 = 0x2BAD_B002;

/// Same identity-map bound the PVH parser uses, and for the same reason: a pointer outside it
/// is not merely a fault, low physical memory holds device MMIO where a read has side effects.
pub const IDENTITY_LIMIT: u64 = 0x4000_0000;

/// Largest memory map this will walk, whatever the loader declares.
const MAX_ENTRIES: usize = 256;

/// Multiboot memory types. 1 is available; everything else is not ours to allocate.
fn kind_of(typ: u32) -> MemoryKind {
    match typ {
        1 => MemoryKind::Usable,
        3 => MemoryKind::AcpiReclaimable,
        4 => MemoryKind::AcpiNvs,
        5 => MemoryKind::Unusable,
        _ => MemoryKind::Reserved,
    }
}

/// How many map entries it is safe to walk, given the loader's declared extent.
///
/// The same bound `pvh::walkable_entries` applies, for the same reason: the count and the
/// address both come from outside the TCB. Multiboot describes the map as a byte LENGTH rather
/// than an entry count, and entries are variable-length, so this bounds the BYTES and the walk
/// itself stops on a malformed entry size.
pub fn walkable_bytes(mmap_addr: u64, mmap_length: u32) -> usize {
    if mmap_addr == 0 || mmap_addr >= IDENTITY_LIMIT {
        return 0;
    }
    let room = IDENTITY_LIMIT - mmap_addr;
    (mmap_length as u64).min(room) as usize
}

/// Decode one multiboot mmap entry from `buf`, returning the region and its total size.
///
/// Entry layout: `size(u32) base(u64) len(u64) type(u32)`, where `size` EXCLUDES itself — so a
/// walk advances by `size + 4`. Getting that wrong walks off by four bytes per entry and
/// produces plausible garbage rather than an obvious failure, which is why it is a named
/// function with tests rather than arithmetic inline in a loop.
pub fn parse_entry(buf: &[u8]) -> Option<(MemoryRegion, usize)> {
    if buf.len() < 24 {
        return None;
    }
    let size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    // A zero (or absurd) size would spin the walk forever on a malformed map.
    if size < 20 {
        return None;
    }
    let mut base = 0u64;
    for i in (0..8).rev() {
        base = (base << 8) | buf[4 + i] as u64;
    }
    let mut len = 0u64;
    for i in (0..8).rev() {
        len = (len << 8) | buf[12 + i] as u64;
    }
    let typ = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    Some((
        MemoryRegion {
            start: base,
            len,
            kind: kind_of(typ),
        },
        size + 4,
    ))
}

/// Parse the multiboot memory map into `out`, clipped to the identity window.
///
/// # Safety
/// Dereferences the boot-info pointer the loader supplied in `%ebx`.
pub unsafe fn memory_map(info: u64, out: &mut [MemoryRegion]) -> usize {
    if info == 0 || info >= IDENTITY_LIMIT {
        return 0;
    }
    let flags = core::ptr::read_volatile(info as *const u32);
    // Bit 6 says the mmap fields are valid. Without it, offsets 44/48 are not a map.
    if flags & (1 << 6) == 0 {
        return 0;
    }
    let mmap_length = core::ptr::read_volatile((info + 44) as *const u32);
    let mmap_addr = core::ptr::read_volatile((info + 48) as *const u32) as u64;
    let bytes = walkable_bytes(mmap_addr, mmap_length);
    if bytes == 0 {
        return 0;
    }
    let map = core::slice::from_raw_parts(mmap_addr as *const u8, bytes);

    let mut off = 0usize;
    let mut count = 0usize;
    let mut seen = 0usize;
    while off < map.len() && count < out.len() && seen < MAX_ENTRIES {
        seen += 1;
        let Some((mut r, step)) = parse_entry(&map[off..]) else {
            break;
        };
        off += step;
        // Clip to the identity window, exactly as the PVH parser does.
        let end = r.start.saturating_add(r.len).min(IDENTITY_LIMIT);
        if end <= r.start {
            continue;
        }
        r.len = end - r.start;
        out[count] = r;
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(base: u64, len: u64, typ: u32) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&20u32.to_le_bytes()); // size EXCLUDES itself
        v.extend_from_slice(&base.to_le_bytes());
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(&typ.to_le_bytes());
        v
    }

    /// The `size` field excludes itself, so a walk advances by `size + 4`. An off-by-four here
    /// yields plausible garbage rather than an obvious failure.
    #[test]
    fn an_entry_advances_by_its_size_plus_four() {
        let e = entry(0x10_0000, 0x1000, 1);
        let (r, step) = parse_entry(&e).expect("well-formed entry");
        assert_eq!(step, 24, "size(20) + the 4 bytes size does not count");
        assert_eq!(r.start, 0x10_0000);
        assert_eq!(r.len, 0x1000);
        assert_eq!(r.kind, MemoryKind::Usable);
    }

    #[test]
    fn only_type_1_is_usable() {
        for (t, k) in [
            (1u32, MemoryKind::Usable),
            (2, MemoryKind::Reserved),
            (3, MemoryKind::AcpiReclaimable),
            (4, MemoryKind::AcpiNvs),
            (5, MemoryKind::Unusable),
            (99, MemoryKind::Reserved),
        ] {
            let e = entry(0, 0x1000, t);
            assert_eq!(parse_entry(&e).unwrap().0.kind, k, "type {t}");
        }
    }

    /// The map comes from outside the TCB. A zero-size entry would spin a naive walk forever —
    /// a boot hang with no output, the same hazard the IVRS walk had.
    #[test]
    fn a_zero_size_entry_is_refused_rather_than_looped_on() {
        let mut e = entry(0, 0x1000, 1);
        e[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(
            parse_entry(&e).is_none(),
            "a zero-size entry must be refused"
        );
        assert!(
            parse_entry(&[]).is_none(),
            "an empty buffer must be refused"
        );
        assert!(
            parse_entry(&e[..10]).is_none(),
            "a truncated entry must be refused"
        );
    }

    #[test]
    fn the_map_extent_is_bounded_to_the_identity_window() {
        assert_eq!(walkable_bytes(0, 100), 0);
        assert_eq!(walkable_bytes(IDENTITY_LIMIT, 100), 0);
        assert_eq!(walkable_bytes(IDENTITY_LIMIT + 1, 100), 0);
        // Starts inside but runs past the end: clipped, not accepted whole.
        assert_eq!(walkable_bytes(IDENTITY_LIMIT - 40, 100), 40);
        assert_eq!(walkable_bytes(0x1000, 100), 100);
    }
}

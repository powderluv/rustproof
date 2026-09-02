#![cfg_attr(not(test), no_std)]

//! loader-riscv — a minimal *static* ELF64 loader for RISC-V (Sv39).
//!
//! [`load_elf`] parses an ET_EXEC RISC-V ELF64 image and maps every `PT_LOAD`
//! segment into a caller-supplied [`vspace_riscv::AddressSpace`], allocating leaf
//! frames from an [`abi::FrameAllocator`]. Each segment is materialized page-by-page
//! at 4 KiB granularity: a fresh frame is zeroed, the file-backed bytes are copied
//! in, and the tail past `p_filesz` stays zero (`.bss`). No dynamic linking, no
//! relocations, no interpreter — the entry point is returned verbatim.
//!
//! This is the exact analog of the x86-64 `loader` crate. The only substantive
//! differences are the machine check (`EM_RISCV` instead of `EM_X86_64`) and the
//! leaf-permission encoding: Sv39 has no negative "no-execute" bit, so `X` is a
//! *positive* permission (set only for executable segments) and every leaf carries
//! `V | R | U` — valid, readable, user-accessible.
//!
//! Frame contents are written through the same physical→virtual window
//! `vspace_riscv` uses to reach page-table frames: byte `b` of physical frame `f`
//! lives at host virtual address `f + aspace.phys_offset()` (identity when
//! `phys_offset == 0`).

use abi::{FrameAllocator, VirtAddr, PAGE_SIZE};
use vspace_riscv::{AddressSpace, PageFlags};

// ------------------------------------------------------------- ELF64 header layout
// Field byte-offsets into the 64-byte Elf64_Ehdr (little-endian, LP64).

/// `e_ident` magic: 0x7F 'E' 'L' 'F'.
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
/// `e_ident[EI_CLASS]` — 2 == ELFCLASS64.
const EI_CLASS: usize = 4;
const ELFCLASS64: u8 = 2;
/// `e_type` (u16) — 2 == ET_EXEC.
const OFF_E_TYPE: usize = 16;
const ET_EXEC: u16 = 2;
/// `e_machine` (u16) — 0xF3 (243) == EM_RISCV.
const OFF_E_MACHINE: usize = 18;
const EM_RISCV: u16 = 0xF3;
/// `e_entry` (u64).
const OFF_E_ENTRY: usize = 24;
/// `e_phoff` (u64) — program-header table file offset.
const OFF_E_PHOFF: usize = 32;
/// `e_phentsize` (u16) — size of one program header (stride).
const OFF_E_PHENTSIZE: usize = 54;
/// `e_phnum` (u16) — number of program headers.
const OFF_E_PHNUM: usize = 56;
/// Minimum size of a valid Elf64_Ehdr.
const EHDR_SIZE: usize = 64;

// -------------------------------------------------------- Elf64_Phdr field offsets
// Byte-offsets into a 56-byte Elf64_Phdr.

const PHDR_SIZE: usize = 56;
const OFF_P_TYPE: usize = 0; // u32
const OFF_P_FLAGS: usize = 4; // u32
const OFF_P_OFFSET: usize = 8; // u64
const OFF_P_VADDR: usize = 16; // u64
const OFF_P_FILESZ: usize = 32; // u64
const OFF_P_MEMSZ: usize = 40; // u64

/// Loadable segment.
const PT_LOAD: u32 = 1;
/// `p_flags` bits.
const PF_X: u32 = 0x1;
const PF_W: u32 = 0x2;

// ------------------------------------------------------------------------ API types

/// A successfully loaded image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Loaded {
    /// The virtual entry point (`e_entry`), to hand to the thread that runs it.
    pub entry: VirtAddr,
}

/// Why [`load_elf`] rejected or failed on an image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadError {
    /// `e_ident` did not begin with the ELF magic.
    BadMagic,
    /// `EI_CLASS` was not ELFCLASS64.
    Not64,
    /// `e_type` was not ET_EXEC.
    NotExec,
    /// `e_machine` was not EM_RISCV.
    BadMachine,
    /// A header field or file-backed byte range fell outside the image.
    Truncated,
    /// The frame allocator ran out of frames for a segment page.
    OutOfFrames,
    /// [`AddressSpace::map`] failed (e.g. out of frames for an intermediate table,
    /// or a conflicting existing mapping).
    MapFailed,
}

// ----------------------------------------------------------- little-endian readers
// Each returns `Truncated` if the requested field is not wholly inside `b`.

#[inline]
fn rd_u16(b: &[u8], off: usize) -> Result<u16, LoadError> {
    let end = off.checked_add(2).ok_or(LoadError::Truncated)?;
    let s = b.get(off..end).ok_or(LoadError::Truncated)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
fn rd_u32(b: &[u8], off: usize) -> Result<u32, LoadError> {
    let end = off.checked_add(4).ok_or(LoadError::Truncated)?;
    let s = b.get(off..end).ok_or(LoadError::Truncated)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[inline]
fn rd_u64(b: &[u8], off: usize) -> Result<u64, LoadError> {
    let end = off.checked_add(8).ok_or(LoadError::Truncated)?;
    let s = b.get(off..end).ok_or(LoadError::Truncated)?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

// ----------------------------------------------------------------------- load_elf

/// Parse `elf` and map every `PT_LOAD` segment into `aspace`, drawing leaf frames
/// from `fa`. Returns the entry point on success.
///
// PROOF(later): every page mapped by this loader lies within some `PT_LOAD`
// segment's `[p_vaddr, p_vaddr + p_memsz)` virtual range — the loader never maps
// a page the ELF did not ask for.
pub fn load_elf(
    elf: &[u8],
    aspace: &mut AddressSpace,
    fa: &mut dyn FrameAllocator,
) -> Result<Loaded, LoadError> {
    // ---- validate the Elf64 header -------------------------------------------
    if elf.len() < EHDR_SIZE {
        return Err(LoadError::Truncated);
    }
    if elf[0] != ELF_MAGIC[0]
        || elf[1] != ELF_MAGIC[1]
        || elf[2] != ELF_MAGIC[2]
        || elf[3] != ELF_MAGIC[3]
    {
        return Err(LoadError::BadMagic);
    }
    if elf[EI_CLASS] != ELFCLASS64 {
        return Err(LoadError::Not64);
    }
    if rd_u16(elf, OFF_E_MACHINE)? != EM_RISCV {
        return Err(LoadError::BadMachine);
    }
    if rd_u16(elf, OFF_E_TYPE)? != ET_EXEC {
        return Err(LoadError::NotExec);
    }

    let e_entry = rd_u64(elf, OFF_E_ENTRY)?;
    let e_phoff = rd_u64(elf, OFF_E_PHOFF)?;
    let e_phentsize = rd_u16(elf, OFF_E_PHENTSIZE)? as u64;
    let e_phnum = rd_u16(elf, OFF_E_PHNUM)?;

    // ---- walk the program-header table ---------------------------------------
    for i in 0..e_phnum as u64 {
        // ph_off = e_phoff + i * e_phentsize, entirely in bounds.
        let stride = i.checked_mul(e_phentsize).ok_or(LoadError::Truncated)?;
        let ph_off = e_phoff.checked_add(stride).ok_or(LoadError::Truncated)?;
        let ph_off: usize = ph_off.try_into().map_err(|_| LoadError::Truncated)?;
        // Guarding the whole 56-byte phdr keeps every field read (and every
        // `ph_off + FIELD` addition below) in range and overflow-free.
        let ph_end = ph_off.checked_add(PHDR_SIZE).ok_or(LoadError::Truncated)?;
        if ph_end > elf.len() {
            return Err(LoadError::Truncated);
        }

        if rd_u32(elf, ph_off + OFF_P_TYPE)? != PT_LOAD {
            continue;
        }
        let p_flags = rd_u32(elf, ph_off + OFF_P_FLAGS)?;
        let p_offset = rd_u64(elf, ph_off + OFF_P_OFFSET)?;
        let p_vaddr = rd_u64(elf, ph_off + OFF_P_VADDR)?;
        let p_filesz = rd_u64(elf, ph_off + OFF_P_FILESZ)?;
        let p_memsz = rd_u64(elf, ph_off + OFF_P_MEMSZ)?;

        load_segment(
            elf, aspace, fa, p_flags, p_offset, p_vaddr, p_filesz, p_memsz,
        )?;
    }

    Ok(Loaded {
        entry: VirtAddr(e_entry),
    })
}

/// Materialize one `PT_LOAD` segment into `aspace`, page by page.
#[allow(clippy::too_many_arguments)]
fn load_segment(
    elf: &[u8],
    aspace: &mut AddressSpace,
    fa: &mut dyn FrameAllocator,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_filesz: u64,
    p_memsz: u64,
) -> Result<(), LoadError> {
    if p_memsz == 0 {
        return Ok(());
    }

    // Virtual extent of the segment, and the virtual point past which everything
    // is `.bss` (zero). Clamp the file-backed window to the mapped extent so a
    // malformed `p_filesz > p_memsz` can never copy past the segment.
    let seg_end_va = p_vaddr.checked_add(p_memsz).ok_or(LoadError::Truncated)?;
    let file_end_va = p_vaddr.checked_add(p_filesz).ok_or(LoadError::Truncated)?;
    let copy_end_va = file_end_va.min(seg_end_va);

    // Leaf permissions (Sv39): every user page is valid + readable + user
    // (`V | R | U`). RISC-V has no negative no-execute bit, so `X` is a positive
    // permission added only for executable segments (`PF_X`); non-executable data
    // simply omits it. `W` is added for writable segments (`PF_W`).
    let mut flags = PageFlags::V | PageFlags::R | PageFlags::U;
    if p_flags & PF_W != 0 {
        flags = flags | PageFlags::W;
    }
    if p_flags & PF_X != 0 {
        flags = flags | PageFlags::X;
    }

    // Iterate the 4 KiB pages covering [p_vaddr, seg_end_va), starting at the page
    // that contains p_vaddr (which may itself be unaligned).
    let mut page_va = p_vaddr & !(PAGE_SIZE - 1);
    while page_va < seg_end_va {
        let frame = fa.alloc_frame().ok_or(LoadError::OutOfFrames)?;

        // Reach the frame's bytes through the physical→virtual window.
        //
        // SAFETY: `frame` is a live 4 KiB physical frame just handed out by `fa`
        // and not yet mapped anywhere, so nothing else aliases it. It is reachable
        // in the current address space at `frame + phys_offset` — the identical
        // window `vspace_riscv` uses to reach page-table frames. We form a slice of
        // exactly PAGE_SIZE bytes and touch only those.
        let dst = aspace.phys_offset().wrapping_add(frame.as_u64()) as *mut u8;
        let page = unsafe { core::slice::from_raw_parts_mut(dst, PAGE_SIZE as usize) };
        page.fill(0);

        // The page's virtual span is [page_va, page_end); the file-backed portion
        // is its intersection with [p_vaddr, copy_end_va).
        let page_end = page_va.saturating_add(PAGE_SIZE);
        let copy_va_start = page_va.max(p_vaddr);
        let copy_va_end = page_end.min(copy_end_va);
        if copy_va_start < copy_va_end {
            let intra = (copy_va_start - page_va) as usize; // in-page dst offset
            let len = (copy_va_end - copy_va_start) as usize;
            // File offset of the first copied byte: p_offset advances 1:1 with the
            // virtual address inside the segment (copy_va_start >= p_vaddr).
            let file_start = p_offset
                .checked_add(copy_va_start - p_vaddr)
                .ok_or(LoadError::Truncated)?;
            let file_start: usize = file_start.try_into().map_err(|_| LoadError::Truncated)?;
            let file_end = file_start.checked_add(len).ok_or(LoadError::Truncated)?;
            let src = elf.get(file_start..file_end).ok_or(LoadError::Truncated)?;
            page[intra..intra + len].copy_from_slice(src);
        }

        aspace
            .map(VirtAddr(page_va), frame, flags, fa)
            .map_err(|_| LoadError::MapFailed)?;

        match page_va.checked_add(PAGE_SIZE) {
            Some(next) => page_va = next,
            None => break, // segment runs to the top of the address space
        }
    }

    Ok(())
}

// ===================================================================== tests ===

#[cfg(test)]
mod tests {
    use super::*;
    use abi::{FrameAllocator, PhysAddr, VirtAddr, PAGE_SIZE};

    // ---- mock frame allocator + address space (mirrors vspace-riscv's tests) --

    #[derive(Clone, Copy)]
    #[repr(C, align(4096))]
    struct Frame([u8; 4096]);

    /// Bump allocator over a `Vec` of page-aligned frames. Frame `i` has physical
    /// address `i * PAGE_SIZE`, reachable at `i*PAGE_SIZE + base` where `base` is
    /// the storage's virtual base — a faithful identity-offset window.
    struct MockAlloc {
        frames: Vec<Frame>,
        next: usize,
        base: u64,
    }

    impl MockAlloc {
        fn new(capacity: usize) -> MockAlloc {
            let mut frames = Vec::with_capacity(capacity);
            for _ in 0..capacity {
                frames.push(Frame([0u8; 4096]));
            }
            // Exactly `capacity` pushes into a `with_capacity` Vec ⇒ no realloc,
            // so the base pointer is stable for the allocator's lifetime.
            let base = frames.as_mut_ptr() as u64;
            MockAlloc {
                frames,
                next: 0,
                base,
            }
        }

        fn phys_offset(&self) -> u64 {
            self.base
        }
    }

    impl FrameAllocator for MockAlloc {
        fn alloc_frame(&mut self) -> Option<PhysAddr> {
            if self.next >= self.frames.len() {
                return None;
            }
            let i = self.next;
            self.next += 1;
            Some(PhysAddr((i as u64) * PAGE_SIZE))
        }

        fn free_frame(&mut self, _frame: PhysAddr) {}
    }

    /// Read one byte of the frame currently mapped at virtual address `va`.
    fn peek(aspace: &AddressSpace, phys_offset: u64, va: u64) -> u8 {
        let pa = aspace.translate(VirtAddr(va)).expect("va should be mapped");
        // SAFETY: `pa` names a live mapped frame reachable at `pa + phys_offset`.
        unsafe { *((phys_offset + pa.as_u64()) as *const u8) }
    }

    // ---- test ELF builder -----------------------------------------------------

    struct Seg {
        vaddr: u64,
        offset: u64,
        filesz: u64,
        memsz: u64,
        flags: u32,
    }

    /// Build a valid one-`PT_LOAD` ET_EXEC RISC-V ELF64 image with `code` placed at
    /// `seg.offset`. The program-header table sits immediately after the 64-byte
    /// header (`e_phoff == 64`).
    fn make_elf(entry: u64, seg: &Seg, code: &[u8]) -> Vec<u8> {
        let phoff: u64 = 64;
        let phentsize: u16 = PHDR_SIZE as u16;
        let need = (seg.offset as usize) + code.len();
        let total = need.max(EHDR_SIZE + PHDR_SIZE);
        let mut v = vec![0u8; total];

        // e_ident
        v[0..4].copy_from_slice(&ELF_MAGIC);
        v[EI_CLASS] = ELFCLASS64;
        v[5] = 1; // EI_DATA = ELFDATA2LSB
        v[6] = 1; // EI_VERSION = EV_CURRENT

        v[OFF_E_TYPE..OFF_E_TYPE + 2].copy_from_slice(&ET_EXEC.to_le_bytes());
        v[OFF_E_MACHINE..OFF_E_MACHINE + 2].copy_from_slice(&EM_RISCV.to_le_bytes());
        v[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        v[OFF_E_ENTRY..OFF_E_ENTRY + 8].copy_from_slice(&entry.to_le_bytes());
        v[OFF_E_PHOFF..OFF_E_PHOFF + 8].copy_from_slice(&phoff.to_le_bytes());
        v[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes()); // e_ehsize
        v[OFF_E_PHENTSIZE..OFF_E_PHENTSIZE + 2].copy_from_slice(&phentsize.to_le_bytes());
        v[OFF_E_PHNUM..OFF_E_PHNUM + 2].copy_from_slice(&1u16.to_le_bytes());

        // one Elf64_Phdr at offset 64
        let p = 64usize;
        v[p + OFF_P_TYPE..p + OFF_P_TYPE + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        v[p + OFF_P_FLAGS..p + OFF_P_FLAGS + 4].copy_from_slice(&seg.flags.to_le_bytes());
        v[p + OFF_P_OFFSET..p + OFF_P_OFFSET + 8].copy_from_slice(&seg.offset.to_le_bytes());
        v[p + OFF_P_VADDR..p + OFF_P_VADDR + 8].copy_from_slice(&seg.vaddr.to_le_bytes());
        v[p + 24..p + 32].copy_from_slice(&seg.vaddr.to_le_bytes()); // p_paddr = p_vaddr
        v[p + OFF_P_FILESZ..p + OFF_P_FILESZ + 8].copy_from_slice(&seg.filesz.to_le_bytes());
        v[p + OFF_P_MEMSZ..p + OFF_P_MEMSZ + 8].copy_from_slice(&seg.memsz.to_le_bytes());
        v[p + 48..p + 56].copy_from_slice(&PAGE_SIZE.to_le_bytes()); // p_align

        v[seg.offset as usize..seg.offset as usize + code.len()].copy_from_slice(code);
        v
    }

    // ---- (a)+(b)+(c): load, map, read back, and bss zero-fill -----------------

    #[test]
    fn loads_maps_and_zero_fills_bss() {
        let entry = 0x40_0000u64;
        let code = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66]; // 6 file bytes
        let seg = Seg {
            vaddr: 0x40_0000,
            offset: 0x1000,
            filesz: code.len() as u64,
            memsz: 0x1000 + 0x10, // spills into a second page ⇒ .bss
            flags: PF_X | 0x4,    // R+X, no write
        };
        let img = make_elf(entry, &seg, &code);

        let mut fa = MockAlloc::new(32);
        let phys_offset = fa.phys_offset();
        let mut aspace = AddressSpace::create(phys_offset, &mut fa).unwrap();

        let loaded = load_elf(&img, &mut aspace, &mut fa).expect("load ok");

        // (a) entry point round-trips.
        assert_eq!(loaded.entry, VirtAddr(entry));

        // (b) file bytes landed at the segment vaddr.
        for (i, &b) in code.iter().enumerate() {
            assert_eq!(peek(&aspace, phys_offset, 0x40_0000 + i as u64), b);
        }

        // (c) tail of the first page past p_filesz is zero (.bss)...
        assert_eq!(peek(&aspace, phys_offset, 0x40_0000 + code.len() as u64), 0);
        assert_eq!(peek(&aspace, phys_offset, 0x40_0000 + 0xFFF), 0);
        // ...and the entire second (bss) page is present and zero.
        assert_eq!(peek(&aspace, phys_offset, 0x40_1000), 0);
        assert_eq!(peek(&aspace, phys_offset, 0x40_1000 + 0xF), 0);

        // Nothing mapped beyond the segment's memsz page span.
        assert_eq!(aspace.translate(VirtAddr(0x40_2000)), None);
    }

    // ---- intra-page offset handling for an unaligned segment ------------------

    #[test]
    fn loads_unaligned_segment() {
        let entry = 0x40_0040u64;
        let code = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let seg = Seg {
            vaddr: 0x40_0040, // 0x40 into the page
            offset: 0x1040,   // congruent file offset
            filesz: code.len() as u64,
            memsz: code.len() as u64,
            flags: PF_X | 0x4,
        };
        let img = make_elf(entry, &seg, &code);

        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let mut aspace = AddressSpace::create(phys_offset, &mut fa).unwrap();

        load_elf(&img, &mut aspace, &mut fa).expect("load ok");

        // Bytes before the intra-page offset stay zero.
        assert_eq!(peek(&aspace, phys_offset, 0x40_0000), 0);
        assert_eq!(peek(&aspace, phys_offset, 0x40_003F), 0);
        // Code landed at the unaligned vaddr.
        for (i, &b) in code.iter().enumerate() {
            assert_eq!(peek(&aspace, phys_offset, 0x40_0040 + i as u64), b);
        }
    }

    // ---- (d): header rejections ----------------------------------------------

    fn valid_image() -> Vec<u8> {
        let seg = Seg {
            vaddr: 0x40_0000,
            offset: 0x1000,
            filesz: 4,
            memsz: 4,
            flags: PF_X | 0x4,
        };
        make_elf(0x40_0000, &seg, &[1, 2, 3, 4])
    }

    fn fresh_space() -> (MockAlloc, AddressSpace) {
        let mut fa = MockAlloc::new(8);
        let phys_offset = fa.phys_offset();
        let aspace = AddressSpace::create(phys_offset, &mut fa).unwrap();
        (fa, aspace)
    }

    #[test]
    fn rejects_bad_magic() {
        let mut img = valid_image();
        img[1] = b'X'; // corrupt magic
        let (mut fa, mut aspace) = fresh_space();
        assert_eq!(
            load_elf(&img, &mut aspace, &mut fa),
            Err(LoadError::BadMagic)
        );
    }

    #[test]
    fn rejects_not_64() {
        let mut img = valid_image();
        img[EI_CLASS] = 1; // ELFCLASS32
        let (mut fa, mut aspace) = fresh_space();
        assert_eq!(load_elf(&img, &mut aspace, &mut fa), Err(LoadError::Not64));
    }

    #[test]
    fn rejects_not_exec() {
        let mut img = valid_image();
        img[OFF_E_TYPE..OFF_E_TYPE + 2].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
        let (mut fa, mut aspace) = fresh_space();
        assert_eq!(
            load_elf(&img, &mut aspace, &mut fa),
            Err(LoadError::NotExec)
        );
    }

    #[test]
    fn rejects_bad_machine() {
        let mut img = valid_image();
        // 0x3E == EM_X86_64: a valid ELF64 exec, but the wrong architecture.
        img[OFF_E_MACHINE..OFF_E_MACHINE + 2].copy_from_slice(&0x3Eu16.to_le_bytes());
        let (mut fa, mut aspace) = fresh_space();
        assert_eq!(
            load_elf(&img, &mut aspace, &mut fa),
            Err(LoadError::BadMachine)
        );
    }

    #[test]
    fn rejects_truncated_header() {
        let img = valid_image();
        let (mut fa, mut aspace) = fresh_space();
        assert_eq!(
            load_elf(&img[..EHDR_SIZE - 1], &mut aspace, &mut fa),
            Err(LoadError::Truncated)
        );
    }

    #[test]
    fn rejects_truncated_segment_data() {
        // filesz claims bytes past the end of the image.
        let seg = Seg {
            vaddr: 0x40_0000,
            offset: 0x1000,
            filesz: 64, // but only 4 code bytes exist
            memsz: 64,
            flags: PF_X | 0x4,
        };
        let img = make_elf(0x40_0000, &seg, &[1, 2, 3, 4]);
        let (mut fa, mut aspace) = fresh_space();
        assert_eq!(
            load_elf(&img, &mut aspace, &mut fa),
            Err(LoadError::Truncated)
        );
    }

    #[test]
    fn reports_map_failure_when_frames_exhausted() {
        // create() takes 1 frame (the Sv39 root); a two-page segment needs data
        // frames + intermediate tables. Capacity 2 leaves one frame after the
        // root: it becomes the first data page, then AddressSpace::map runs out of
        // frames for an intermediate (level-1/level-0) table → MapFailed.
        let seg = Seg {
            vaddr: 0x40_0000,
            offset: 0x1000,
            filesz: 4,
            memsz: 0x2000,
            flags: PF_W | 0x4,
        };
        let img = make_elf(0x40_0000, &seg, &[1, 2, 3, 4]);
        let mut fa = MockAlloc::new(2);
        let phys_offset = fa.phys_offset();
        let mut aspace = AddressSpace::create(phys_offset, &mut fa).unwrap();
        assert_eq!(
            load_elf(&img, &mut aspace, &mut fa),
            Err(LoadError::MapFailed)
        );
    }

    #[test]
    fn reports_out_of_frames_for_data_page() {
        // Only the root frame exists (capacity 1); the first data-page allocation
        // returns None → OutOfFrames.
        let seg = Seg {
            vaddr: 0x40_0000,
            offset: 0x1000,
            filesz: 4,
            memsz: 4,
            flags: PF_W | 0x4,
        };
        let img = make_elf(0x40_0000, &seg, &[1, 2, 3, 4]);
        let mut fa = MockAlloc::new(1);
        let phys_offset = fa.phys_offset();
        let mut aspace = AddressSpace::create(phys_offset, &mut fa).unwrap();
        assert_eq!(
            load_elf(&img, &mut aspace, &mut fa),
            Err(LoadError::OutOfFrames)
        );
    }

    #[test]
    fn writable_segment_loads_and_reads_back() {
        let seg = Seg {
            vaddr: 0x40_0000,
            offset: 0x1000,
            filesz: 4,
            memsz: 4,
            flags: PF_W | 0x4, // RW, not executable
        };
        let img = make_elf(0x40_0000, &seg, &[9, 8, 7, 6]);
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let mut aspace = AddressSpace::create(phys_offset, &mut fa).unwrap();
        load_elf(&img, &mut aspace, &mut fa).expect("load ok");
        // A successful translate + read confirms the leaf is present.
        assert_eq!(peek(&aspace, phys_offset, 0x40_0000), 9);
    }

    /// These four mirror `crates/loader`. The riscv side had the same GUARDS -- they were
    /// kept in sync -- but none of the tests, so nothing had ever asked whether its
    /// truncated-header rejection, its empty-segment early return or its W^X mapping
    /// could fail. The mutation table reached only x86 until now.
    #[test]
    fn rejects_a_program_header_running_past_the_image() {
        let mut img = vec![0u8; EHDR_SIZE + PHDR_SIZE + 4];
        img[0..4].copy_from_slice(&ELF_MAGIC);
        img[EI_CLASS] = ELFCLASS64;
        img[5] = 1;
        img[6] = 1;
        img[OFF_E_TYPE..OFF_E_TYPE + 2].copy_from_slice(&ET_EXEC.to_le_bytes());
        img[OFF_E_MACHINE..OFF_E_MACHINE + 2].copy_from_slice(&EM_RISCV.to_le_bytes());
        img[20..24].copy_from_slice(&1u32.to_le_bytes());
        img[OFF_E_ENTRY..OFF_E_ENTRY + 8].copy_from_slice(&0x1000u64.to_le_bytes());
        img[OFF_E_PHOFF..OFF_E_PHOFF + 8].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
        img[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
        img[OFF_E_PHENTSIZE..OFF_E_PHENTSIZE + 2]
            .copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        // TWO headers declared; the image holds one and four bytes of the next.
        img[OFF_E_PHNUM..OFF_E_PHNUM + 2].copy_from_slice(&2u16.to_le_bytes());

        let p = EHDR_SIZE;
        img[p + OFF_P_TYPE..p + OFF_P_TYPE + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        img[p + OFF_P_FLAGS..p + OFF_P_FLAGS + 4].copy_from_slice(&4u32.to_le_bytes()); // R
        img[p + OFF_P_OFFSET..p + OFF_P_OFFSET + 8].copy_from_slice(&0u64.to_le_bytes());
        img[p + OFF_P_VADDR..p + OFF_P_VADDR + 8].copy_from_slice(&0x1000u64.to_le_bytes());
        img[p + 24..p + 32].copy_from_slice(&0x1000u64.to_le_bytes());
        img[p + OFF_P_FILESZ..p + OFF_P_FILESZ + 8].copy_from_slice(&0u64.to_le_bytes());
        img[p + OFF_P_MEMSZ..p + OFF_P_MEMSZ + 8].copy_from_slice(&PAGE_SIZE.to_le_bytes());
        img[p + 48..p + 56].copy_from_slice(&PAGE_SIZE.to_le_bytes());

        // The four readable bytes of the second header say "not PT_LOAD" -- exactly the
        // value that gets skipped rather than rejected once the guard is gone.
        let q = EHDR_SIZE + PHDR_SIZE;
        img[q..q + 4].copy_from_slice(&(PT_LOAD + 1).to_le_bytes());

        let (mut fa, mut aspace) = fresh_space();
        assert_eq!(
            load_elf(&img, &mut aspace, &mut fa),
            Err(LoadError::Truncated),
            "a header declared but not present must be rejected, not skipped"
        );
    }

    /// On riscv permissions are POSITIVE (`R`/`W`/`X` bits present on the leaf), where x86
    /// spells the same policy as an absence (`NO_EXEC`). Both are read back off the leaf
    /// the loader actually installed -- `translate` returns a PhysAddr and discards them.
    fn leaf_of(img: &[u8], va: u64) -> PageFlags {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let mut aspace = AddressSpace::create(phys_offset, &mut fa).unwrap();
        load_elf(img, &mut aspace, &mut fa).expect("load ok");
        aspace
            .leaf_flags(VirtAddr(va))
            .expect("segment should be mapped")
    }

    #[test]
    fn a_code_segment_is_executable_and_not_writable() {
        let seg = Seg {
            vaddr: 0x40_0000,
            offset: 0x1000,
            filesz: 4,
            memsz: 4,
            flags: PF_X | 0x4, // R+X, no write
        };
        let img = make_elf(0x40_0000, &seg, &[1, 2, 3, 4]);
        let f = leaf_of(&img, 0x40_0000);
        assert!(f.contains(PageFlags::V) && f.contains(PageFlags::U));
        assert!(
            f.contains(PageFlags::X),
            "a PF_X segment must be mapped executable"
        );
        assert!(
            !f.contains(PageFlags::W),
            "a segment without PF_W must not be writable -- W^X is the whole point"
        );
    }

    #[test]
    fn a_data_segment_is_writable_and_not_executable() {
        let seg = Seg {
            vaddr: 0x40_0000,
            offset: 0x1000,
            filesz: 4,
            memsz: 4,
            flags: PF_W | 0x4, // RW, not executable
        };
        let img = make_elf(0x40_0000, &seg, &[9, 8, 7, 6]);
        let f = leaf_of(&img, 0x40_0000);
        assert!(f.contains(PageFlags::W), "a PF_W segment must be writable");
        assert!(
            !f.contains(PageFlags::X),
            "a segment without PF_X must not be executable, or a data page is a code page"
        );
    }

    #[test]
    fn a_zero_length_segment_at_an_unaligned_address_maps_nothing() {
        // `p_memsz == 0` returns early. Without it the page walk starts at
        // `p_vaddr & !0xfff`, which for an UNALIGNED vaddr is strictly below `seg_end_va`
        // -- so the loop runs once and an empty segment quietly gets a frame and a mapping.
        // Aligned vaddrs cannot see this: there the loop is empty either way.
        let seg = Seg {
            vaddr: 0x40_0004,
            offset: 0x1000,
            filesz: 0,
            memsz: 0,
            flags: PF_W | 0x4,
        };
        let img = make_elf(0x40_0004, &seg, &[]);
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let mut aspace = AddressSpace::create(phys_offset, &mut fa).unwrap();
        let before = fa.next;
        load_elf(&img, &mut aspace, &mut fa).expect("load ok");
        assert_eq!(
            aspace.translate(VirtAddr(0x40_0000)),
            None,
            "an empty segment must not map the page its unaligned vaddr sits in"
        );
        assert_eq!(fa.next, before, "an empty segment must not consume a frame");
    }
}

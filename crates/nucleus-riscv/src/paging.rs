//! Kernel Sv39 identity map (RV-M1).
use abi::{FrameAllocator, PhysAddr};
use vspace_riscv::{PageFlags, PageTable, Pte};

/// Build a 3 GiB identity map (0..3 GiB) using three 1 GiB gigapage leaves — enough to
/// cover the low MMIO devices (UART, test finisher, PLIC/CLINT) and all of RAM
/// (0x8000_0000..) — and return the `satp` value (MODE=Sv39 | root PPN).
///
/// Kernel pages: `V R W X`, no `U` (so U-mode can never reach them). Runs in bare mode,
/// so the freshly-allocated root frame is written at its physical address directly.
pub unsafe fn build_kernel_identity(fa: &mut dyn FrameAllocator) -> u64 {
    let root_pa = fa.alloc_frame().expect("root page-table frame");
    let root = root_pa.as_u64() as *mut PageTable;
    core::ptr::write_bytes(root as *mut u8, 0, 4096); // frames are not zeroed by mm

    let flags = PageFlags::V | PageFlags::R | PageFlags::W | PageFlags::X;
    for gib in 0..3u64 {
        let base = gib << 30; // 1 GiB-aligned gigapage base
        (*root).entries[gib as usize] = Pte::new(PhysAddr(base), flags);
    }
    // PROOF(later): entries 0..3 are 1 GiB-aligned identity leaves; the walk of any VA in
    // 0..3 GiB yields the same physical address, kernel-only (no U bit).
    (8u64 << 60) | (root_pa.as_u64() >> 12)
}

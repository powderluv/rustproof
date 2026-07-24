//! RISC-V (rv64) implementation of the `hal` traits.
use abi::{FrameAllocator, MemoryKind, MemoryRegion, PhysAddr, VirtAddr};
use arch_riscv64::interrupts::{self, TrapFrame};
use arch_riscv64::{csr, mmu, qemu, serial::Uart, timer};
use hal::{Arch, Perms, Space, UserFrame};
use vspace_riscv::{PageFlags, PageTable, Pte};

/// Reinterpret the opaque [`UserFrame`] as the RISC-V [`TrapFrame`] (its first 34 words).
#[inline]
fn frame(f: &UserFrame) -> &TrapFrame {
    // SAFETY: UserFrame is `[u64; 40]`, align 16; TrapFrame is 34 `#[repr(C)]` words at
    // offset 0. The extra words are unused on RISC-V.
    unsafe { &*(f.0.as_ptr() as *const TrapFrame) }
}

#[inline]
fn frame_mut(f: &mut UserFrame) -> &mut TrapFrame {
    // SAFETY: see `frame`.
    unsafe { &mut *(f.0.as_mut_ptr() as *mut TrapFrame) }
}

/// User address space: a `vspace-riscv` Sv39 page-table tree.
pub struct RiscvSpace(vspace_riscv::AddressSpace);

impl Space for RiscvSpace {
    fn create(fa: &mut dyn FrameAllocator) -> Option<Self> {
        vspace_riscv::AddressSpace::create(0, fa).map(RiscvSpace)
    }

    fn map_page(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        perms: Perms,
        fa: &mut dyn FrameAllocator,
    ) -> bool {
        // Sv39 leaf: V + R always; +W/+X/+U per perms (no negative NX bit).
        let mut f = PageFlags::V | PageFlags::R;
        if perms.write {
            f = f | PageFlags::W;
        }
        if perms.exec {
            f = f | PageFlags::X;
        }
        if perms.user {
            f = f | PageFlags::U;
        }
        self.0.map(va, pa, f, fa).is_ok()
    }

    fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.0.translate(va)
    }

    fn token(&self) -> u64 {
        self.0.satp()
    }

    unsafe fn share_kernel(&mut self, kernel_token: u64) {
        // Copy the kernel identity gigapages (root entries 0..3) into the user root.
        let kroot = ((kernel_token & ((1u64 << 44) - 1)) << 12) as *const PageTable;
        let uroot = self.0.root_phys().as_u64() as *mut PageTable;
        for i in 0..3 {
            (*uroot).entries[i] = (*kroot).entries[i];
        }
    }
}

/// The RISC-V hardware surface.
pub struct Riscv;

impl Arch for Riscv {
    type Space = RiscvSpace;
    const NAME: &'static str = "riscv64";
    const USER_BASE: u64 = 0x1_0000_0000; // 4 GiB
    const USER_LIMIT: u64 = 0x2_0000_0000; // 8 GiB
    const USER_STACK_TOP: u64 = 0x1_4000_0000; // 5 GiB
    const USER_STACK_PAGES: u64 = 16;

    fn console_write(bytes: &[u8]) {
        for &b in bytes {
            Uart::write_byte(b);
        }
    }

    fn exit(success: bool) -> ! {
        if success {
            qemu::exit_success()
        } else {
            qemu::exit_fail()
        }
    }

    fn init_traps() {
        arch_riscv64::interrupts::init();
    }

    fn memory_map(_a0: u64, _a1: u64, out: &mut [MemoryRegion]) -> usize {
        // QEMU virt with -m 512M: RAM 0x8000_0000..0xA000_0000; reserve the low 4 MiB.
        if out.is_empty() {
            return 0;
        }
        out[0] = MemoryRegion {
            start: 0x8040_0000,
            len: 0xA000_0000 - 0x8040_0000,
            kind: MemoryKind::Usable,
        };
        1
    }

    fn reserve_below() -> u64 {
        0x8040_0000
    }

    fn dma_top() -> u64 {
        0x8800_0000
    }

    fn setup_paging(fa: &mut dyn FrameAllocator) -> u64 {
        // Build a 3 GiB identity map (1 GiB gigapage leaves, kernel-only) and enable Sv39.
        let root_pa = fa.alloc_frame().expect("root page-table frame");
        let root = root_pa.as_u64() as *mut PageTable;
        unsafe { core::ptr::write_bytes(root as *mut u8, 0, 4096) };
        let flags = PageFlags::V | PageFlags::R | PageFlags::W | PageFlags::X;
        for gib in 0..3u64 {
            unsafe { (*root).entries[gib as usize] = Pte::new(PhysAddr(gib << 30), flags) };
        }
        let satp = (8u64 << 60) | (root_pa.as_u64() >> 12);
        unsafe { mmu::enable_paging(satp) };
        satp
    }

    fn load_user(elf: &[u8], space: &mut Self::Space, fa: &mut dyn FrameAllocator) -> Option<u64> {
        loader_riscv::load_elf(elf, &mut space.0, fa)
            .ok()
            .map(|l| l.entry.as_u64())
    }

    const FRAME_WORDS: usize = TrapFrame::WORDS;

    fn frame_init(entry: u64, sp: u64, arg0: u64) -> UserFrame {
        let mut uf = UserFrame::ZERO;
        let tf = frame_mut(&mut uf);
        tf.regs[2] = sp; // x2 = sp
        tf.regs[10] = arg0; // a0 = process id (first arg)
        tf.sepc = entry;
        // SPP = 0 (sret -> U-mode), SPIE = 1 (interrupts on after sret), SUM = 1 (kernel
        // may access user memory while servicing this process's syscalls).
        tf.sstatus = csr::SSTATUS_SPIE | csr::SSTATUS_SUM;
        uf
    }

    fn frame_num(f: &UserFrame) -> u64 {
        frame(f).regs[17] // a7
    }

    fn frame_arg(f: &UserFrame, i: usize) -> u64 {
        // a0..a4 = x10..x14.
        frame(f).regs.get(10 + i).copied().unwrap_or(0)
    }

    fn frame_set_ret(f: &mut UserFrame, v: u64) {
        frame_mut(f).regs[10] = v; // a0 = result
    }

    fn frame_set_ret2(f: &mut UserFrame, v: u64) {
        // a1 — part of the trap frame and restored by `resume`, so the user sees it on
        // return (their stub declares it as an output).
        frame_mut(f).regs[11] = v;
    }

    fn start_preemption() {
        // Enable the Sstc supervisor timer + arm the first tick. Ticks fire in U-mode
        // (an S-interrupt is delivered there regardless of sstatus.SIE); the kernel runs
        // with SIE clear, so the handler stays non-reentrant.
        unsafe { timer::init() }
    }

    fn end_of_interrupt() {
        // Ack + schedule the next tick (writing stimecmp forward clears the pending one).
        unsafe { timer::rearm() }
    }

    unsafe fn resume(token: u64, f: &UserFrame) -> ! {
        // Switch to the process's address space, then restore its registers and `sret`.
        mmu::enable_paging(token);
        interrupts::resume(frame(f) as *const TrapFrame)
    }

    unsafe fn copy_to_user(uptr: u64, bytes: &[u8]) -> bool {
        if !Self::user_ptr_ok(uptr, bytes.len()) {
            return false;
        }
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), uptr as *mut u8, bytes.len());
        true
    }

    unsafe fn copy_from_user(uptr: u64, out: &mut [u8]) -> bool {
        if !Self::user_ptr_ok(uptr, out.len()) {
            return false;
        }
        core::ptr::copy_nonoverlapping(uptr as *const u8, out.as_mut_ptr(), out.len());
        true
    }

    fn user_ptr_ok(uptr: u64, len: usize) -> bool {
        (len as u64) < (1 << 20)
            && uptr >= Self::USER_BASE
            && uptr
                .checked_add(len as u64)
                .map_or(false, |end| end <= Self::USER_LIMIT)
    }
}

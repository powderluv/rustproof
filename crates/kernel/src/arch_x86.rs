//! x86-64 implementation of the `hal` traits.
use abi::{FrameAllocator, MemoryRegion, PhysAddr, VirtAddr};
use arch_x86_64::syscall::TrapFrame;
use arch_x86_64::{cpu, interrupts, pic, qemu, serial::Serial, syscall};
use hal::{Arch, Perms, Space, UserFrame};

/// Reinterpret the opaque [`UserFrame`] as the x86 [`TrapFrame`] (its first 20 words).
#[inline]
fn frame(f: &UserFrame) -> &TrapFrame {
    // SAFETY: UserFrame is `[u64; 40]`, align 16; TrapFrame is 20 `#[repr(C)]` words at
    // offset 0. The extra words are unused on x86.
    unsafe { &*(f.0.as_ptr() as *const TrapFrame) }
}

#[inline]
fn frame_mut(f: &mut UserFrame) -> &mut TrapFrame {
    // SAFETY: see `frame`.
    unsafe { &mut *(f.0.as_mut_ptr() as *mut TrapFrame) }
}

/// User address space: a `vspace` 4-level page-table tree.
pub struct X86Space(vspace::AddressSpace);

impl Space for X86Space {
    fn create(fa: &mut dyn FrameAllocator) -> Option<Self> {
        vspace::AddressSpace::create(0, fa).map(X86Space)
    }

    fn map_page(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        perms: Perms,
        fa: &mut dyn FrameAllocator,
    ) -> bool {
        let mut f = vspace::PageFlags::PRESENT;
        if perms.write {
            f = f | vspace::PageFlags::WRITABLE;
        }
        if perms.user {
            f = f | vspace::PageFlags::USER;
        }
        if !perms.exec {
            f = f | vspace::PageFlags::NO_EXEC;
        }
        if perms.device {
            f = f | vspace::PageFlags::NO_CACHE;
        }
        self.0.map(va, pa, f, fa).is_ok()
    }

    fn unmap_page(&mut self, va: VirtAddr) -> Option<PhysAddr> {
        self.0.unmap(va)
    }

    fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.0.translate(va)
    }

    fn token(&self) -> u64 {
        self.0.pml4_phys().as_u64()
    }

    unsafe fn from_token(token: u64) -> Self {
        X86Space(vspace::AddressSpace::new(
            PhysAddr(token & 0x000f_ffff_ffff_f000),
            0,
        ))
    }

    unsafe fn share_kernel(&mut self, kernel_token: u64) {
        // Copy PML4[0] (the kernel's low identity map) into the user root. Kernel leaf
        // pages carry no USER bit, so ring 3 still cannot reach them.
        let kroot = (kernel_token & 0x000f_ffff_ffff_f000) as *const u64;
        let uroot = self.0.pml4_phys().as_u64() as *mut u64;
        core::ptr::write(uroot, core::ptr::read(kroot));
    }
}

/// Is `[uptr, uptr+len)` inside the user window AND actually mapped, with USER access and
/// (when `need_write`) write permission, in the CURRENTLY ACTIVE address space?
///
/// A range check alone is not enough for a ring-0 copy on a user's behalf. An in-range but
/// unmapped address would fault in the kernel — fatal, since the exception handler dumps
/// and halts — and without this a write would also go through a page the loader mapped
/// read-only (the kernel bypasses the R/W bit unless `CR0.WP` is set, which boot.s now
/// does). Both are the caller's failure to report, not the kernel's to crash on.
fn user_range_mapped(uptr: u64, len: usize, need_write: bool) -> bool {
    if !X86::user_ptr_ok(uptr, len) {
        return false;
    }
    if len == 0 {
        return true;
    }
    let mut need = vspace::PageFlags::PRESENT | vspace::PageFlags::USER;
    if need_write {
        need = need | vspace::PageFlags::WRITABLE;
    }
    // SAFETY: reading CR3 is always valid; the active tree is identity-mapped low RAM.
    let root = unsafe { cpu::read_cr3() } & 0x000f_ffff_ffff_f000;
    let space = vspace::AddressSpace::new(PhysAddr(root), 0);
    let mut va = uptr & !(abi::PAGE_SIZE - 1);
    let end = uptr.saturating_add(len as u64);
    while va < end {
        match space.leaf_flags(VirtAddr(va)) {
            Some(f) if f.contains(need) => {}
            _ => return false,
        }
        va = va.saturating_add(abi::PAGE_SIZE);
    }
    true
}

/// The x86-64 hardware surface.
pub struct X86;

impl Arch for X86 {
    type Space = X86Space;
    const NAME: &'static str = "x86-64";
    const USER_BASE: u64 = 0x80_0000_0000;
    const USER_LIMIT: u64 = 0x81_0000_0000;
    const USER_STACK_TOP: u64 = 0x80_4000_0000;
    const USER_STACK_PAGES: u64 = 16;
    const USER_MMIO_BASE: u64 = 0x80_2000_0000;

    // Above the stack top and far below `USER_LIMIT`: the share window is the only mapping
    // whose address the KERNEL chooses, so it must not be able to land on the image, the
    // device window or the stack. Checked at boot, not trusted from this comment.
    const USER_SHARE_BASE: u64 = 0x80_5000_0000;

    fn console_write(bytes: &[u8]) {
        for &b in bytes {
            Serial::write_byte(b);
        }
    }

    fn exit(success: bool) -> ! {
        qemu::exit(if success {
            qemu::EXIT_SUCCESS
        } else {
            qemu::EXIT_FAILURE
        })
    }

    fn init_traps() {
        arch_x86_64::interrupts::init();
        arch_x86_64::gdt::init();
        syscall::init();
    }

    fn memory_map(a0: u64, _a1: u64, out: &mut [MemoryRegion]) -> usize {
        crate::pvh::memory_map(a0, out)
    }

    fn reserve_below() -> u64 {
        0x80_0000 // 8 MiB (kernel image + low structures)
    }

    fn dma_top() -> u64 {
        0x100_0000 // 16 MiB
    }

    fn flush_tlb() {
        // Reloading CR3 drops every non-global entry, which is enough here and needs no new
        // primitive: `invlpg` would be narrower but this runs once at boot.
        unsafe { cpu::write_cr3(cpu::read_cr3()) }
    }

    fn setup_paging(_fa: &mut dyn FrameAllocator) -> u64 {
        // x86-64 boots with paging on (boot trampoline identity-maps the low 1 GiB); the
        // kernel token is the active CR3.
        unsafe { cpu::read_cr3() }
    }

    fn load_user(elf: &[u8], space: &mut Self::Space, fa: &mut dyn FrameAllocator) -> Option<u64> {
        loader::load_elf(elf, &mut space.0, fa)
            .ok()
            .map(|l| l.entry.as_u64())
    }

    const FRAME_WORDS: usize = TrapFrame::WORDS;

    fn frame_init(entry: u64, sp: u64, arg0: u64) -> UserFrame {
        let mut uf = UserFrame::ZERO;
        *frame_mut(&mut uf) = TrapFrame::new_user(entry, sp, arg0);
        uf
    }

    fn frame_num(f: &UserFrame) -> u64 {
        frame(f).rax
    }

    fn frame_arg(f: &UserFrame, i: usize) -> u64 {
        let tf = frame(f);
        match i {
            0 => tf.rdi,
            1 => tf.rsi,
            2 => tf.rdx,
            3 => tf.r10,
            4 => tf.r8,
            _ => 0,
        }
    }

    fn frame_set_ret(f: &mut UserFrame, v: u64) {
        frame_mut(f).rax = v;
    }

    fn frame_set_ret3(f: &mut UserFrame, v: u64) {
        // `r10` — the a3 argument register, part of the trap frame and restored by `resume`.
        frame_mut(f).r10 = v;
    }

    unsafe fn activate(token: u64) {
        cpu::write_cr3(token);
    }

    fn frame_set_ret2(f: &mut UserFrame, v: u64) {
        // `rdx` — a caller-saved arg register, part of the trap frame and restored by
        // `resume`, so the user sees it on return (their stub declares it as an output).
        frame_mut(f).rdx = v;
    }

    fn start_preemption() {
        // Remap the PICs (IRQ0 -> vector 0x20), point that vector at the timer stub, and
        // start a ~100 Hz PIT tick. Interrupts stay masked in the kernel, so the first
        // tick only arrives once a process is running in ring 3 (IF set on `iretq`).
        unsafe {
            pic::remap_and_mask();
            interrupts::set_gate(pic::TIMER_VECTOR, interrupts::timer_handler_addr());
            pic::init_pit(100);
        }
    }

    unsafe fn idle() -> ! {
        syscall::idle()
    }

    fn start_console_irq() {
        // COM1 receive -> IRQ4 -> vector 0x24 -> the device stub. The UART's interrupt is
        // enabled last: the gate and the mask must be in place before the line can assert.
        unsafe {
            interrupts::set_gate(pic::CONSOLE_VECTOR, interrupts::console_handler_addr());
            // A second unmasked line is what makes a spurious IRQ7 reachable; give it a
            // gate before enabling the source, or the first one kills the guest.
            interrupts::set_gate(pic::SPURIOUS_VECTOR, interrupts::spurious_handler_addr());
            pic::unmask_console();
            Serial::enable_rx_interrupt();
        }
    }

    fn console_irq_ack() {
        // Drain first, EOI second: EOI only clears the controller's latch, and a byte still
        // sitting in the UART would immediately re-assert IRQ4.
        unsafe {
            Serial::drain_rx();
            pic::eoi_master();
        }
    }

    fn end_of_interrupt() {
        unsafe { pic::eoi_master() }
    }

    unsafe fn resume(token: u64, f: &UserFrame) -> ! {
        cpu::write_cr3(token);
        syscall::resume(frame(f) as *const TrapFrame)
    }

    fn user_write_ok(uptr: u64, len: usize) -> bool {
        user_range_mapped(uptr, len, true)
    }

    unsafe fn copy_to_user(uptr: u64, bytes: &[u8]) -> bool {
        if !user_range_mapped(uptr, bytes.len(), true) {
            return false;
        }
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), uptr as *mut u8, bytes.len());
        true
    }

    unsafe fn copy_from_user(uptr: u64, out: &mut [u8]) -> bool {
        if !user_range_mapped(uptr, out.len(), false) {
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

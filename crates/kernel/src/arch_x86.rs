//! x86-64 implementation of the `hal` traits.
use abi::{FrameAllocator, MemoryRegion, PhysAddr, VirtAddr};
use arch_x86_64::{cpu, qemu, serial::Serial, syscall};
use hal::{Arch, Perms, Space};

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
        self.0.map(va, pa, f, fa).is_ok()
    }

    fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.0.translate(va)
    }

    fn token(&self) -> u64 {
        self.0.pml4_phys().as_u64()
    }

    unsafe fn share_kernel(&mut self, kernel_token: u64) {
        // Copy PML4[0] (the kernel's low identity map) into the user root. Kernel leaf
        // pages carry no USER bit, so ring 3 still cannot reach them.
        let kroot = (kernel_token & 0x000f_ffff_ffff_f000) as *const u64;
        let uroot = self.0.pml4_phys().as_u64() as *mut u64;
        core::ptr::write(uroot, core::ptr::read(kroot));
    }
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

    unsafe fn enter_user(space_token: u64, entry: u64, user_sp: u64) -> ! {
        cpu::write_cr3(space_token);
        syscall::enter_user(entry, user_sp)
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

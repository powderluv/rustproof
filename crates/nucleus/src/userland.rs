//! Userland bring-up: load the `init` ELF into a fresh address space, grant it its
//! capabilities, drop to ring 3, and service its capability-gated host-contract syscalls.
//!
//! The kernel stays mapped (no CR3 switch on `syscall`) via a shared kernel PML4[0], so
//! the dispatcher reaches both kernel state and — since CR3 is the user address space and
//! there is no SMAP — the caller's user memory directly.
use abi::{CapRights, CapType, FrameAllocator, GpuInfo, HostEnv, PhysAddr, VirtAddr};
use arch_x86_64::{cpu, kprintln, qemu, syscall};

/// User virtual-address window (PML4[1], >= 512 GiB). The kernel identity map lives in
/// PML4[0] and is never user-accessible, so these never overlap kernel memory.
const USER_BASE: u64 = 0x80_0000_0000;
const USER_LIMIT: u64 = 0x81_0000_0000;
/// Top of the user stack (grows down); 16 pages are mapped below it.
const USER_STACK_TOP: u64 = 0x80_4000_0000;
const USER_STACK_PAGES: u64 = 16;

// Kernel state the syscall dispatcher needs while a process runs. Single process, single
// CPU: plain statics are enough (a per-process/per-CPU table comes with real scheduling).
static mut FA: Option<mm::BitmapAllocator> = None;
static mut PROC_CAPS: capabilities::CapSpace<64> = capabilities::CapSpace::new();

fn user_range_ok(uptr: u64, len: usize) -> bool {
    (len as u64) < (1 << 20)
        && uptr >= USER_BASE
        && uptr
            .checked_add(len as u64)
            .map_or(false, |end| end <= USER_LIMIT)
}

/// The real `HostEnv`, backed by kernel state. Its effects are the trusted boundary the
/// pure `hostcontract::dispatch` logic sits behind.
struct KernelEnv;

impl HostEnv for KernelEnv {
    fn debug_write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            arch_x86_64::serial::Serial::write_byte(b);
        }
    }

    fn gpu_info(&self) -> GpuInfo {
        // Stubbed gfx1201 identity (real values come from PCI enumeration later).
        GpuInfo {
            pci_vendor: 0x1002,
            pci_device: 0x7551,
            gfx_version: 0x1201,
            vram_bytes: 16u64 << 30,
        }
    }

    fn cap_lookup(&self, cap: abi::CapId) -> Option<(CapType, CapRights, u64)> {
        // SAFETY: single-threaded; PROC_CAPS is only mutated during setup, before ring 3.
        let caps = unsafe { &*core::ptr::addr_of!(PROC_CAPS) };
        caps.lookup(cap).map(|s| (s.cap_type, s.rights, s.object))
    }

    fn alloc_dma(&mut self) -> Option<PhysAddr> {
        // SAFETY: single-threaded access to the process's allocator.
        let fa = unsafe { (*core::ptr::addr_of_mut!(FA)).as_mut()? };
        fa.alloc_dma_frame()
    }

    fn write_user_bytes(&mut self, uptr: u64, bytes: &[u8]) -> bool {
        if !user_range_ok(uptr, bytes.len()) {
            return false;
        }
        // SAFETY: CR3 is the user AS; `uptr` is validated to lie in the user window whose
        // pages are mapped writable+user. Ring 0 may write user pages (no SMAP).
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), uptr as *mut u8, bytes.len()) };
        true
    }

    fn read_user_bytes(&self, uptr: u64, out: &mut [u8]) -> bool {
        if !user_range_ok(uptr, out.len()) {
            return false;
        }
        // SAFETY: as above; reading validated, mapped user memory.
        unsafe { core::ptr::copy_nonoverlapping(uptr as *const u8, out.as_mut_ptr(), out.len()) };
        true
    }
}

/// Called from the `syscall` entry stub (arch) with the decoded register args. `EXIT` ends
/// the process (and, for this demo, the run); everything else is the capability-gated
/// host contract.
#[no_mangle]
extern "C" fn rustproof_syscall_dispatch(
    num: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
) -> u64 {
    if num == abi::sysno::EXIT {
        kprintln!();
        kprintln!("[kernel] init exited with code {}", a0);
        kprintln!("rustproof: BOOT OK");
        qemu::exit(qemu::EXIT_SUCCESS);
    }
    let mut env = KernelEnv;
    hostcontract::dispatch(&mut env, num, a0, a1, a2, a3, a4)
}

/// Build the user address space, load `user_elf`, grant caps, switch CR3, and drop to
/// ring 3. Consumes `fa` (parked in a static for the dispatcher). Never returns — the
/// process runs until it makes an `EXIT` syscall.
pub unsafe fn setup_and_enter(user_elf: &[u8], mut fa: mm::BitmapAllocator) -> ! {
    arch_x86_64::gdt::init();
    syscall::init();

    let mut aspace = vspace::AddressSpace::create(0, &mut fa).expect("user address space");

    // Share the kernel's low identity map (PML4[0]) into the user AS so the syscall
    // handler + kernel stay mapped while CR3 is the user AS. The kernel leaf pages have no
    // USER bit, so ring 3 still cannot reach them.
    let boot_pml4 = cpu::read_cr3() & 0x000f_ffff_ffff_f000;
    let kernel_entry0 = core::ptr::read(boot_pml4 as *const u64);
    core::ptr::write(aspace.pml4_phys().as_u64() as *mut u64, kernel_entry0);

    let loaded = loader::load_elf(user_elf, &mut aspace, &mut fa).expect("load init ELF");

    // Map the user stack (writable + user).
    let ustack_flags =
        vspace::PageFlags::PRESENT | vspace::PageFlags::WRITABLE | vspace::PageFlags::USER;
    for i in 1..=USER_STACK_PAGES {
        let va = VirtAddr(USER_STACK_TOP - i * abi::PAGE_SIZE);
        let frame = fa.alloc_frame().expect("user stack frame");
        aspace
            .map(va, frame, ustack_flags, &mut fa)
            .expect("map user stack");
    }

    // Grant the process its capabilities. `init` expects Mmio at CapId(1) and Untyped at
    // CapId(2); slot 0 is a placeholder so the grants land at slots 1 and 2.
    let caps = &mut *core::ptr::addr_of_mut!(PROC_CAPS);
    let _placeholder = caps.insert(CapType::Endpoint, CapRights::NONE, 0);
    let c_mmio = caps.insert(CapType::Mmio, CapRights::ALL, 0xE000_0000);
    let c_untyped = caps.insert(CapType::Untyped, CapRights::ALL, 0);
    kprintln!(
        "[user] granted caps: Mmio@{:?} Untyped@{:?}; init entry {:#x}",
        c_mmio,
        c_untyped,
        loaded.entry.as_u64()
    );

    // Park the allocator for the dispatcher (ALLOC_VRAM), switch to the user AS, go.
    FA = Some(fa);
    cpu::write_cr3(aspace.pml4_phys().as_u64());
    kprintln!("[user] dropping to ring 3");
    syscall::enter_user(loaded.entry.as_u64(), USER_STACK_TOP)
}

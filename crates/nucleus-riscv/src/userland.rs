//! RISC-V userland bring-up (RV-M2): load `riscv-init` into a user address space, drop to
//! U-mode via `sret`, and service its capability-gated host-contract `ecall` syscalls.
//!
//! The user AS shares the kernel's identity gigapages (root entries 0..3) so the S-mode
//! trap vector + handler stay reachable when a U-mode `ecall` traps; `sstatus.SUM` lets
//! the handler read/write the caller's user memory directly (the pages carry the U bit).
use abi::{CapRights, CapType, FrameAllocator, GpuInfo, HostEnv, PhysAddr, VirtAddr};
use arch_riscv64::{kprintln, mmu, qemu};
use core::fmt::Write as _;
use vspace_riscv::{AddressSpace, PageFlags, PageTable};

const USER_BASE: u64 = 0x1_0000_0000; // 4 GiB (riscv-init link base)
const USER_LIMIT: u64 = 0x2_0000_0000; // 8 GiB
const USER_STACK_TOP: u64 = 0x1_4000_0000; // 5 GiB
const USER_STACK_PAGES: u64 = 16;

static mut FA: Option<mm::BitmapAllocator> = None;
static mut PROC_CAPS: capabilities::CapSpace<64> = capabilities::CapSpace::new();

fn user_range_ok(uptr: u64, len: usize) -> bool {
    (len as u64) < (1 << 20)
        && uptr >= USER_BASE
        && uptr
            .checked_add(len as u64)
            .map_or(false, |end| end <= USER_LIMIT)
}

struct RiscvEnv;

impl HostEnv for RiscvEnv {
    fn debug_write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let _ = arch_riscv64::serial::Uart.write_char(b as char);
        }
    }

    fn gpu_info(&self) -> GpuInfo {
        GpuInfo {
            pci_vendor: 0x1002,
            pci_device: 0x7551,
            gfx_version: 0x1201,
            vram_bytes: 16u64 << 30,
        }
    }

    fn cap_lookup(&self, cap: abi::CapId) -> Option<(CapType, CapRights, u64)> {
        // SAFETY: single-threaded; PROC_CAPS is only mutated during setup, before U-mode.
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
        // SAFETY: satp is the user AS; `uptr` is validated in the user window (U pages),
        // and SUM is set so S-mode may write it.
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

/// The `ecall`-from-U handler body (called by the arch trap dispatcher). `EXIT` ends the
/// process (and the run); everything else is the capability-gated host contract.
#[no_mangle]
extern "C" fn rustproof_riscv_syscall_dispatch(
    num: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
) -> u64 {
    if num == abi::sysno::EXIT {
        kprintln!();
        kprintln!("[kernel] riscv-init exited with code {}", a0);
        kprintln!("rustproof: RV BOOT OK");
        qemu::exit_success();
    }
    let mut env = RiscvEnv;
    hostcontract::dispatch(&mut env, num, a0, a1, a2, a3, a4)
}

/// Build the user address space (sharing the kernel identity gigapages), load `user_elf`,
/// grant caps, and drop to U-mode. `ksatp` is the RV-M1 kernel satp. Never returns.
pub unsafe fn setup_and_enter(user_elf: &[u8], mut fa: mm::BitmapAllocator, ksatp: u64) -> ! {
    let mut aspace = AddressSpace::create(0, &mut fa).expect("user address space");

    // Share the kernel identity gigapages (root entries 0..3) into the user root.
    let kroot = ((ksatp & ((1u64 << 44) - 1)) << 12) as *const PageTable;
    let uroot = aspace.root_phys().as_u64() as *mut PageTable;
    for i in 0..3 {
        (*uroot).entries[i] = (*kroot).entries[i];
    }

    let loaded = loader_riscv::load_elf(user_elf, &mut aspace, &mut fa).expect("load riscv-init");

    // Map the user stack (V R W U).
    let sflags = PageFlags::V | PageFlags::R | PageFlags::W | PageFlags::U;
    for i in 1..=USER_STACK_PAGES {
        let va = VirtAddr(USER_STACK_TOP - i * abi::PAGE_SIZE);
        let frame = fa.alloc_frame().expect("user stack frame");
        aspace
            .map(va, frame, sflags, &mut fa)
            .expect("map user stack");
    }

    // Grant caps: riscv-init expects Mmio at CapId(1), Untyped at CapId(2); slot 0 is a
    // placeholder so the grants land at slots 1 and 2.
    let caps = &mut *core::ptr::addr_of_mut!(PROC_CAPS);
    let _placeholder = caps.insert(CapType::Endpoint, CapRights::NONE, 0);
    let c_mmio = caps.insert(CapType::Mmio, CapRights::ALL, 0xE000_0000);
    let c_untyped = caps.insert(CapType::Untyped, CapRights::ALL, 0);
    kprintln!(
        "[user] granted caps: Mmio@{:?} Untyped@{:?}; entry {:#x}",
        c_mmio,
        c_untyped,
        loaded.entry.as_u64()
    );

    FA = Some(fa);
    kprintln!("[user] dropping to U-mode");
    mmu::enter_user(aspace.satp(), loaded.entry.as_u64(), USER_STACK_TOP)
}

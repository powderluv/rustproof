//! kernel — the architecture-generic Rustproof nucleus, written once against
//! [`hal::Arch`] and instantiated per-ISA by the thin `nucleus` / `nucleus-riscv` bins.
//!
//! [`run`] is the whole kmain: console + traps, the portable core (frame allocator,
//! capabilities, IPC, a cooperative context switch), then paging + the capability-gated
//! ring-3/U-mode userland. The x86-64 and RISC-V specifics live entirely behind the
//! `hal::Arch` + `hal::Space` implementations in [`arch_x86`] / [`arch_riscv`].
#![no_std]

use core::fmt::Write as _;
use hal::{Arch, Perms, Space};

#[cfg(target_arch = "x86_64")]
mod arch_x86;
#[cfg(target_arch = "x86_64")]
mod pvh;
#[cfg(target_arch = "x86_64")]
pub use arch_x86::X86 as CurrentArch;

#[cfg(target_arch = "riscv64")]
mod arch_riscv;
#[cfg(target_arch = "riscv64")]
pub use arch_riscv::Riscv as CurrentArch;

// ---- kernel state (single process, single CPU: plain statics) ----
static mut BITMAP: [u64; 12288] = [0; 12288]; // one bit per 4 KiB frame
static mut FA: Option<mm::BitmapAllocator> = None;
static mut PROC_CAPS: capabilities::CapSpace<64> = capabilities::CapSpace::new();
static mut MAIN_CTX: sched::Context = sched::Context::new();
static mut B_CTX: sched::Context = sched::Context::new();
static mut B_STACK: [u8; 16 * 1024] = [0; 16 * 1024];

/// Stubbed gfx1201 identity returned by the host contract's `GET_INFO`.
const GPU_INFO: abi::GpuInfo = abi::GpuInfo {
    pci_vendor: 0x1002,
    pci_device: 0x7551,
    gfx_version: 0x1201,
    vram_bytes: 16u64 << 30,
};

/// A `core::fmt::Write` sink that routes through `A::console_write`, expanding `\n` to CRLF.
pub struct Console<A>(core::marker::PhantomData<A>);

impl<A: Arch> Console<A> {
    pub fn new() -> Self {
        Console(core::marker::PhantomData)
    }
}

impl<A: Arch> Default for Console<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Arch> core::fmt::Write for Console<A> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                A::console_write(b"\r");
            }
            A::console_write(&[b]);
        }
        Ok(())
    }
}

/// A second kernel thread used to exercise the cooperative context switch.
extern "C" fn thread_b<A: Arch>() -> ! {
    A::console_write(b"  [thread B] running on its own stack -- switching back to main\n");
    unsafe {
        sched::switch(
            core::ptr::addr_of_mut!(B_CTX),
            core::ptr::addr_of!(MAIN_CTX),
        )
    };
    loop {
        core::hint::spin_loop();
    }
}

/// The generic kmain. `a0`/`a1` are the arch boot args (x86 PVH `start_info`; RISC-V
/// `hartid`/`dtb`); `user_elf` is the embedded user program. Never returns.
pub fn run<A: Arch>(a0: u64, a1: u64, user_elf: &[u8]) -> ! {
    use abi::FrameAllocator as _;
    let mut con = Console::<A>::new();
    let _ = writeln!(con);
    let _ = writeln!(con, "Rustproof nucleus ({}) — unified kernel/hal", A::NAME);

    A::init_traps();
    let _ = writeln!(con, "  traps installed");

    #[cfg(feature = "provoke-fault")]
    {
        let _ = writeln!(
            con,
            "provoke-fault: reading unmapped 0xDEADBEEF to force a fault"
        );
        let _ = unsafe { core::ptr::read_volatile(0xDEAD_BEEF_usize as *const u32) };
    }

    // ---------------- mm: memory map + bitmap frame allocator ----------------
    let mut regions = [abi::MemoryRegion {
        start: 0,
        len: 0,
        kind: abi::MemoryKind::Reserved,
    }; 32];
    let n = A::memory_map(a0, a1, &mut regions);
    let regions = &regions[..n];
    let words = mm::BitmapAllocator::bitmap_words_needed(regions);
    let bitmap: &'static mut [u64] = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(BITMAP) as *mut u64, words)
    };
    let mut fa = mm::BitmapAllocator::new(regions, bitmap, A::reserve_below(), A::dma_top());
    let _ = writeln!(
        con,
        "\n[mm] {} frames tracked, {} free",
        fa.total_frames(),
        fa.free_count()
    );
    let f0 = fa.alloc_frame();
    let f1 = fa.alloc_frame();
    let dma = fa.alloc_dma_frame();
    let _ = writeln!(
        con,
        "[mm] alloc -> {:#x}, {:#x}; dma -> {:#x}; free {}",
        f0.map(|p| p.as_u64()).unwrap_or(0),
        f1.map(|p| p.as_u64()).unwrap_or(0),
        dma.map(|p| p.as_u64()).unwrap_or(0),
        fa.free_count()
    );
    if let Some(p) = f0 {
        fa.free_frame(p);
    }

    // ---------------- capabilities: authority-monotonic derivation ----------------
    {
        let mut caps = capabilities::CapSpace::<64>::new();
        let root = caps
            .insert(abi::CapType::Untyped, abi::CapRights::ALL, 0xF00D)
            .expect("root cap");
        let child = caps
            .derive(root, abi::CapRights::READ)
            .expect("read-only child");
        let escalated = caps
            .derive(child, abi::CapRights::WRITE)
            .and_then(|c| caps.lookup(c))
            .map(|s| s.rights.0)
            .unwrap_or(0);
        let _ = writeln!(
            con,
            "\n[cap] READ-only child derives WRITE -> rights={:#05b} ({})",
            escalated,
            if escalated & abi::CapRights::WRITE.0 == 0 {
                "WRITE dropped — authority-monotonic"
            } else {
                "ESCALATED?! (bug)"
            }
        );
        caps.revoke_subtree(root);
    }

    // ---------------- ipc: synchronous endpoint ----------------
    {
        let mut ep = ipc::Endpoint::<8>::new();
        let waiting = ep.recv(abi::ThreadId(2));
        let _ = writeln!(con, "\n[ipc] T2 recv with no sender -> {:?}", waiting);
        let words = [0xCAFE_u64, 0xF00D_u64];
        if let ipc::IpcAction::Deliver { to, from, msg, .. } =
            ep.send(abi::ThreadId(1), abi::MessageInfo::new(0x42, 2), &words)
        {
            let _ = writeln!(
                con,
                "[ipc] T1 send -> Deliver to T{} from T{} label={:#x}",
                to.0, from.0, msg.label
            );
        }
    }

    // ---------------- sched: real cooperative context switch ----------------
    let _ = writeln!(con, "\n[sched] switching to thread B (real context switch)");
    unsafe {
        let top = abi::VirtAddr(core::ptr::addr_of!(B_STACK) as u64 + 16 * 1024);
        B_CTX = sched::Context::prepare(top, thread_b::<A>);
        sched::switch(
            core::ptr::addr_of_mut!(MAIN_CTX),
            core::ptr::addr_of!(B_CTX),
        );
    }
    let _ = writeln!(con, "  [main] resumed after thread B via context switch");

    // ---------------- paging ----------------
    let ktoken = A::setup_paging(&mut fa);
    let _ = writeln!(con, "\n[paging] enabled; kernel token = {:#018x}", ktoken);

    // ---------------- userland: capability-gated ring-3/U-mode ----------------
    if user_elf.len() >= 64 {
        let mut space = A::Space::create(&mut fa).expect("user address space");
        unsafe { space.share_kernel(ktoken) };
        let entry = A::load_user(user_elf, &mut space, &mut fa).expect("load user ELF");
        for i in 1..=A::USER_STACK_PAGES {
            let va = abi::VirtAddr(A::USER_STACK_TOP - i * abi::PAGE_SIZE);
            let frame = fa.alloc_frame().expect("user stack frame");
            space.map_page(va, frame, Perms::USER_RW, &mut fa);
        }
        // Grant caps: slot 0 placeholder, Mmio@CapId(1), Untyped@CapId(2).
        unsafe {
            let caps = &mut *core::ptr::addr_of_mut!(PROC_CAPS);
            let _ = caps.insert(abi::CapType::Endpoint, abi::CapRights::NONE, 0);
            let _ = caps.insert(abi::CapType::Mmio, abi::CapRights::ALL, 0xE000_0000);
            let _ = caps.insert(abi::CapType::Untyped, abi::CapRights::ALL, 0);
        }
        let token = space.token();
        unsafe { FA = Some(fa) };
        let _ = writeln!(con, "[user] entering user mode (entry {:#x})", entry);
        unsafe { A::enter_user(token, entry, A::USER_STACK_TOP) };
    }

    let _ = writeln!(con, "\nrustproof: BOOT OK (no user image)");
    A::exit(true)
}

/// The syscall/`ecall` handler body — called by the arch entry stub via the
/// `rustproof_syscall_dispatch` symbol the thin bin exports. `EXIT` ends the run.
pub fn handle_syscall<A: Arch>(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    if num == abi::sysno::EXIT {
        let mut con = Console::<A>::new();
        let _ = writeln!(con, "\n[kernel] user exited with code {}", a0);
        let _ = writeln!(con, "rustproof: BOOT OK");
        A::exit(true);
    }
    let mut env = KEnv::<A>(core::marker::PhantomData);
    hostcontract::dispatch(&mut env, num, a0, a1, a2, a3, a4)
}

/// The real `HostEnv`, backed by kernel state + the current `Arch`'s user-memory access.
struct KEnv<A>(core::marker::PhantomData<A>);

impl<A: Arch> abi::HostEnv for KEnv<A> {
    fn debug_write(&mut self, bytes: &[u8]) {
        A::console_write(bytes);
    }
    fn gpu_info(&self) -> abi::GpuInfo {
        GPU_INFO
    }
    fn cap_lookup(&self, cap: abi::CapId) -> Option<(abi::CapType, abi::CapRights, u64)> {
        let caps = unsafe { &*core::ptr::addr_of!(PROC_CAPS) };
        caps.lookup(cap).map(|s| (s.cap_type, s.rights, s.object))
    }
    fn alloc_dma(&mut self) -> Option<abi::PhysAddr> {
        let fa = unsafe { (*core::ptr::addr_of_mut!(FA)).as_mut()? };
        fa.alloc_dma_frame()
    }
    fn write_user_bytes(&mut self, uptr: u64, bytes: &[u8]) -> bool {
        unsafe { A::copy_to_user(uptr, bytes) }
    }
    fn read_user_bytes(&self, uptr: u64, out: &mut [u8]) -> bool {
        unsafe { A::copy_from_user(uptr, out) }
    }
}

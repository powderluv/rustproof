//! kernel — the architecture-generic Rustproof nucleus, written once against
//! [`hal::Arch`] and instantiated per-ISA by the thin `nucleus` / `nucleus-riscv` bins.
//!
//! [`run`] is the whole kmain: console + traps, the portable core (frame allocator,
//! capabilities, IPC, a cooperative context switch), then paging, and finally several
//! isolated capability-gated user processes under a round-robin scheduler. Every entry
//! from user mode saves a [`hal::UserFrame`] and re-enters via `Arch::resume`;
//! [`syscall_trap`] services the call / `YIELD` / `EXIT` and picks the next process (see
//! `docs/scheduling.md`). The x86-64 and RISC-V specifics live entirely behind the
//! `hal::Arch` + `hal::Space` implementations in [`arch_x86`] / [`arch_riscv`].
#![no_std]

use core::fmt::Write as _;
use core::marker::PhantomData;
use hal::{Arch, Perms, Space, UserFrame};

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

// ---- kernel state (single CPU: plain statics) ----
static mut BITMAP: [u64; 12288] = [0; 12288]; // one bit per 4 KiB frame
static mut FA: Option<mm::BitmapAllocator> = None;
static mut MAIN_CTX: sched::Context = sched::Context::new();
static mut B_CTX: sched::Context = sched::Context::new();
static mut B_STACK: [u8; 16 * 1024] = [0; 16 * 1024];

/// Run-queue capacity (also the process-table size).
const MAX_PROCS: usize = 4;
/// How many independent copies of the user image to launch.
const NUM_PROCS: usize = 3;

/// One scheduled user process: its own address space (`token`), its last-saved user
/// register state (`frame`), and its own capability space — the isolation boundary.
struct Process {
    /// Paging-base token (`cr3`/`satp`) of this process's address space.
    token: u64,
    /// Saved user register state, resumed by `Arch::resume`.
    frame: UserFrame,
    /// Per-process authority: what this process may invoke via the host contract.
    caps: capabilities::CapSpace<16>,
    /// False once the process has exited (slot reusable).
    active: bool,
}

impl Process {
    const EMPTY: Process = Process {
        token: 0,
        frame: UserFrame::ZERO,
        caps: capabilities::CapSpace::new(),
        active: false,
    };
}

/// The process table + round-robin run queue + index of the running process. Kept in
/// sync: `CURRENT == SCHED.current()` at every trap boundary.
static mut PROCS: [Process; MAX_PROCS] = [Process::EMPTY; MAX_PROCS];
static mut SCHED: sched::Scheduler<MAX_PROCS> = sched::Scheduler::new();
static mut CURRENT: usize = 0;

/// A `&'static mut` to process slot `i`, via a raw pointer (no direct `static mut` ref).
///
/// # Safety
/// Single-CPU, non-reentrant: callers hold no other live borrow of `PROCS[i]`.
#[inline]
unsafe fn proc_at<'a>(i: usize) -> &'a mut Process {
    &mut *(core::ptr::addr_of_mut!(PROCS) as *mut Process).add(i)
}

/// A `&'static mut` to the scheduler, via a raw pointer.
///
/// # Safety
/// Single-CPU, non-reentrant: callers hold no other live borrow of `SCHED`.
#[inline]
unsafe fn sched() -> &'static mut sched::Scheduler<MAX_PROCS> {
    &mut *core::ptr::addr_of_mut!(SCHED)
}

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

    // ---------------- userland: N isolated capability-gated processes ----------------
    if user_elf.len() >= 64 {
        let _ = writeln!(
            con,
            "\n[proc] loading {} isolated user processes",
            NUM_PROCS
        );
        for i in 0..NUM_PROCS {
            // Each process gets its own address space (kernel shared in) + own stack.
            let mut space = A::Space::create(&mut fa).expect("user address space");
            unsafe { space.share_kernel(ktoken) };
            let entry = A::load_user(user_elf, &mut space, &mut fa).expect("load user ELF");
            for p in 1..=A::USER_STACK_PAGES {
                let va = abi::VirtAddr(A::USER_STACK_TOP - p * abi::PAGE_SIZE);
                let frame = fa.alloc_frame().expect("user stack frame");
                space.map_page(va, frame, Perms::USER_RW, &mut fa);
            }
            // Populate this process's slot: its own caps (Mmio@CapId(1), Untyped@CapId(2);
            // slot 0 is a placeholder so the first grant lands at CapId(1)), token, and an
            // initial frame that enters `_start` with its process id in the first-arg reg.
            let slot = unsafe { proc_at(i) };
            let _ = slot
                .caps
                .insert(abi::CapType::Endpoint, abi::CapRights::NONE, 0);
            let _ = slot
                .caps
                .insert(abi::CapType::Mmio, abi::CapRights::ALL, 0xE000_0000);
            let _ = slot
                .caps
                .insert(abi::CapType::Untyped, abi::CapRights::ALL, 0);
            slot.token = space.token();
            slot.frame = A::frame_init(entry, A::USER_STACK_TOP, i as u64);
            slot.active = true;
            unsafe { sched().add(abi::ThreadId(i)) };
            let _ = writeln!(con, "  proc {} loaded (entry {:#x})", i, entry);
        }
        unsafe { FA = Some(fa) };
        let first = unsafe { sched().current() }.expect("a ready process").0;
        unsafe { CURRENT = first };
        let _ = writeln!(
            con,
            "[proc] starting scheduler at process {} (round-robin)\n",
            first
        );
        // Hand off to the scheduler: this resumes process `first` in user mode, and the
        // trap handler drives every switch thereafter. Never returns here.
        let p = unsafe { proc_at(first) };
        unsafe { A::resume(p.token, &p.frame) };
    }

    let _ = writeln!(con, "\nrustproof: BOOT OK (no user image)");
    A::exit(true)
}

/// The scheduler-aware trap handler — the arch entry stub calls this (via the
/// `rustproof_syscall_trap` symbol the thin bin exports) with a pointer to the
/// [`UserFrame`]-shaped register save the stub just built on the kernel stack. It persists
/// the running process's state, services the syscall / `YIELD` / `EXIT`, picks the next
/// ready process, and resumes it. Never returns (it re-enters user mode via `A::resume`,
/// or halts the guest when the last process exits).
///
/// # Safety
/// `frame` must point at `A::FRAME_WORDS` valid `u64`s (the on-stack trap frame).
pub unsafe fn syscall_trap<A: Arch>(frame: *mut u64) -> ! {
    let cur = CURRENT;
    // Snapshot the running process's live user state into a local frame, so servicing the
    // syscall never holds a `&mut` to the process table across `hostcontract::dispatch`
    // (which re-borrows the same slot for the capability lookup).
    let mut f = UserFrame::ZERO;
    core::ptr::copy_nonoverlapping(frame, f.0.as_mut_ptr(), A::FRAME_WORDS);

    match A::frame_num(&f) {
        abi::sysno::YIELD => {
            // Round-robin to the next ready process (the same one if it is alone).
            CURRENT = sched().next().map(|t| t.0).unwrap_or(cur);
        }
        abi::sysno::EXIT => {
            let code = A::frame_arg(&f, 0);
            let mut con = Console::<A>::new();
            let _ = writeln!(con, "[kernel] proc {} exited with code {}", cur, code);
            proc_at(cur).active = false;
            sched().remove(abi::ThreadId(cur));
            match sched().current() {
                None => {
                    // The last process has exited: the run is done.
                    let _ = writeln!(con, "\nrustproof: BOOT OK");
                    A::exit(true);
                }
                Some(t) => CURRENT = t.0,
            }
        }
        num => {
            // A host-contract syscall: serviced under the current process's page tables
            // (still active — we have not switched) with its own capability space. CURRENT
            // is left unchanged, so the same process resumes with the result in `rax`/`a0`.
            let a = [
                A::frame_arg(&f, 0),
                A::frame_arg(&f, 1),
                A::frame_arg(&f, 2),
                A::frame_arg(&f, 3),
                A::frame_arg(&f, 4),
            ];
            let mut env = KEnv::<A> {
                proc_idx: cur,
                _p: PhantomData,
            };
            let ret = hostcontract::dispatch(&mut env, num, a[0], a[1], a[2], a[3], a[4]);
            A::frame_set_ret(&mut f, ret);
        }
    }

    // Persist `cur`'s (possibly result-updated) frame, then resume whoever is now current.
    proc_at(cur).frame = f;
    let next = CURRENT;
    let token = proc_at(next).token;
    A::resume(token, &proc_at(next).frame)
}

/// The real `HostEnv`, backed by the running process's capability space + kernel state and
/// the current `Arch`'s user-memory access.
struct KEnv<A> {
    /// Index of the process on whose behalf the syscall is serviced.
    proc_idx: usize,
    _p: PhantomData<A>,
}

impl<A: Arch> abi::HostEnv for KEnv<A> {
    fn debug_write(&mut self, bytes: &[u8]) {
        A::console_write(bytes);
    }
    fn gpu_info(&self) -> abi::GpuInfo {
        GPU_INFO
    }
    fn cap_lookup(&self, cap: abi::CapId) -> Option<(abi::CapType, abi::CapRights, u64)> {
        // SAFETY: single-CPU; no other live borrow of this process slot during dispatch.
        let caps = unsafe { &proc_at(self.proc_idx).caps };
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

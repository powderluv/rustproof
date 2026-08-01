//! hal — the hardware-abstraction traits the generic nucleus (`kernel` crate) runs on.
//!
//! Pure traits over `abi`; the concrete per-arch implementations (and the newtype
//! [`Space`] wrappers over the arch page-table crates) live in the `kernel` crate. This
//! keeps `hal` dependency-light and orphan-rule-clean.
#![no_std]

use abi::{FrameAllocator, MemoryRegion, PhysAddr, VirtAddr};

/// Architecture-neutral page permissions; each arch maps these onto its PTE flag bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Perms {
    pub write: bool,
    pub exec: bool,
    pub user: bool,
}

impl Perms {
    pub const KERNEL_RW: Perms = Perms {
        write: true,
        exec: false,
        user: false,
    };
    pub const USER_RW: Perms = Perms {
        write: true,
        exec: false,
        user: true,
    };
    /// User-readable only: no write, no execute. What a device window mapped through a
    /// capability that carries `READ` but not `WRITE` must get.
    pub const USER_RO: Perms = Perms {
        write: false,
        exec: false,
        user: true,
    };
    pub const USER_RX: Perms = Perms {
        write: false,
        exec: true,
        user: true,
    };

    /// Permissions for a loaded ELF segment (`PF_W` / `PF_X`), always user + readable.
    #[inline]
    pub const fn from_elf(pf_w: bool, pf_x: bool) -> Perms {
        Perms {
            write: pf_w,
            exec: pf_x,
            user: true,
        }
    }
}

/// The saved user register state of a process — an opaque, fixed-size POD large enough
/// for either ISA's trap frame. Each arch casts it to its concrete `TrapFrame` layout;
/// keeping it concrete (not a generic/associated type) lets the process table be a plain
/// non-generic static. Only [`Arch`]'s frame methods interpret the contents.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct UserFrame(pub [u64; 40]);

impl UserFrame {
    /// An all-zero frame (never resumed as-is; overwritten by [`Arch::frame_init`]).
    pub const ZERO: UserFrame = UserFrame([0; 40]);
}

impl Default for UserFrame {
    fn default() -> Self {
        Self::ZERO
    }
}

/// An address space (page-table tree) an image can be mapped into.
pub trait Space: Sized {
    /// Allocate + zero a fresh root table.
    fn create(fa: &mut dyn FrameAllocator) -> Option<Self>;
    /// Map one 4 KiB page `va -> pa` with `perms`, allocating intermediate tables from `fa`.
    fn map_page(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        perms: Perms,
        fa: &mut dyn FrameAllocator,
    ) -> bool;
    /// Remove the mapping at `va`, if any. Returns the physical address that was mapped.
    fn unmap_page(&mut self, va: VirtAddr) -> Option<PhysAddr>;
    /// Walk `va` to its physical address (if mapped).
    fn translate(&self, va: VirtAddr) -> Option<PhysAddr>;
    /// The value to load into the paging base register (`cr3` on x86, `satp` on RISC-V).
    fn token(&self) -> u64;
    /// Wrap an ALREADY-LIVE space named by `token` (a `cr3`/`satp` value) for further
    /// mapping. Used to install a mapping into a process's space from a syscall, where the
    /// kernel holds the token rather than the original `Space` value.
    ///
    /// # Safety
    /// `token` must name a live page-table tree whose frames are reachable (identity map).
    unsafe fn from_token(token: u64) -> Self;

    /// Copy the kernel's shared mappings (from `kernel_token`) into this user space, so the
    /// trap/syscall path stays reachable while this space is active. Kernel pages carry no
    /// user bit, so user mode still cannot reach them.
    ///
    /// # Safety
    /// `kernel_token` must be the active kernel space's token; the root tables must be
    /// reachable at their physical addresses (identity map).
    unsafe fn share_kernel(&mut self, kernel_token: u64);
}

/// The per-arch hardware surface the generic kernel is written against.
pub trait Arch {
    /// The arch's address-space type.
    type Space: Space;

    const NAME: &'static str;
    /// Base of the user virtual-address window (never overlaps kernel mappings).
    const USER_BASE: u64;
    /// Exclusive upper bound of the user window (for pointer validation).
    const USER_LIMIT: u64;
    /// Top of the user stack (grows down); [`USER_STACK_PAGES`](Self::USER_STACK_PAGES) below it are mapped.
    const USER_STACK_TOP: u64;
    const USER_STACK_PAGES: u64;
    /// Base of the per-process window where device (MMIO) mappings are installed. Sits
    /// between the loaded image and the stack, so it collides with neither.
    const USER_MMIO_BASE: u64;

    /// Emit raw bytes to the debug console.
    fn console_write(bytes: &[u8]);
    /// Shut the guest down (`success` -> clean/zero exit).
    fn exit(success: bool) -> !;
    /// Install the trap/interrupt vector(s).
    fn init_traps();
    /// Fill `out` with the usable physical memory regions from the boot args (`a0`,`a1`).
    fn memory_map(a0: u64, a1: u64, out: &mut [MemoryRegion]) -> usize;
    /// Bytes below which physical memory is reserved (kernel image + firmware).
    fn reserve_below() -> u64;
    /// DMA-capable pool ceiling.
    fn dma_top() -> u64;
    /// Ensure paging is on (build+enable it if needed) and return the kernel space token.
    fn setup_paging(fa: &mut dyn FrameAllocator) -> u64;
    /// Load a user ELF into `space` (via the arch's loader), returning its entry VA.
    fn load_user(elf: &[u8], space: &mut Self::Space, fa: &mut dyn FrameAllocator) -> Option<u64>;

    // ---- trap-frame / scheduling surface -------------------------------------------

    /// How many `u64` words the trap stub saves into a [`UserFrame`] (the arch's
    /// `TrapFrame` size). The generic trap handler copies exactly this many words out of
    /// the on-stack frame the stub built.
    const FRAME_WORDS: usize;

    /// Build the initial frame for a fresh process: enter `_start` at `entry` on stack
    /// `sp`, with `arg0` in the first-argument register (used to hand each process its id).
    fn frame_init(entry: u64, sp: u64, arg0: u64) -> UserFrame;

    /// The syscall number the user requested (from the frame's number register).
    fn frame_num(f: &UserFrame) -> u64;
    /// Syscall argument `i` (0..=4) from the frame's argument registers.
    fn frame_arg(f: &UserFrame, i: usize) -> u64;
    /// Set the syscall return value the user will observe on resume.
    fn frame_set_ret(f: &mut UserFrame, v: u64);
    /// Set a SECOND return value, in a register distinct from the status one. Needed by any
    /// syscall that returns both a status and an unrestricted 64-bit payload (`RECV`): the
    /// payload domain is the whole `u64`, so it cannot share a register with the `syserr`
    /// sentinels without the two becoming indistinguishable. User stubs for such calls must
    /// declare this register as an asm output.
    fn frame_set_ret2(f: &mut UserFrame, v: u64);
    /// Set a THIRD return value, in yet another distinct register (the `a3` argument
    /// register). Needed by `RECV`, which returns a status, an unrestricted payload word,
    /// and a byte count that must not be confused with either.
    fn frame_set_ret3(f: &mut UserFrame, v: u64);

    /// Make `token`'s address space the active one WITHOUT resuming a process — so the
    /// kernel can touch that space's user memory (e.g. deliver an IPC payload into a
    /// receiver that is not the process currently running). The kernel's own mappings are
    /// shared into every space, so kernel code and data stay reachable across the switch.
    ///
    /// # Safety
    /// `token` must name a live space that shares the kernel mappings. The caller must not
    /// rely on the previous space's user memory afterwards.
    unsafe fn activate(token: u64);

    /// Enable preemptive scheduling: start a periodic timer interrupt that drives the
    /// scheduler (so a non-cooperating process is time-sliced). A no-op on arches where
    /// preemption is not yet wired (they remain cooperative). Called once, after
    /// [`init_traps`](Self::init_traps), before the first process runs.
    fn start_preemption();

    /// Enable the CONSOLE device's receive interrupt and route it to the nucleus's device
    /// handler. Called once at boot, after [`start_preemption`](Self::start_preemption).
    ///
    /// This is the kernel's second interrupt source and its only *quiet* one: the timer
    /// fires whether or not anything happened, so a process blocked on it always wakes on
    /// its own. A console byte arrives only when something really happens, which is what a
    /// driver waiting on its device is doing — and it is the only way the idle park is ever
    /// ended by something other than the clock.
    fn start_console_irq();

    /// Acknowledge a console interrupt: DRAIN the device, then end-of-interrupt at the
    /// controller. Draining is not optional — a UART holds its interrupt asserted until its
    /// receive buffer is empty, so an unread byte re-raises the line forever.
    fn console_irq_ack();

    /// Park the CPU until an interrupt arrives, with interrupts ENABLED — the state to be
    /// in when every process is waiting on one. Resets the stack pointer to a dedicated
    /// idle stack first, because an interrupt taken while already in the kernel pushes onto
    /// the current stack: without the reset, each tick would grow it without bound.
    ///
    /// Never returns: the interrupt handler decides what runs next (another process, or
    /// this again).
    ///
    /// # Safety
    /// Callers must have no live borrows of anything on the current kernel stack, since it
    /// is abandoned.
    unsafe fn idle() -> !;

    /// Acknowledge the current interrupt at the interrupt controller (end-of-interrupt),
    /// so the next timer tick can be delivered. Called from the timer handler before it
    /// resumes a process. A no-op where [`start_preemption`](Self::start_preemption) is.
    fn end_of_interrupt();

    /// Load `token` into the paging base register and resume the user state in `frame`
    /// (`iretq` / `sret`). Never returns.
    ///
    /// # Safety
    /// `token` must name a valid user space that shares the kernel mappings; `frame` must
    /// hold a coherent user register state whose `rip`/`sp` are mapped user-accessible in
    /// that space. The `frame` pointer must stay valid (it lives in the process table).
    unsafe fn resume(token: u64, frame: &UserFrame) -> !;

    /// Copy into the current user space at `uptr` (validated + arch-permitted).
    /// # Safety: caller ensures the user space is active.
    unsafe fn copy_to_user(uptr: u64, bytes: &[u8]) -> bool;
    /// Copy from the current user space at `uptr`.
    /// # Safety: caller ensures the user space is active.
    unsafe fn copy_from_user(uptr: u64, out: &mut [u8]) -> bool;
    /// True if `[uptr, uptr+len)` lies within the user window.
    fn user_ptr_ok(uptr: u64, len: usize) -> bool;
    /// True if `[uptr, uptr+len)` is not merely in range but actually MAPPED writable and
    /// user-accessible in the currently active space — i.e. a [`copy_to_user`](Self::copy_to_user)
    /// there would succeed. Lets a syscall vet a buffer up front instead of discovering the
    /// problem during a copy it can no longer report cleanly.
    fn user_write_ok(uptr: u64, len: usize) -> bool;
}

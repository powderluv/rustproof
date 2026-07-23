//! nucleus — the bootable Rustproof guest kernel image.
//!
//! M0 core: PVH entry -> long-mode trampoline (src/boot.s) -> `kmain`, which brings up
//! serial + the IDT, then exercises the parallel M0-core crates end to end:
//!   * `mm`           — parse the PVH memory map, init the bitmap frame allocator;
//!   * `vspace`       — build a fresh address space, map/translate/unmap a page;
//!   * `capabilities` — derive a capability with reduced rights (authority-monotonic);
//!   * `ipc`          — a synchronous endpoint send/recv;
//!   * `sched`        — a real cooperative context switch into a second kernel thread.
//! See docs/milestone-M0.md.
#![no_std]
#![no_main]

mod pvh;

use abi::{
    CapRights, CapType, FrameAllocator, MemoryKind, MemoryRegion, MessageInfo, ThreadId, VirtAddr,
};
use arch_x86_64::{kprintln, qemu};
use sched::Context;

// The 32->64-bit boot trampoline + PVH note. Provides `_start`; calls `kmain`.
core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

// ---- sched demo: a second kernel thread with its own stack + saved context ----
static mut MAIN_CTX: Context = Context::new();
static mut B_CTX: Context = Context::new();
static mut B_STACK: [u8; 16 * 1024] = [0; 16 * 1024];

extern "C" fn thread_b() -> ! {
    kprintln!("  [thread B] running on its own stack — switching back to main");
    // Return control to main by switching back to its saved context.
    unsafe {
        sched::switch(
            core::ptr::addr_of_mut!(B_CTX),
            core::ptr::addr_of!(MAIN_CTX),
        )
    };
    // Unreachable: main never switches back to B in this demo.
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

// ---- frame-allocator bitmap storage (heap-free; the loader zero-fills .bss) ----
static mut BITMAP: [u64; 8192] = [0; 8192]; // one bit per 4 KiB frame; covers 2 GiB

/// 64-bit Rust entry, called by the trampoline with the PVH `start_info` pointer.
#[no_mangle]
pub extern "C" fn kmain(start_info: u64) -> ! {
    unsafe { arch_x86_64::serial::Serial::init() };
    kprintln!();
    kprintln!("Rustproof nucleus — M0 core");
    kprintln!("  long mode + COM1 serial + identity map (low 1 GiB)");
    kprintln!("  PVH start_info @ {:#018x}", start_info);

    arch_x86_64::interrupts::init();
    kprintln!("  IDT loaded (32 CPU exception vectors)");

    #[cfg(feature = "provoke-fault")]
    {
        kprintln!("provoke-fault: reading unmapped 0xDEADBEEF to force a #PF");
        let bad = 0xDEAD_BEEF_usize as *const u32;
        let _ = unsafe { core::ptr::read_volatile(bad) };
    }

    // ---------------- mm: memory map + frame allocator ----------------
    let mut regions = [MemoryRegion {
        start: 0,
        len: 0,
        kind: MemoryKind::Reserved,
    }; 32];
    let nr = pvh::memory_map(start_info, &mut regions);
    let regions = &regions[..nr];
    let usable: u64 = regions
        .iter()
        .filter(|r| r.kind == MemoryKind::Usable)
        .map(|r| r.len)
        .sum();
    kprintln!();
    kprintln!(
        "[mm] PVH map: {} region(s), {} MiB usable (<= 1 GiB window)",
        nr,
        usable >> 20
    );

    let words = mm::BitmapAllocator::bitmap_words_needed(regions);
    let bitmap: &'static mut [u64] = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(BITMAP) as *mut u64, words)
    };
    // Reserve the low 8 MiB (kernel image + low structures); DMA pool below 16 MiB.
    let mut fa = mm::BitmapAllocator::new(regions, bitmap, 0x80_0000, 0x100_0000);
    kprintln!(
        "[mm] {} frames tracked, {} free",
        fa.total_frames(),
        fa.free_count()
    );
    let f0 = fa.alloc_frame();
    let f1 = fa.alloc_frame();
    let dma = fa.alloc_dma_frame();
    kprintln!(
        "[mm] alloc -> {:#x}, {:#x}; dma -> {:#x}; free now {}",
        f0.map(|p| p.as_u64()).unwrap_or(0),
        f1.map(|p| p.as_u64()).unwrap_or(0),
        dma.map(|p| p.as_u64()).unwrap_or(0),
        fa.free_count()
    );
    if let Some(p) = f0 {
        fa.free_frame(p);
    }
    kprintln!("[mm] freed one -> free now {}", fa.free_count());

    // ---------------- vspace: page tables ----------------
    kprintln!();
    if let Some(mut aspace) =
        vspace::AddressSpace::create(0 /* identity phys_offset */, &mut fa)
    {
        let va = VirtAddr(0x5000_0000);
        let target = fa.alloc_frame().expect("a frame for the test mapping");
        let flags = vspace::PageFlags::PRESENT | vspace::PageFlags::WRITABLE;
        match aspace.map(va, target, flags, &mut fa) {
            Ok(()) => {
                let t = aspace.translate(va).map(|p| p.as_u64()).unwrap_or(0);
                kprintln!(
                    "[vspace] new AS pml4 @ {:#x}; map {:#x} -> {:#x}; translate -> {:#x}",
                    aspace.pml4_phys().as_u64(),
                    va.as_u64(),
                    target.as_u64(),
                    t
                );
                let u = aspace.unmap(va).map(|p| p.as_u64()).unwrap_or(0);
                let after = if aspace.translate(va).is_none() {
                    "unmapped"
                } else {
                    "STILL MAPPED?!"
                };
                kprintln!("[vspace] unmap -> {:#x}; translate now -> {}", u, after);
            }
            Err(_) => kprintln!("[vspace] map failed"),
        }
    } else {
        kprintln!("[vspace] could not create address space (out of frames)");
    }

    // ---------------- capabilities: authority-monotonic derivation ----------------
    kprintln!();
    let mut caps = capabilities::CapSpace::<64>::new();
    let root = caps
        .insert(CapType::Untyped, CapRights::ALL, 0xF00D)
        .expect("root cap");
    let child = caps.derive(root, CapRights::READ).expect("read-only child");
    // Deriving WRITE from a READ-only cap intersects rights, so WRITE is dropped
    // (the child can never pass on authority it does not hold).
    let escalate = caps.derive(child, CapRights::WRITE);
    let escalated_rights = escalate
        .and_then(|c| caps.lookup(c))
        .map(|s| s.rights.0)
        .unwrap_or(0);
    kprintln!(
        "[cap] root rights={:#05b}, READ-only child rights={:#05b}",
        caps.lookup(root).unwrap().rights.0,
        caps.lookup(child).unwrap().rights.0
    );
    kprintln!(
        "[cap] derive WRITE from READ-only child -> rights={:#05b} ({})",
        escalated_rights,
        if escalated_rights & CapRights::WRITE.0 == 0 {
            "WRITE dropped — authority-monotonic"
        } else {
            "ESCALATED?! (bug)"
        }
    );
    caps.revoke_subtree(root);
    let freed = caps
        .lookup(root)
        .map(|s| s.cap_type == CapType::Null)
        .unwrap_or(true);
    kprintln!(
        "[cap] revoke_subtree(root) -> root {}",
        if freed { "freed" } else { "still live?! (bug)" }
    );

    // ---------------- ipc: synchronous endpoint ----------------
    kprintln!();
    let mut ep = ipc::Endpoint::<8>::new();
    let waiting = ep.recv(ThreadId(2)); // no sender yet -> Block
    kprintln!("[ipc] T2 recv with no sender -> {:?}", waiting);
    let words = [0xCAFE_u64, 0xF00D_u64];
    match ep.send(ThreadId(1), MessageInfo::new(0x42, 2), &words) {
        ipc::IpcAction::Deliver {
            to,
            from,
            msg,
            words,
            ..
        } => kprintln!(
            "[ipc] T1 send -> Deliver to T{} from T{} label={:#x} w0={:#x}",
            to.0,
            from.0,
            msg.label,
            words[0]
        ),
        other => kprintln!("[ipc] unexpected action: {:?}", other),
    }

    // ---------------- sched: real cooperative context switch ----------------
    kprintln!();
    kprintln!("[sched] preparing thread B and switching (real context switch)");
    unsafe {
        let top = VirtAddr(core::ptr::addr_of!(B_STACK) as u64 + 16 * 1024);
        B_CTX = Context::prepare(top, thread_b);
        sched::switch(
            core::ptr::addr_of_mut!(MAIN_CTX),
            core::ptr::addr_of!(B_CTX),
        );
    }
    kprintln!("  [main] resumed after thread B returned via context switch");

    kprintln!();
    kprintln!("rustproof: BOOT OK");
    qemu::exit(qemu::EXIT_SUCCESS);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("nucleus PANIC: {}", info);
    qemu::exit(qemu::EXIT_FAILURE);
}

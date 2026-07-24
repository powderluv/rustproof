//! nucleus-riscv — the bootable Rustproof guest kernel image for RISC-V (rv64gc).
//!
//! RV-M0 core: OpenSBI -> `_start` (arch-riscv64 boot asm) -> `kmain`, which brings up
//! the NS16550A console + the supervisor trap vector, then exercises the SAME portable
//! M0-core crates the x86 nucleus uses, UNCHANGED:
//!   * `mm`           — build the QEMU-virt memory map, init the bitmap frame allocator;
//!   * `capabilities` — derive a capability with reduced rights (authority-monotonic);
//!   * `ipc`          — a synchronous endpoint recv (block) then send (deliver).
//! Runs in BARE mode (satp = 0, physical addressing) — no paging in this MVP.
//! See docs/riscv-port.md.
#![no_std]
#![no_main]

mod paging;
mod userland;

use abi::{CapRights, CapType, FrameAllocator, MemoryKind, MemoryRegion, MessageInfo, ThreadId};
use arch_riscv64::{kprintln, qemu};

/// The `riscv-init` user program, staged by `tools/run-qemu-riscv.sh` (empty until then —
/// `build.rs` guarantees the file exists so `include_bytes!` compiles).
static USER_ELF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/user.elf"));

// The boot entry (`_start`) + boot stack live in arch-riscv64; it calls `kmain` below.

// ---- frame-allocator bitmap storage (heap-free; the _start bss loop zero-fills it) ----
// One bit per 4 KiB frame. Sized to cover the whole QEMU-virt RAM window up to
// 0xA000_0000 (0xA_0000 frames -> 10240 u64 words); 12288 leaves generous headroom.
static mut BITMAP: [u64; 12288] = [0; 12288];

/// S-mode Rust entry, called by the boot trampoline with OpenSBI's `a0`/`a1`.
#[no_mangle]
pub extern "C" fn kmain(hartid: u64, dtb: u64) -> ! {
    kprintln!();
    kprintln!("Rustproof nucleus (riscv64) — RV-M0");
    kprintln!("  S-mode boot via OpenSBI; NS16550A serial @ 0x1000_0000");
    kprintln!("  hartid = {}, DTB @ {:#018x}", hartid, dtb);

    arch_riscv64::interrupts::init();
    kprintln!("  stvec installed (trap vector, Direct mode)");

    // ---------------- mm: memory map + bitmap frame allocator ----------------
    // QEMU virt with -m 512M: RAM 0x8000_0000..0xA000_0000. Reserve the low 4 MiB for
    // OpenSBI + the kernel image; the rest is Usable.
    let regions = [MemoryRegion {
        start: 0x8040_0000,
        len: 0xA000_0000 - 0x8040_0000,
        kind: MemoryKind::Usable,
    }];
    let words = mm::BitmapAllocator::bitmap_words_needed(&regions);
    // SAFETY: single-threaded boot; we form one exclusive slice over the static bitmap.
    let bitmap: &'static mut [u64] = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(BITMAP) as *mut u64, words)
    };
    // reserve_below = 0x8040_0000 (below the usable window); DMA pool below 0x8800_0000.
    let mut fa = mm::BitmapAllocator::new(&regions, bitmap, 0x8040_0000, 0x8800_0000);
    kprintln!();
    kprintln!(
        "[mm] {} frames tracked, {} free ({} MiB usable)",
        fa.total_frames(),
        fa.free_count(),
        (fa.free_count() as u64 * abi::PAGE_SIZE) >> 20
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

    // ---------------- capabilities: authority-monotonic derivation ----------------
    kprintln!();
    let mut caps = capabilities::CapSpace::<64>::new();
    let root = caps
        .insert(CapType::Untyped, CapRights::ALL, 0xF00D)
        .expect("root cap");
    let child = caps.derive(root, CapRights::READ).expect("read-only child");
    // Deriving WRITE from a READ-only cap intersects rights, so WRITE is dropped — a
    // child can never pass on authority it does not itself hold.
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

    // ---------------- RV-M1: enable Sv39 paging ----------------
    kprintln!();
    let ksatp = unsafe { paging::build_kernel_identity(&mut fa) };
    unsafe { arch_riscv64::mmu::enable_paging(ksatp) };
    kprintln!(
        "[paging] Sv39 on: satp={:#018x} (3 GiB identity via gigapages)",
        ksatp
    );
    if let Some(mut aspace) = vspace_riscv::AddressSpace::create(0, &mut fa) {
        let va = abi::VirtAddr(0x1_0000_0000);
        let target = fa.alloc_frame().expect("test frame");
        let flags =
            vspace_riscv::PageFlags::V | vspace_riscv::PageFlags::R | vspace_riscv::PageFlags::W;
        match aspace.map(va, target, flags, &mut fa) {
            Ok(()) => {
                kprintln!(
                    "[vspace] new AS satp={:#x}; map {:#x} -> {:#x}; translate -> {:#x}",
                    aspace.satp(),
                    va.as_u64(),
                    target.as_u64(),
                    aspace.translate(va).map(|p| p.as_u64()).unwrap_or(0)
                );
                aspace.unmap(va);
                kprintln!(
                    "[vspace] unmap -> translate now {}",
                    if aspace.translate(va).is_none() {
                        "unmapped"
                    } else {
                        "STILL MAPPED?!"
                    }
                );
            }
            Err(_) => kprintln!("[vspace] map failed"),
        }
    }

    // ---------------- RV-M2: userland (U-mode + ecall host contract) ----------------
    if USER_ELF.len() >= 64 {
        kprintln!();
        kprintln!(
            "[user] loading riscv-init ({} bytes) into a fresh address space",
            USER_ELF.len()
        );
        // Never returns: riscv-init runs in U-mode and its EXIT ecall prints the banner.
        unsafe { userland::setup_and_enter(USER_ELF, fa, ksatp) }
    }

    kprintln!();
    kprintln!("rustproof: RV BOOT OK (no user image)");
    qemu::exit_success();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("nucleus-riscv PANIC: {}", info);
    qemu::exit_fail();
}

//! GDT with kernel + user segments and a 64-bit TSS — the descriptor layout `syscall`
//! and ring-3 need.
//!
//! Layout (selector -> descriptor):
//!   0x00 null · 0x08 kernel code64 · 0x10 kernel data ·
//!   0x18 user code32 (placeholder for the sysret layout) · 0x20 user data ·
//!   0x28 user code64 · 0x30 TSS (16-byte descriptor spanning two slots).
//! This matches the STAR MSR programmed in [`crate::syscall`].
use core::arch::asm;
use core::mem::size_of;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iopb: u16,
}

impl Tss {
    const fn new() -> Self {
        Tss {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iopb: size_of::<Tss>() as u16, // no I/O bitmap
        }
    }
}

#[repr(C, align(16))]
struct Stack([u8; 16 * 1024]);

static mut TSS: Tss = Tss::new();
// Ring-3 -> ring-0 (fault/interrupt) stack, loaded into TSS.rsp0.
static mut FAULT_STACK: Stack = Stack([0; 16 * 1024]);

// 8 quadwords: 6 segment descriptors + a 2-slot TSS descriptor.
static mut GDT: [u64; 8] = [
    0,
    0x00AF_9A00_0000_FFFF, // 0x08 kernel code64  (P, DPL0, code, L=1)
    0x00CF_9200_0000_FFFF, // 0x10 kernel data    (P, DPL0, data, DB)
    0x00CF_FA00_0000_FFFF, // 0x18 user code32    (P, DPL3, code)  [sysret placeholder]
    0x00CF_F200_0000_FFFF, // 0x20 user data      (P, DPL3, data)
    0x00AF_FA00_0000_FFFF, // 0x28 user code64    (P, DPL3, code, L=1)
    0,                     // 0x30 TSS low  (filled in init)
    0,                     // 0x38 TSS high
];

#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

/// The ring-0 stack pointer the CPU loads on a ring-3 -> ring-0 trap (TSS.rsp0).
pub fn kernel_fault_stack_top() -> u64 {
    core::ptr::addr_of!(FAULT_STACK) as u64 + size_of::<Stack>() as u64
}

/// Install the GDT + TSS and load the task register. Kernel CS/data selectors are
/// unchanged (0x08 / 0x10) so no far reload is needed.
pub fn init() {
    unsafe {
        let tss = core::ptr::addr_of_mut!(TSS);
        (*tss).rsp[0] = kernel_fault_stack_top();

        let base = tss as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        let low = (limit & 0xFFFF)
            | ((base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40) // present, type = available 64-bit TSS
            | (((limit >> 16) & 0xF) << 48)
            | (((base >> 24) & 0xFF) << 56);
        let high = (base >> 32) & 0xFFFF_FFFF;

        let gdt = core::ptr::addr_of_mut!(GDT);
        (*gdt)[6] = low;
        (*gdt)[7] = high;

        let gdtr = Gdtr {
            limit: (size_of::<[u64; 8]>() - 1) as u16,
            base: gdt as u64,
        };
        asm!("lgdt [{}]", in(reg) &gdtr, options(readonly, nostack, preserves_flags));
        asm!("ltr {0:x}", in(reg) 0x30u16, options(nostack, preserves_flags));
    }
}

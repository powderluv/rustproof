/* Rustproof nucleus boot trampoline.
 *
 * Entered via the PVH boot protocol (QEMU/libvirt -kernel): the hypervisor jumps
 * to `_start` in 32-bit protected mode, paging off, flat segments, with %ebx ->
 * the PVH hvm_start_info struct. We identity-map the low 1 GiB with 2 MiB pages,
 * switch to 64-bit long mode, and call the Rust entry `kmain(start_info)`.
 *
 * AT&T syntax (see options(att_syntax) at the global_asm! call site).
 */

/* ---- 32-bit entry ---- */
.section .text
.code32
.global _start
_start:
    cli
    cld
    movl $stack_top, %esp
    movl %ebx, (start_info_ptr)      /* save PVH start_info pointer */

    /* PML4[0] = pdpt | present | writable */
    movl $pdpt, %eax
    orl  $0x3, %eax
    movl %eax, (pml4)
    movl $0, (pml4 + 4)

    /* PDPT[0] = pd | present | writable */
    movl $pd, %eax
    orl  $0x3, %eax
    movl %eax, (pdpt)
    movl $0, (pdpt + 4)

    /* PD[i] = i*2MiB | present | writable | huge, for i in 0..512 (1 GiB) */
    movl $pd, %edi
    movl $0x83, %eax
    movl $512, %ecx
.fill_pd:
    movl %eax, (%edi)
    movl $0, 4(%edi)
    addl $0x200000, %eax
    addl $8, %edi
    loop .fill_pd

    /* CR3 = pml4 */
    movl $pml4, %eax
    movl %eax, %cr3

    /* CR4.PAE = 1 (0x20) | CR4.TSD = 1 (0x04)
     *
     * TSD makes `rdtsc` privileged. Without it every ring-3 process — including the
     * least-authority producer, which holds one send-only endpoint capability and
     * nothing else — has a free-running nanosecond clock that no capability gates.
     * riscv denies the same observation by leaving `scounteren` clear, so without this
     * the two arches disagreed about whether reading elapsed time is authority, and
     * nothing asserted either. Denying is the reversible direction: a process that
     * needs a clock can be given one deliberately. */
    movl %cr4, %eax
    orl  $0x24, %eax
    movl %eax, %cr4

    /* EFER.LME = 1 */
    movl $0xC0000080, %ecx
    rdmsr
    orl  $0x100, %eax
    wrmsr

    /* CR0.PG | CR0.PE */
    movl %cr0, %eax
    orl  $0x80010001, %eax          // PG | WP | PE — WP makes ring-0 stores honour R/W
    movl %eax, %cr0

    lgdt (gdt64_ptr)
    ljmp $0x08, $long_mode

/* ---- 64-bit entry ---- */
.code64
long_mode:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs
    movq $stack_top, %rsp
    movl (start_info_ptr), %edi      /* arg0 = start_info (zero-extended to 64 bits) */
    call kmain
.hang:
    hlt
    jmp .hang

/* ---- 64-bit GDT: null, kernel code (L=1), kernel data ---- */
.section .rodata
.align 16
gdt64:
    .quad 0x0000000000000000
    .quad 0x00AF9A000000FFFF
    .quad 0x00AF92000000FFFF
gdt64_end:
gdt64_ptr:
    .word gdt64_end - gdt64 - 1
    .long gdt64

/* ---- page tables + stack (zeroed .bss) ---- */
.section .bss
.align 4096
pml4: .skip 4096
pdpt: .skip 4096
pd:   .skip 4096
.align 8
start_info_ptr: .skip 8
.align 16
.skip 0x4000
stack_top:

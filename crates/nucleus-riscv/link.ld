/* Rustproof RISC-V nucleus image layout. Non-PIE, fixed load at 0x8020_0000 where
 * OpenSBI drops the kernel and jumps to _start in S-mode. The boot stack lives in
 * .bss (a static in arch-riscv64), so it is reserved and zeroed with the rest of
 * .bss by the _start bss loop between __bss_start and __bss_end. */

ENTRY(_start)

SECTIONS {
    . = 0x80200000;

    .text : {
        *(.text._start)         /* boot entry first */
        *(.text.trap)           /* trap vector (4-byte aligned) */
        *(.text .text.*)
    }

    . = ALIGN(4K);
    .rodata : {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    . = ALIGN(4K);
    .data : {
        *(.data .data.*)
        *(.sdata .sdata.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.*)
    }

    . = ALIGN(8);
    PROVIDE(__bss_start = .);
    .bss : {
        *(.sbss .sbss.*)
        *(.bss .bss.*)          /* includes arch-riscv64's BOOT_STACK */
        *(COMMON)
    }
    . = ALIGN(8);
    PROVIDE(__bss_end = .);

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
    }
}

/* riscv-init -- RISC-V U-mode user program image layout. Non-PIE ET_EXEC linked
 * at the USER virtual base 0x1_0000_0000 (4 GiB). The kernel loader maps these
 * PT_LOAD segments into the process address space at their link-time virtual
 * addresses and enters at _start. Segments are page-granular with distinct
 * R/W/X flags.
 *
 * Unlike the x86-64 `init` image (base 512 GiB, which needs the large code
 * model), RISC-V reaches `.rodata`/`.data` symbols PC-relative via `auipc`
 * under the default medium (medany) code model, so 4 GiB needs no special
 * addressing: code and data are adjacent in the image and every cross-section
 * reference is a tiny PC-relative offset. */

ENTRY(_start)

PHDRS {
    text   PT_LOAD FLAGS(5);   /* R + X */
    rodata PT_LOAD FLAGS(4);   /* R     */
    data   PT_LOAD FLAGS(6);   /* R + W */
}

SECTIONS {
    . = 0x100000000;

    .text : { *(.text .text.*) } :text

    . = ALIGN(4K);
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(4K);
    .data : { *(.data .data.*) } :data
    .bss  : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note.gnu.*)
        *(.comment)
    }
}

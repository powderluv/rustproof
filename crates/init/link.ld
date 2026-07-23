/* init -- ring-3 user program image layout. Non-PIE ET_EXEC linked at the USER
 * virtual base 0x80_0000_0000 (512 GiB). The kernel loader maps these PT_LOAD
 * segments into the process address space at their link-time virtual addresses
 * and enters at _start. Segments are page-granular with distinct R/W/X flags. */

ENTRY(_start)

PHDRS {
    text   PT_LOAD FLAGS(5);   /* R + X */
    rodata PT_LOAD FLAGS(4);   /* R     */
    data   PT_LOAD FLAGS(6);   /* R + W */
}

SECTIONS {
    . = 0x8000000000;

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

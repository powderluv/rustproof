/* Rustproof nucleus image layout. Non-PIE, fixed load at 1 MiB. The PVH note
 * must land in its own PT_NOTE segment so the loader can find the 32-bit entry. */

ENTRY(_start)

PHDRS {
    note   PT_NOTE   FLAGS(4);          /* R   */
    text   PT_LOAD   FLAGS(5);          /* R+X */
    rodata PT_LOAD   FLAGS(4);          /* R   */
    data   PT_LOAD   FLAGS(6);          /* R+W */
}

SECTIONS {
    . = 1M;

    .note.Xen : { KEEP(*(.note.Xen)) } :note

    . = ALIGN(4K);
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

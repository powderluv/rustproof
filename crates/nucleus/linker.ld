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
    /* Multiboot's a.out kludge needs the image extent as absolute addresses: QEMU's ELF64
       loader refuses a 64-bit multiboot image ("give a 32bit one"), so the firmware path
       hands it a FLAT binary described by these. */
    __image_start = .;

    .note.Xen : { KEEP(*(.note.Xen)) } :note
    /* Multiboot header must lie in the first 8 KiB of the image, so it goes first. */
    .multiboot : { KEEP(*(.multiboot)) } :text

    . = ALIGN(4K);
    .text : { *(.text .text.*) } :text

    . = ALIGN(4K);
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(4K);
    .data : { *(.data .data.*) } :data
    __data_end = .;
    .bss  : {
        *(.bss .bss.*)
        *(COMMON)
    } :data
    __bss_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note.gnu.*)
        *(.comment)
    }
}

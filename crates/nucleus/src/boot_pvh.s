/* ---- PVH ELF note: advertises the 32-bit entry point to the loader ---- */
.section .note.Xen, "a"
.align 4
.long 4                 /* namesz: "Xen\0" */
.long 4                 /* descsz: 4-byte entry address */
.long 18                /* type = XEN_ELFNOTE_PHYS32_ENTRY */
.asciz "Xen"
.align 4
.long _start            /* the 32-bit entry (physical address) */
.align 4


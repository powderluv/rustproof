/* Multiboot 1 header — the FIRMWARE boot path.
 *
 * QEMU's `-kernel` takes the PVH route whenever the ELF carries a PVH note, and that route
 * skips firmware: no SeaBIOS, so no PCI enumeration, so every BAR is left unassigned.
 * Measured — `edu` reports `BAR0 at 0xffffffffffffffff` under PVH and `0xfea00000` once
 * SeaBIOS has run. Only firmware assigns BARs, and a device whose registers are unreachable
 * cannot be told to DMA, which is what the containment proof needs.
 *
 * This header REPLACES the PVH note rather than joining it: with both present QEMU picks PVH
 * and firmware is skipped again.
 *
 * The a.out KLUDGE (flag bit 16) is not optional here. QEMU's multiboot loader refuses a
 * 64-bit ELF outright — "Cannot load x86-64 image, give a 32bit one" — and this kernel is an
 * ELF64 whose entry happens to be 32-bit code. The kludge lets the loader take a FLAT image
 * described by absolute addresses instead of parsing ELF, so the rig hands it an objcopy'd
 * binary. The linker supplies the extents.
 *
 * Entry state matches the PVH trampoline by ABI coincidence, not design — both arrive in
 * 32-bit protected mode with the info pointer in %ebx — so `_start` is shared and only the
 * INTERPRETATION differs (crates/kernel/src/multiboot.rs).
 */
.section .multiboot, "a"
.align 4
mb_header:
    .long 0x1BADB002                        /* magic */
    .long 0x00010002                        /* flags: bit1 memory map | bit16 a.out kludge */
    .long -(0x1BADB002 + 0x00010002)        /* checksum: the three must sum to zero */
    .long mb_header                         /* header_addr */
    .long __image_start                     /* load_addr */
    .long __data_end                        /* load_end_addr: the last byte the loader reads
                                               FROM THE FILE. `.got` is placed explicitly ahead
                                               of it in linker.ld — left as an orphan the linker
                                               puts it past `.bss`, outside this extent, and the
                                               GOT is silently never loaded. */
    .long __bss_end                         /* bss_end_addr */
    .long _start                            /* entry_addr */

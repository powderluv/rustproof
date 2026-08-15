//! x86 port I/O primitives.
use core::arch::asm;

/// Write a byte to an I/O port.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

/// Read a byte from an I/O port.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}

/// Write a dword to an I/O port.
#[inline]
pub unsafe fn outl(port: u16, val: u32) {
    asm!("out dx, eax", in("dx") port, in("eax") val, options(nomem, nostack, preserves_flags));
}

/// Read a dword from an I/O port.
///
/// The counterpart to [`outl`], and the missing half of a PCI config-space read: the address
/// goes out to 0xCF8 with `outl`, the data comes back from 0xCFC with this.
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    asm!("in eax, dx", out("eax") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}

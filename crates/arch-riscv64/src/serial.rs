//! Minimal NS16550A UART driver — the nucleus's only output device for the RV-M0
//! boot spike. QEMU's `virt` machine maps a 16550-compatible UART at 0x1000_0000, and
//! OpenSBI has already programmed the line settings, so we only poll + push bytes.
//! Polled, no interrupts.
use core::fmt;

/// MMIO base of the QEMU `virt` NS16550A UART.
const UART_BASE: usize = 0x1000_0000;
/// Transmit Holding Register (write a byte to send). Offset 0.
const THR: usize = 0;
/// Line Status Register. Offset 5.
const LSR: usize = 5;
/// LSR bit 5: transmit-holding register empty (ready to accept a byte).
const LSR_THRE: u8 = 1 << 5;

/// Zero-sized handle to the UART. Stateless: the device holds the state.
pub struct Uart;

impl Uart {
    #[inline]
    fn reg(off: usize) -> *mut u8 {
        (UART_BASE + off) as *mut u8
    }

    #[inline]
    fn transmit_empty() -> bool {
        // SAFETY: fixed MMIO register on the QEMU virt board; volatile 1-byte read.
        unsafe { core::ptr::read_volatile(Self::reg(LSR)) & LSR_THRE != 0 }
    }

    /// Write one byte, spinning until the transmit holding register is free.
    pub fn write_byte(b: u8) {
        while !Self::transmit_empty() {}
        // SAFETY: fixed MMIO register on the QEMU virt board; volatile 1-byte write.
        unsafe { core::ptr::write_volatile(Self::reg(THR), b) };
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                Uart::write_byte(b'\r');
            }
            Uart::write_byte(b);
        }
        Ok(())
    }
}

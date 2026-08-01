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
/// Receive-buffer register (same offset as THR; read instead of write).
const RBR: usize = 0;
/// Interrupt-enable register.
const IER: usize = 1;
/// Line-status bit 0: data ready.
const LSR_DR: u8 = 1;

/// Zero-sized handle to the UART. Stateless: the device holds the state.
pub struct Uart;

impl Uart {
    #[inline]
    fn reg(off: usize) -> *mut u8 {
        (UART_BASE + off) as *mut u8
    }

    #[inline]
    /// Enable the "received data available" interrupt (IER bit 0), so an arriving byte
    /// raises UART0's PLIC source. The nucleus's stand-in for a real device interrupt:
    /// unlike the timer it fires only when something actually happened.
    ///
    /// # Safety
    /// Touches UART0's registers; call once during boot.
    pub unsafe fn enable_rx_interrupt() {
        core::ptr::write_volatile(Self::reg(IER), 0x01);
    }

    /// Read and discard every buffered byte. Not optional: the 16550 holds its receive
    /// interrupt asserted until the buffer is empty, so an unread byte re-raises the PLIC
    /// source forever. We only care that input ARRIVED, not what it was.
    ///
    /// # Safety
    /// Touches UART0's registers; call from the external-interrupt handler.
    pub unsafe fn drain_rx() {
        while core::ptr::read_volatile(Self::reg(LSR)) & LSR_DR != 0 {
            let _ = core::ptr::read_volatile(Self::reg(RBR));
        }
    }

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

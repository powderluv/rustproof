//! Minimal 16550 UART driver for COM1 (0x3F8) — the nucleus's only output device
//! for the M0 boot spike. Polled, no interrupts.
use crate::port::{inb, outb};
use core::fmt;

const COM1: u16 = 0x3F8;

/// Zero-sized handle to COM1. Stateless: the UART holds the state.
pub struct Serial;

impl Serial {
    /// Program COM1 to 38400 8N1, FIFO on, interrupts off.
    pub unsafe fn init() {
        outb(COM1 + 1, 0x00); // disable all UART interrupts
        outb(COM1 + 3, 0x80); // enable DLAB (set baud divisor)
        outb(COM1 + 0, 0x03); // divisor low  = 3 -> 38400 baud
        outb(COM1 + 1, 0x00); // divisor high = 0
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit; clears DLAB
        outb(COM1 + 2, 0xC7); // enable FIFO, clear, 14-byte threshold
        outb(COM1 + 4, 0x03); // RTS/DSR set
    }

    #[inline]
    fn transmit_empty() -> bool {
        unsafe { inb(COM1 + 5) & 0x20 != 0 }
    }

    /// Write one byte, spinning until the transmit holding register is free.
    pub fn write_byte(b: u8) {
        while !Self::transmit_empty() {}
        unsafe { outb(COM1, b) };
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                Serial::write_byte(b'\r');
            }
            Serial::write_byte(b);
        }
        Ok(())
    }
}

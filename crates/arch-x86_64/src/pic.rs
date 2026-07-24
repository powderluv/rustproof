//! Legacy 8259 PIC remap + 8254 PIT periodic timer (IRQ0) — the interrupt source that
//! drives preemptive scheduling. The CPU exception vectors occupy 0..31, so the PICs are
//! remapped to 0x20.. and IRQ0 (the timer) lands on vector 32.
use crate::port::outb;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;
const PIT_CH0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;

/// PIT input clock (~1.193182 MHz).
const PIT_HZ: u32 = 1_193_182;

/// The IDT vector IRQ0 (the timer) is remapped to.
pub const TIMER_VECTOR: usize = 0x20;

/// Remap the master/slave PICs to vectors 0x20..0x2F (clear of the CPU exception vectors
/// 0..31), then mask every IRQ except the timer (IRQ0 on the master).
pub unsafe fn remap_and_mask() {
    // ICW1: begin init (cascade mode, expect ICW4).
    outb(PIC1_CMD, 0x11);
    outb(PIC2_CMD, 0x11);
    // ICW2: vector offsets — master 0x20..0x27, slave 0x28..0x2F.
    outb(PIC1_DATA, 0x20);
    outb(PIC2_DATA, 0x28);
    // ICW3: cascade wiring — slave attached to master IRQ2.
    outb(PIC1_DATA, 0x04);
    outb(PIC2_DATA, 0x02);
    // ICW4: 8086/88 mode.
    outb(PIC1_DATA, 0x01);
    outb(PIC2_DATA, 0x01);
    // Masks: unmask only IRQ0 (timer) on the master; mask the entire slave.
    outb(PIC1_DATA, 0xFE);
    outb(PIC2_DATA, 0xFF);
}

/// Program PIT channel 0 for a periodic (~`hz`) interrupt on IRQ0 (mode 3, square wave).
pub unsafe fn init_pit(hz: u32) {
    let divisor = (PIT_HZ / hz) as u16;
    outb(PIT_CMD, 0x36); // channel 0, access lo+hi byte, mode 3, binary
    outb(PIT_CH0, divisor as u8);
    outb(PIT_CH0, (divisor >> 8) as u8);
}

/// Signal end-of-interrupt to the master PIC (for IRQ0..7).
pub unsafe fn eoi_master() {
    outb(PIC1_CMD, 0x20);
}

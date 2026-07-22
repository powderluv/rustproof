#![no_std]
#![no_main]
#![allow(unused)]

//! init -- untrusted root task: parse boot modules, spawn driver-host, grant the
//! GPU MMIO/DMA/IRQ caps, drive the M0 single-wave test. TODO(M0): see docs/milestone-M0.md.


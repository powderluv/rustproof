#![no_std]
#![no_main]
#![allow(unused)]

//! nucleus -- the bootable guest kernel image. Links the verified TCB libs +
//! arch-x86_64. Limine entry -> serial/GDT/IDT/paging -> nucleus-core -> hand off to init.
//! TODO(M0): implement boot. See docs/milestone-M0.md (T0.1-T0.4).


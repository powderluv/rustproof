#![no_std]
#![allow(unused)]

//! abi -- shared syscall numbers, IPC message layout, capability indices.
//! `#[repr(C)]` types used by BOTH the kernel and untrusted userland (incl. the C++
//! driver via a cbindgen-generated header). See docs/host-contract.md.


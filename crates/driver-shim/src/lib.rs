#![no_std]
#![allow(unused)]

//! driver-shim -- libc/POSIX SUBSET (mmap/open/ioctl/pthread/clock) re-pointed at
//! nucleus IPC, so the vendored C++ lite:: driver compiles unmodified. UNTRUSTED.
//! See docs/host-contract.md and docs/repo-structure.md sec 4.


#![no_std]
#![no_main]
#![allow(unused)]

//! **NOT IMPLEMENTED.** This crate is empty: no functions, no tests, nothing depends
//! on it. It reserves the name and records the intent, nothing more.
//! driver-host -- UNTRUSTED process that hosts the C++ lite:: driver (linked from
//! vendor/rocr-lite via CMake). Receives GPU caps; runs the dispatch-one-wave flow.
//! TODO(M0): add build.rs (cmake -> libamd_lite.a) + link. See docs/milestone-M0.md (T0.7-T0.11).

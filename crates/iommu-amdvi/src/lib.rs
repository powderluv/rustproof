#![no_std]
#![allow(unused)]

//! iommu-amdvi -- AMD-Vi Device Table + I/O page tables; the DMA-reach CRUX proof.
//!
//! VERIFIED TCB (+ Kani for register pokes). See docs/host-contract.md and docs/verification.md.
//! `dma_reach` proof is admitted + NON-load-bearing until M3/M4 (host IOMMU covers M0-M2).


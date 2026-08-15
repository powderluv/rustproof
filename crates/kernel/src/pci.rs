//! Just enough PCI configuration space to FIND THE IOMMU, and nothing else.
//!
//! # Why this is in the kernel, when a general config accessor is not
//!
//! A design pass on 2026-08-14 declined to add a config-space accessor, because a config-space
//! capability handed to a driver is authority over every device in the machine: config space is
//! a write-capable control plane, and on x86 the 0xCF8/0xCFC pair is GLOBAL, so one process's
//! access is every function's BARs. That ruling stands, and nothing here is reachable from user
//! mode.
//!
//! The IOMMU is the exception the design already carves out. docs/nucleus-design.md: the nucleus
//! grants the driver an `Mmio` capability for the GPU aperture but NEVER for the IOMMU aperture
//! — "the driver can therefore command arbitrary DMA; it cannot touch the tables that bound that
//! DMA." The nucleus must therefore locate the IOMMU itself, and no one else may. So this is
//! read-only, kernel-only, and looks for exactly one thing.
//!
//! # Scope
//!
//! Reads only. There is deliberately no config WRITE here: BAR sizing needs writes with the
//! decode bit cleared, which transiently unmaps a live device, and that was ruled a standing
//! no in the kernel rather than a deferral.

use arch_x86_64::port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// PCI base class 0x08 (system peripheral), subclass 0x06 (IOMMU).
const CLASS_IOMMU: u16 = 0x0806;

/// A located PCI function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Function {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
}

impl Function {
    /// The BDF as AMD-Vi's Device Table is indexed: bus:dev.func packed into 16 bits.
    pub fn bdf(&self) -> u16 {
        ((self.bus as u16) << 8) | ((self.dev as u16) << 3) | (self.func as u16)
    }
}

/// Read one 32-bit config register.
///
/// # Safety
/// Touches the global 0xCF8/0xCFC pair. Single-CPU, non-reentrant, kernel-only: no other agent
/// may be mid-config-cycle, which holds because nothing else in this nucleus performs one.
unsafe fn read32(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    let addr = 0x8000_0000
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((off as u32) & 0xFC);
    port::outl(CONFIG_ADDRESS, addr);
    port::inl(CONFIG_DATA)
}

/// Find the IOMMU, if this machine has one.
///
/// Scans bus 0 only. That is sufficient and not a simplification worth apologising for: an
/// AMD-Vi unit is a root-complex function by construction, and a nucleus that had to walk
/// bridges to find the thing that bounds DMA would already have lost.
///
/// # Safety
/// See [`read32`].
pub unsafe fn find_iommu() -> Option<Function> {
    for dev in 0..32u8 {
        for func in 0..8u8 {
            let id = read32(0, dev, func, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            // 0xFFFF is "no function here"; a missing function 0 means the whole device is
            // absent, so skip its remaining functions rather than probing seven ghosts.
            if vendor == 0xFFFF {
                if func == 0 {
                    break;
                }
                continue;
            }
            let class = (read32(0, dev, func, 0x08) >> 16) as u16;
            if class == CLASS_IOMMU {
                return Some(Function {
                    bus: 0,
                    dev,
                    func,
                    vendor,
                    device: (id >> 16) as u16,
                });
            }
        }
    }
    None
}

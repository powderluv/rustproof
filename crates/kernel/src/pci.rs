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

// The config-cycle half is x86-only; the pure decisions below compile everywhere so they can
// be host-tested on any development machine.
#[cfg(target_arch = "x86_64")]
use arch_x86_64::port;

#[cfg(target_arch = "x86_64")]
const CONFIG_ADDRESS: u16 = 0xCF8;
#[cfg(target_arch = "x86_64")]
const CONFIG_DATA: u16 = 0xCFC;

/// PCI base class 0x02 (network), subclass 0x00 (ethernet) — a DMA-capable function to
/// program a DTE for.
#[cfg(target_arch = "x86_64")]
const CLASS_ETHERNET: u16 = 0x0200;

/// PCI base class 0x08 (system peripheral), subclass 0x06 (IOMMU).
#[cfg(target_arch = "x86_64")]
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
#[cfg(target_arch = "x86_64")]

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
#[cfg(target_arch = "x86_64")]

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

/// What an AMD-Vi unit's PCI capability block says about its register aperture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AmdViCap {
    /// Physical base of the 512 KiB register aperture, or `None` if the capability's base
    /// register is UNPROGRAMMED.
    ///
    /// That is the normal state here, and it is worth being exact about: composing a base from
    /// an unassigned low half yields a plausible-looking address that is simply wrong.
    /// Measured on this rig — `lo=0x00000000, hi=0x0000fed8` produced `0xfed800000000` before
    /// the enable bit was consulted, which is not where anything lives. The base-low register
    /// is written by FIRMWARE; this nucleus boots `-kernel`/PVH with none, so nothing assigned
    /// it. The address must come from the ACPI IVRS table — reachable through the PVH
    /// `rsdp_paddr` the boot info already carries and nothing yet reads — or the nucleus must
    /// program the capability itself, which would be the first config WRITE in this tree.
    pub base: Option<u64>,
    /// Raw capability header, so a caller can decode fields this struct does not model.
    pub header: u32,
    /// The unit reports its register aperture as enabled (base-low bit 0).
    pub enabled: bool,
}

/// PCI capability ID for the AMD-Vi (Secure Device) block.
#[cfg(target_arch = "x86_64")]
const CAP_ID_SECURE_DEVICE: u8 = 0x0F;
/// Capability-type field value that says this Secure Device block is an IOMMU.
#[cfg(target_arch = "x86_64")]
const CAP_TYPE_IOMMU: u32 = 0x3;
#[cfg(target_arch = "x86_64")]

/// Walk `func`'s capability list and read its AMD-Vi register base.
///
/// This is the prerequisite for programming anything: the Device Table Base and Control
/// registers live in that aperture, and the aperture's address is only discoverable here.
/// Reading it is NOT programming it — nothing is written, by this function or any other.
///
/// Returns `None` if the function advertises no capability list, or has no Secure Device
/// capability, or has one that does not identify itself as an IOMMU. Each of those is a
/// different way of not being an AMD-Vi unit and none of them is an error.
///
/// # Safety
/// See [`read32`].
pub unsafe fn amd_vi_cap(func: &Function) -> Option<AmdViCap> {
    // Status register bit 4: "capability list present". Without it, offset 0x34 is not a
    // capability pointer and following it walks garbage.
    let status = (read32(func.bus, func.dev, func.func, 0x04) >> 16) as u16;
    if status & (1 << 4) == 0 {
        return None;
    }
    let mut off = (read32(func.bus, func.dev, func.func, 0x34) & 0xFC) as u8;
    // A malformed list can be circular. Config space holds at most 48 capabilities, so a walk
    // longer than that is a loop and must terminate rather than hang the boot.
    for _ in 0..48 {
        if off < 0x40 {
            return None;
        }
        let header = read32(func.bus, func.dev, func.func, off);
        if (header & 0xFF) as u8 == CAP_ID_SECURE_DEVICE {
            if (header >> 16) & 0x7 != CAP_TYPE_IOMMU {
                return None;
            }
            let lo = read32(func.bus, func.dev, func.func, off + 0x04);
            let hi = read32(func.bus, func.dev, func.func, off + 0x08);
            return Some(AmdViCap {
                base: compose_base(lo, hi),
                header,
                enabled: lo & 1 != 0,
            });
        }
        off = ((header >> 8) & 0xFC) as u8;
        if off == 0 {
            return None;
        }
    }
    None
}

/// Compose the register base from the capability's low/high halves, or `None` if unassigned.
///
/// Pure, so the rule can be host-tested; the config cycle that produces `lo`/`hi` cannot be.
/// The rule is "trust nothing unless the unit says the aperture is enabled AND the low half
/// actually carries base bits", and it exists because the obvious composition is wrong in the
/// case this rig actually presents.
pub fn compose_base(lo: u32, hi: u32) -> Option<u64> {
    if lo & 1 == 0 || lo & 0xFFFF_C000 == 0 {
        return None;
    }
    Some(((hi as u64) << 32) | ((lo as u64) & 0xFFFF_C000))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case this rig actually presents, and the one that produced a wrong answer.
    #[test]
    fn an_unprogrammed_base_is_not_composed_into_an_address() {
        // Measured on q35 + amd-iommu with no firmware: lo is zero, hi defaults to 0xfed8.
        // Composing them yields 0xfed800000000, which is a plausible-looking address and is
        // not where anything lives.
        assert_eq!(compose_base(0x0000_0000, 0x0000_fed8), None);
        // Enable bit set but no base bits: still nothing to point at.
        assert_eq!(compose_base(0x0000_0001, 0x0000_fed8), None);
        // Base bits present but the aperture is disabled: the unit is not claiming that
        // address yet, so neither do we.
        assert_eq!(compose_base(0xfed8_0000, 0x0000_0000), None);
    }

    #[test]
    fn a_programmed_base_is_composed_from_both_halves() {
        // Enabled, base bits present: the 32-bit case firmware normally leaves behind.
        assert_eq!(compose_base(0xfed8_0001, 0x0000_0000), Some(0xfed8_0000));
        // The low half's bits [13:0] are flags, not address, and must not leak in.
        assert_eq!(compose_base(0xfed8_3fff, 0x0000_0000), Some(0xfed8_0000));
        // A genuine 64-bit base uses the high half.
        assert_eq!(compose_base(0xfed8_0001, 0x0000_0007), Some(0x7_fed8_0000));
    }
}

/// Find the first function of `class` on bus 0.
///
/// Same walk as [`find_iommu`], parameterised. Kept separate rather than merged because the
/// IOMMU lookup is load-bearing for the isolation story and this one is a convenience for
/// choosing a device to bound; conflating them would invite "find any device" to grow rights
/// the IOMMU lookup must never have.
///
/// # Safety
/// See [`read32`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn find_class(class: u16) -> Option<Function> {
    for dev in 0..32u8 {
        for func in 0..8u8 {
            let id = read32(0, dev, func, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            if vendor == 0xFFFF {
                if func == 0 {
                    break;
                }
                continue;
            }
            if (read32(0, dev, func, 0x08) >> 16) as u16 == class {
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

/// The DMA-capable function this nucleus will bound, if the machine has one.
///
/// # Safety
/// See [`read32`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn find_dma_device() -> Option<Function> {
    find_class(CLASS_ETHERNET)
}

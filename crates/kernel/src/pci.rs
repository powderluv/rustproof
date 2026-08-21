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
    /// A slot no device has claimed. `0xFFFF` is the vendor id config space returns for an
    /// absent function, so this cannot be mistaken for a real one.
    pub const EMPTY: Function = Function {
        bus: 0,
        dev: 0,
        func: 0,
        vendor: 0xFFFF,
        device: 0xFFFF,
    };

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
unsafe fn read32(bus: u8, dev: u8, func: u8, off: u16) -> u32 {
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
    // u16, not u8. A capability may legally sit at 0xF8 or 0xFC, where `off + 0x08` wraps a
    // u8 — silently in release, since overflow-checks are off there, so the aperture base
    // would be composed from the Vendor/Device ID registers instead. The walk was already
    // hardened against a circular list and a below-header pointer; this was the same
    // outside-the-TCB input at the other end.
    let mut off = (read32(func.bus, func.dev, func.func, 0x34) & 0xFC) as u16;
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
            // Both registers must lie inside the 256-byte config header.
            if off + 0x08 > 0xFF {
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
        off = ((header >> 8) & 0xFC) as u16;
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

/// QEMU's `edu` test device: a register-driven DMA engine.
const EDU_VENDOR: u16 = 0x1234;
const EDU_DEVICE: u16 = 0x11e8;

/// The DMA-capable function this nucleus will bound.
///
/// Matched by VENDOR:DEVICE rather than by class, and the difference is not pedantry. Class
/// 0x0200 also matches the rig's e1000, and targeting that instead wrote DMA commands into a
/// NIC's registers — caught only because the caller checks an identification register before
/// trusting the mapping (`ident=0x00140241`, an e1000, not edu's `0x010000ed`). A device is
/// driven by its programming model, and only its exact identity implies that.
///
/// Falls back to any Ethernet function so a machine without `edu` still reports a bounded
/// device; it just cannot be driven into a transfer.
///
/// # Safety
/// See [`read32`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn find_dma_device() -> Option<Function> {
    for dev in 0..32u8 {
        for func in 0..8u8 {
            let id = read32(0, dev, func, 0x00);
            if (id & 0xFFFF) as u16 == EDU_VENDOR && (id >> 16) as u16 == EDU_DEVICE {
                return Some(Function {
                    bus: 0,
                    dev,
                    func,
                    vendor: EDU_VENDOR,
                    device: EDU_DEVICE,
                });
            }
        }
    }
    find_class(CLASS_ETHERNET)
}

/// Collect every DMA-capable function, `edu` first, into `out`. Returns how many were found.
///
/// `find_dma_device` returns only the first, which was enough while one domain existed. Per-device
/// containment needs them all: a domain bound to one BDF says nothing about what another device
/// can reach, and the second device is what makes "a capability for A's domain cannot grant reach
/// into B's" a claim with two sides rather than one.
#[cfg(target_arch = "x86_64")]
pub unsafe fn find_dma_devices(out: &mut [Function]) -> usize {
    let mut n = 0;
    // `edu` first, deliberately: it is the one this nucleus can drive into a transfer, so it is
    // the device every hardware oracle here is written against, and it should be domain 1
    // regardless of where it sits in the bus scan.
    if let Some(f) = find_dma_device() {
        if n < out.len() {
            out[n] = f;
            n += 1;
        }
    }
    for dev in 0..32u8 {
        for func in 0..8u8 {
            if n >= out.len() {
                return n;
            }
            let id = read32(0, dev, func, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            if vendor == 0xFFFF {
                continue;
            }
            let class = read32(0, dev, func, 0x08) >> 16;
            if class != CLASS_ETHERNET as u32 {
                continue;
            }
            let f = Function {
                bus: 0,
                dev,
                func,
                vendor,
                device: (id >> 16) as u16,
            };
            if out[..n].iter().any(|g| g.bdf() == f.bdf()) {
                continue;
            }
            out[n] = f;
            n += 1;
        }
    }
    n
}

/// How many DMA-capable functions exist, regardless of how many we have room to bound.
///
/// [`find_dma_devices`] stops at the caller's array, so it cannot answer this — and the
/// difference matters: a device-table entry with `V = 0` is PASSTHROUGH, not deny, so a
/// DMA-capable function the nucleus never enumerated has unrestricted access to memory while
/// the unit is enabled.
#[cfg(target_arch = "x86_64")]
pub unsafe fn count_dma_devices() -> usize {
    let mut n = 0;
    for dev in 0..32u8 {
        for func in 0..8u8 {
            let id = read32(0, dev, func, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            if vendor == 0xFFFF {
                continue;
            }
            let class = read32(0, dev, func, 0x08) >> 16;
            let is_edu = vendor == EDU_VENDOR && (id >> 16) as u16 == EDU_DEVICE;
            if is_edu || class == CLASS_ETHERNET as u32 {
                n += 1;
            }
        }
    }
    n
}

/// Every PCI function present on bus 0, up to `out.len()`. Returns how many were written.
///
/// Not just the DMA-capable ones: any function can be made a bus master by whoever holds its
/// registers, and an entry with `V = 0` is passthrough. What the unit needs is an entry for
/// everything, so the default is DENY.
#[cfg(target_arch = "x86_64")]
pub unsafe fn present_functions(out: &mut [Function]) -> usize {
    let mut n = 0;
    for dev in 0..32u8 {
        for func in 0..8u8 {
            if n >= out.len() {
                return n;
            }
            let id = read32(0, dev, func, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            if vendor == 0xFFFF {
                continue;
            }
            out[n] = Function {
                bus: 0,
                dev,
                func,
                vendor,
                device: (id >> 16) as u16,
            };
            n += 1;
        }
    }
    n
}

/// Is this function the one we know how to drive into a DMA?
#[cfg(target_arch = "x86_64")]
pub fn is_edu(f: &Function) -> bool {
    f.vendor == EDU_VENDOR && f.device == EDU_DEVICE
}

/// The physical base of `func`'s BAR0, or `None` if unassigned.
///
/// Only meaningful once FIRMWARE has run: a `-kernel`/PVH boot leaves every BAR at all-ones,
/// which is why the proof rig boots through SeaBIOS. Memory BARs carry flags in the low four
/// bits; masking them off is the difference between a base and a base plus its type bits.
///
/// # Safety
/// See [`read32`].
#[cfg(target_arch = "x86_64")]
pub unsafe fn bar0(func: &Function) -> Option<u64> {
    let raw = read32(func.bus, func.dev, func.func, 0x10);
    // All-ones means unassigned; bit 0 set means an I/O-space BAR, which is not a mapping.
    if raw == 0xFFFF_FFFF || raw == 0 || raw & 1 != 0 {
        return None;
    }
    Some((raw & 0xFFFF_FFF0) as u64)
}

/// Enable memory decoding and bus mastering for `func`.
///
/// A device cannot DMA with bus mastering off, and firmware leaves it off for anything it did
/// not itself drive. This is a config WRITE — the first in this tree — and it is deliberately
/// the narrowest one that exists: it sets two bits in the command register and touches no BAR,
/// so it cannot move a device's registers or resize a window. Assigning BARs remains firmware's
/// job, which is the whole reason the proof rig boots through SeaBIOS.
///
/// # Safety
/// See [`read32`]. Single-CPU, kernel-only.
#[cfg(target_arch = "x86_64")]
pub unsafe fn enable_bus_master(func: &Function) -> u32 {
    const MEMORY_SPACE: u32 = 1 << 1;
    const BUS_MASTER: u32 = 1 << 2;
    let cmd = read32(func.bus, func.dev, func.func, 0x04);
    let want = cmd | MEMORY_SPACE | BUS_MASTER;
    let addr = 0x8000_0000
        | ((func.bus as u32) << 16)
        | ((func.dev as u32) << 11)
        | ((func.func as u32) << 8)
        | 0x04;
    port::outl(CONFIG_ADDRESS, addr);
    port::outl(CONFIG_DATA, want);
    read32(func.bus, func.dev, func.func, 0x04)
}

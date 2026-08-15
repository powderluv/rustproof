//! Just enough ACPI to find the IVRS table, and nothing else.
//!
//! # Availability is host-dependent, which cost a wrong conclusion
//!
//! A `-kernel`/PVH boot has no firmware, so nobody fetches QEMU's ACPI tables out of fw_cfg and
//! places them. On QEMU 8.2.1 (homebrew, macOS) that is exactly what happens: `rsdp_paddr` is
//! `0x0` and there is no ACPI at all. On QEMU 8.2.2 (Ubuntu, shark-a) the SAME nucleus with the
//! SAME flags gets `rsdp_paddr = 0xf52c0`.
//!
//! That was recorded as "there is no ACPI on this boot path" on the strength of ONE host, and it
//! is wrong: it is a property of the QEMU build, not of PVH. The lesson is not about ACPI — a
//! negative result from a single environment is a claim about that environment, and this project
//! has two on purpose.
//!
//! Nothing here writes. Parsing is pure where it can be, so the validation rules are host-tested
//! rather than trusted.

/// A parsed Root System Description Pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rsdp {
    pub revision: u8,
    /// 32-bit RSDT address (ACPI 1.0). Zero when only an XSDT is provided.
    pub rsdt: u32,
    /// 64-bit XSDT address (ACPI 2.0+), `None` on a revision-0 table.
    pub xsdt: Option<u64>,
}

/// Why an RSDP was rejected. Named, because "not found" and "corrupt" are different problems.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RsdpErr {
    /// The 8-byte signature is not `RSD PTR `.
    BadSignature,
    /// The first 20 bytes do not sum to zero mod 256.
    BadChecksum,
    /// Revision >= 2 claims an extended part whose checksum does not sum to zero.
    BadExtendedChecksum,
    /// Revision >= 2 but the buffer is too short to hold the extended part.
    Truncated,
}

/// Validate and parse an RSDP from the 36 bytes at its physical address.
///
/// Pure: the caller does the (unsafe) read, this decides whether to believe it. The checksum is
/// the whole point — an RSDP is found by scanning or by a pointer someone else supplied, and a
/// structure that merely starts with the right eight bytes is not one.
pub fn parse_rsdp(buf: &[u8]) -> Result<Rsdp, RsdpErr> {
    if buf.len() < 20 {
        return Err(RsdpErr::Truncated);
    }
    if &buf[0..8] != b"RSD PTR " {
        return Err(RsdpErr::BadSignature);
    }
    if buf[0..20].iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
        return Err(RsdpErr::BadChecksum);
    }
    let revision = buf[15];
    let rsdt = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
    if revision < 2 {
        return Ok(Rsdp {
            revision,
            rsdt,
            xsdt: None,
        });
    }
    if buf.len() < 36 {
        return Err(RsdpErr::Truncated);
    }
    if buf[0..36].iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
        return Err(RsdpErr::BadExtendedChecksum);
    }
    let mut x = 0u64;
    for i in (24..32).rev() {
        x = (x << 8) | buf[i] as u64;
    }
    Ok(Rsdp {
        revision,
        rsdt,
        xsdt: if x == 0 { None } else { Some(x) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rsdp_v2(xsdt: u64) -> [u8; 36] {
        let mut b = [0u8; 36];
        b[0..8].copy_from_slice(b"RSD PTR ");
        b[9..15].copy_from_slice(b"BOCHS ");
        b[15] = 2;
        b[16..20].copy_from_slice(&0x7fff_0000u32.to_le_bytes());
        b[20..24].copy_from_slice(&36u32.to_le_bytes());
        b[24..32].copy_from_slice(&xsdt.to_le_bytes());
        // First checksum covers bytes 0..20, extended covers 0..36. Order matters: the first
        // must be fixed before the extended one is computed over a buffer containing it.
        let s: u8 = b[0..20].iter().fold(0u8, |a, x| a.wrapping_add(*x));
        b[8] = (!s).wrapping_add(1);
        let e: u8 = b[0..36].iter().fold(0u8, |a, x| a.wrapping_add(*x));
        b[32] = (!e).wrapping_add(1);
        b
    }

    #[test]
    fn a_well_formed_v2_rsdp_parses() {
        let b = rsdp_v2(0x7fff_1000);
        let r = parse_rsdp(&b).expect("should parse");
        assert_eq!(r.revision, 2);
        assert_eq!(r.xsdt, Some(0x7fff_1000));
        assert_eq!(r.rsdt, 0x7fff_0000);
    }

    /// The checksum is the whole reason to parse rather than cast. A structure that merely
    /// starts with the right eight bytes is not an RSDP, and a pointer handed to us by the
    /// hypervisor is exactly the kind of thing that might not be one.
    #[test]
    fn a_corrupt_rsdp_is_refused_rather_than_believed() {
        let mut b = rsdp_v2(0x7fff_1000);
        b[17] ^= 0xFF; // flip a byte the first checksum covers
        assert_eq!(parse_rsdp(&b), Err(RsdpErr::BadChecksum));

        let mut b = rsdp_v2(0x7fff_1000);
        b[30] ^= 0xFF; // inside the XSDT address: only the EXTENDED checksum covers this
        assert_eq!(parse_rsdp(&b), Err(RsdpErr::BadExtendedChecksum));

        let mut b = rsdp_v2(0x7fff_1000);
        b[0] = b'X';
        assert_eq!(parse_rsdp(&b), Err(RsdpErr::BadSignature));
    }

    #[test]
    fn a_revision_0_rsdp_has_no_xsdt_and_is_not_read_past_20_bytes() {
        let mut b = [0u8; 20];
        b[0..8].copy_from_slice(b"RSD PTR ");
        b[15] = 0;
        b[16..20].copy_from_slice(&0x000f_0000u32.to_le_bytes());
        let s: u8 = b[0..20].iter().fold(0u8, |a, x| a.wrapping_add(*x));
        b[8] = (!s).wrapping_add(1);
        let r = parse_rsdp(&b).expect("v0 must parse from 20 bytes");
        assert_eq!(r.revision, 0);
        assert_eq!(r.xsdt, None);
        assert_eq!(r.rsdt, 0x000f_0000);
    }

    #[test]
    fn a_truncated_buffer_is_refused() {
        assert_eq!(parse_rsdp(&[]), Err(RsdpErr::Truncated));
        let b = rsdp_v2(0x7fff_1000);
        // Claims revision 2 but only 20 bytes are available.
        assert_eq!(parse_rsdp(&b[0..20]), Err(RsdpErr::Truncated));
    }
}

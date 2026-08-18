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

/// The 36-byte header every ACPI description table starts with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableHeader {
    pub signature: [u8; 4],
    /// Total table length INCLUDING this header.
    pub length: u32,
    pub revision: u8,
}

/// Validate a table header and its whole-table checksum.
///
/// `buf` must be the entire table, because the checksum covers every byte of it: validating
/// only the header would accept a table whose body has been corrupted, which is the half that
/// actually gets parsed.
pub fn parse_table(buf: &[u8]) -> Result<TableHeader, RsdpErr> {
    if buf.len() < 36 {
        return Err(RsdpErr::Truncated);
    }
    let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if (length as usize) < 36 || buf.len() < length as usize {
        return Err(RsdpErr::Truncated);
    }
    if buf[..length as usize]
        .iter()
        .fold(0u8, |a, b| a.wrapping_add(*b))
        != 0
    {
        return Err(RsdpErr::BadChecksum);
    }
    Ok(TableHeader {
        signature: [buf[0], buf[1], buf[2], buf[3]],
        length,
        revision: buf[8],
    })
}

/// The 32-bit table pointers an RSDT carries after its header.
///
/// Returns an iterator rather than a collection because the count is `(length - 36) / 4` and
/// the caller has nowhere to put a heap allocation.
pub fn rsdt_entries(buf: &[u8]) -> impl Iterator<Item = u32> + '_ {
    let n = if buf.len() >= 36 {
        (buf.len() - 36) / 4
    } else {
        0
    };
    (0..n).map(move |i| {
        let o = 36 + i * 4;
        u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
    })
}

/// The AMD-Vi register base from the first IVHD block of an IVRS table.
///
/// IVRS layout: the 36-byte ACPI header, `IVinfo` (u32) and 8 reserved bytes, then a sequence
/// of IVHD blocks. Each block is `type(u8) flags(u8) length(u16) device_id(u16)
/// capability_offset(u16) iommu_base_address(u64) ...`, so the base sits at block offset 8.
///
/// Only the first block is read. A machine with several IOMMUs has several IVHDs, and this
/// nucleus has no story for more than one — taking the first and saying so is better than
/// silently ignoring the rest, which is why the caller is told how many were present.
pub fn ivrs_first_base(buf: &[u8]) -> Option<(u64, usize)> {
    if buf.len() < 48 || &buf[0..4] != b"IVRS" {
        return None;
    }
    let total = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if total > buf.len() {
        return None;
    }
    let mut off = 48;
    let mut first: Option<u64> = None;
    let mut count = 0usize;
    while off + 24 <= total {
        let len = u16::from_le_bytes([buf[off + 2], buf[off + 3]]) as usize;
        // A zero-length block would spin forever on a malformed table supplied from outside.
        if len < 24 || off + len > total {
            break;
        }
        count += 1;
        if first.is_none() {
            let mut b = 0u64;
            for i in (0..8).rev() {
                b = (b << 8) | buf[off + 8 + i] as u64;
            }
            first = Some(b);
        }
        off += len;
    }
    first.map(|b| (b, count))
}

/// Where the BIOS RSDP lives when firmware placed it: the last 128 KiB below 1 MiB.
pub const BIOS_SCAN_START: u64 = 0x000E_0000;
pub const BIOS_SCAN_END: u64 = 0x0010_0000;

/// Offset of the first validly-checksummed RSDP in `buf`, if any.
///
/// Needed by the FIRMWARE boot path: multiboot carries no `rsdp_paddr` the way PVH does, so
/// with firmware in play the pointer has to be found rather than handed over. ACPI puts it on
/// a 16-byte boundary in this window, and the checksum is what separates a real one from the
/// eight bytes `RSD PTR ` appearing by chance — which is exactly why this reuses
/// [`parse_rsdp`] rather than matching a signature.
pub fn scan_for_rsdp(buf: &[u8]) -> Option<usize> {
    let mut off = 0usize;
    while off + 20 <= buf.len() {
        if &buf[off..off + 8] == b"RSD PTR " && parse_rsdp(&buf[off..]).is_ok() {
            return Some(off);
        }
        off += 16;
    }
    None
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

    fn table(sig: &[u8; 4], body: &[u8]) -> std::vec::Vec<u8> {
        let mut t = std::vec::Vec::new();
        t.extend_from_slice(sig);
        let len = (36 + body.len()) as u32;
        t.extend_from_slice(&len.to_le_bytes());
        t.push(1); // revision
        t.push(0); // checksum, fixed below
        t.extend_from_slice(&[0u8; 26]); // oem fields
        t.extend_from_slice(body);
        let s: u8 = t.iter().fold(0u8, |a, x| a.wrapping_add(*x));
        t[9] = (!s).wrapping_add(1);
        t
    }

    #[test]
    fn a_tables_checksum_covers_its_body_not_just_the_header() {
        let mut t = table(b"IVRS", &[0u8; 32]);
        assert!(parse_table(&t).is_ok());
        // Corrupt a byte well past the 36-byte header. A header-only checksum would accept
        // this, and the body is the half that actually gets parsed.
        let n = t.len() - 1;
        t[n] ^= 0xFF;
        assert_eq!(parse_table(&t), Err(RsdpErr::BadChecksum));
    }

    #[test]
    fn a_table_shorter_than_it_claims_is_refused() {
        let t = table(b"IVRS", &[0u8; 32]);
        assert_eq!(parse_table(&t[..40]), Err(RsdpErr::Truncated));
        assert_eq!(parse_table(&[0u8; 10]), Err(RsdpErr::Truncated));
    }

    fn ivrs_with(bases: &[u64]) -> std::vec::Vec<u8> {
        let mut body = std::vec::Vec::new();
        body.extend_from_slice(&[0u8; 12]); // IVinfo + reserved
        for b in bases {
            let mut blk = std::vec::Vec::new();
            blk.push(0x10); // type
            blk.push(0); // flags
            blk.extend_from_slice(&24u16.to_le_bytes()); // length
            blk.extend_from_slice(&0u16.to_le_bytes()); // device id
            blk.extend_from_slice(&0x40u16.to_le_bytes()); // capability offset
            blk.extend_from_slice(&b.to_le_bytes()); // base
            blk.extend_from_slice(&[0u8; 8]); // segment/info/features
            body.extend_from_slice(&blk);
        }
        table(b"IVRS", &body)
    }

    #[test]
    fn the_amd_vi_base_comes_from_the_first_ivhd_block() {
        let t = ivrs_with(&[0xfed8_0000]);
        assert_eq!(ivrs_first_base(&t), Some((0xfed8_0000, 1)));
        // Several units: the first is taken and the COUNT is reported, so a caller cannot
        // mistake "one IOMMU" for "the only IOMMU".
        let t = ivrs_with(&[0xfed8_0000, 0xfed9_0000]);
        assert_eq!(ivrs_first_base(&t), Some((0xfed8_0000, 2)));
    }

    /// The table comes from outside the TCB, so a malformed one must terminate rather than
    /// spin. A zero-length IVHD block is the obvious way to make a naive walk hang forever.
    #[test]
    fn a_zero_length_ivhd_block_cannot_hang_the_walk() {
        let mut t = ivrs_with(&[0xfed8_0000, 0xfed9_0000]);
        // Blocks start at 48; zero the SECOND block's length field.
        t[48 + 24 + 2] = 0;
        t[48 + 24 + 3] = 0;
        assert_eq!(ivrs_first_base(&t), Some((0xfed8_0000, 1)));
    }

    #[test]
    fn a_non_ivrs_table_yields_no_base() {
        let t = table(b"APIC", &[0u8; 32]);
        assert_eq!(ivrs_first_base(&t), None);
    }

    #[test]
    fn rsdt_entries_are_read_from_after_the_header() {
        let mut body = std::vec::Vec::new();
        for p in [0x1000u32, 0x2000, 0x3000] {
            body.extend_from_slice(&p.to_le_bytes());
        }
        let t = table(b"RSDT", &body);
        let got: std::vec::Vec<u32> = rsdt_entries(&t).collect();
        assert_eq!(got, std::vec![0x1000, 0x2000, 0x3000]);
    }

    #[test]
    fn the_scan_finds_a_checksummed_rsdp_and_ignores_a_bare_signature() {
        let good = rsdp_v2(0x7fff_1000);
        let mut buf = std::vec![0u8; 512];
        // A DECOY first: the right eight bytes with a wrong checksum, on a 16-byte boundary.
        // A signature match alone would stop here and hand back a structure that is not an
        // RSDP, which is the whole reason the scan validates rather than matches.
        buf[16..16 + 8].copy_from_slice(b"RSD PTR ");
        buf[16 + 15] = 2;
        buf[128..128 + 36].copy_from_slice(&good);
        assert_eq!(scan_for_rsdp(&buf), Some(128));
        // Nothing valid anywhere.
        let empty = std::vec![0u8; 512];
        assert_eq!(scan_for_rsdp(&empty), None);
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

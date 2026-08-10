#![cfg_attr(not(test), no_std)]
//! hostcontract — the capability-gated host-contract syscall dispatcher.
//!
//! [`dispatch`] is a **pure function over the [`abi::HostEnv`] trait**: it reads no
//! global kernel state, only what the integrator exposes through `env`. That keeps the
//! whole decision surface host-unit-testable (a mock `HostEnv` in tests; real kernel
//! state at runtime) and makes it the natural Verus proof target — the load-bearing
//! guarantee, "no host-contract op grants authority without the required capability",
//! is expressed entirely in terms of `env.cap_lookup`.
//!
//! The syscall calling convention is fixed by [`abi::sysno`]: `rax` = number, args
//! `a0..a4` in `rdi, rsi, rdx, r10, r8`, result in `rax`. The integrator's trap handler
//! decodes those registers and calls [`dispatch`]; `EXIT` is handled there (it never
//! returns) so it only ever reaches us as an invalid selector.
//!
//! Maps to `docs/host-contract.md`: `GET_INFO` is VERIFIED, `MAP_BAR` is
//! VERIFIED* (the cap check is verified and gates a real mapping installed by the
//! integrator; only the BAR geometry — the window size — is still fixed rather than probed).

use abi::{syserr, sysno, CapId, CapRights, CapType, GpuInfo, HostEnv, MapBarResp, PAGE_SIZE};

/// Bytes copied out of user memory per `read_user_bytes` call in `DEBUG_WRITE`.
const DEBUG_CHUNK: usize = 256;
/// Upper bound on how many bytes a single `DEBUG_WRITE` will emit — a run-away or
/// hostile length is clamped here so the copy loop is always bounded.
const DEBUG_MAX_TOTAL: u64 = 64 * 1024;

/// Pages of a device window a single `MAP_BAR` installs. Fixed geometry for now (a real
/// backend reads the BAR size from PCI config space).
const STUB_BAR_PAGES: u64 = 1;

/// Reinterpret a `#[repr(C)]` plain-old-data value as its raw little-endian bytes.
///
/// TCB / SAFETY: `T` must be a `#[repr(C)]`, pointer-free, all-integer POD with no
/// padding — which holds for the three host-contract response structs used below
/// ([`GpuInfo`], [`MapBarResp`]). The returned slice borrows `val`, so it
/// cannot dangle, and we only ever *read* it (to copy into user memory), so no invalid
/// or uninitialized bit pattern is ever observed.
unsafe fn as_bytes<T>(val: &T) -> &[u8] {
    core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>())
}

/// Capability-gated host-contract syscall dispatch.
///
/// Pure over `env`; returns an [`abi::syserr`] code in every arm ([`syserr::OK`] on
/// success). `a3`/`a4` are reserved by the current contract.
///
// PROOF(later): no host-contract op succeeds (returns OK / grants authority) unless the
// required capability was present with the required type — i.e. every non-`OK` path is
// reachable only from a failing `env.cap_lookup`, a failing `env.map_device`, or a bad
// user pointer, and no `OK` path skips the capability check for `MAP_BAR`.
pub fn dispatch(
    env: &mut dyn HostEnv,
    num: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
) -> u64 {
    // a3/a4 are unused by every current op; bind them so the signature stays the fixed
    // 5-arg kernel dispatch shape without an unused-warning.
    let _ = (a3, a4);
    match num {
        sysno::DEBUG_WRITE => sys_debug_write(env, a0, a1),
        sysno::GET_INFO => sys_get_info(env, a0),
        sysno::MAP_BAR => sys_map_bar(env, a0, a2),
        _ => syserr::BAD_SYSCALL,
    }
}

/// `DEBUG_WRITE`: copy `len` bytes from user pointer `uptr` to the debug console, in
/// bounded `DEBUG_CHUNK`-sized chunks through a stack buffer. Clamps to `DEBUG_MAX_TOTAL`.
fn sys_debug_write(env: &mut dyn HostEnv, uptr: u64, len: u64) -> u64 {
    let total = len.min(DEBUG_MAX_TOTAL);
    let mut buf = [0u8; DEBUG_CHUNK];
    let mut off: u64 = 0;
    while off < total {
        let n = ((total - off) as usize).min(DEBUG_CHUNK);
        let chunk = &mut buf[..n];
        // `wrapping_add` keeps arithmetic total even at the top of the address space; a
        // wrapped-past-the-end pointer is simply rejected by `read_user_bytes`.
        if !env.read_user_bytes(uptr.wrapping_add(off), chunk) {
            return syserr::FAULT;
        }
        env.debug_write(chunk);
        off += n as u64;
    }
    syserr::OK
}

/// `GET_INFO`: write the device [`GpuInfo`] (raw `#[repr(C)]` bytes) to user pointer `uptr`.
fn sys_get_info(env: &mut dyn HostEnv, uptr: u64) -> u64 {
    let info: GpuInfo = env.gpu_info();
    // SAFETY: `info` is a live `#[repr(C)]` POD; see `as_bytes`.
    let bytes = unsafe { as_bytes(&info) };
    if env.write_user_bytes(uptr, bytes) {
        syserr::OK
    } else {
        syserr::FAULT
    }
}

/// `MAP_BAR`: require an [`CapType::Mmio`] capability at `cap_id`, then write a
/// [`MapBarResp`] to user pointer `resp_ptr`.
///
/// VERIFIED* per `docs/host-contract.md`: the capability CHECK below is the verified,
/// load-bearing part, and it now gates a REAL mapping — the integrator installs page-table
/// entries for the window the capability names, so a process that holds an `Mmio`
/// capability can actually reach the device, and one that does not cannot name it at all.
/// What remains stubbed is only the BAR *geometry*: the window size is fixed rather than
/// read from PCI config space.
fn sys_map_bar(env: &mut dyn HostEnv, cap_id: u64, resp_ptr: u64) -> u64 {
    let cap = CapId(cap_id as usize);
    let (base, writable) = match env.cap_lookup(cap) {
        // Type AND rights, per `docs/host-contract.md`: "rights ⊇ need" on every op.
        // Mapping a BAR at minimum exposes the device's registers, so it needs `READ` —
        // and the mapping is writable only if the capability also carries `WRITE`, or the
        // mapping would confer authority the capability does not.
        Some((CapType::Mmio, rights, base)) if rights.contains(CapRights::READ) => {
            (base, rights.contains(CapRights::WRITE))
        }
        // Missing cap, wrong object type, or insufficient rights: no authority.
        _ => return syserr::NO_CAP,
    };
    // Install the mapping. The capability's object is the physical base it names, so a
    // caller can only ever map a window some capability of its own already authorises.
    let user_va = match env.map_device(base, STUB_BAR_PAGES, writable) {
        Some(va) => va,
        None => return syserr::NO_MEM,
    };
    let resp = MapBarResp {
        user_va,
        size: STUB_BAR_PAGES * PAGE_SIZE,
    };
    // SAFETY: `resp` is a live `#[repr(C)]` POD; see `as_bytes`.
    let bytes = unsafe { as_bytes(&resp) };
    if env.write_user_bytes(resp_ptr, bytes) {
        syserr::OK
    } else {
        // The caller never learns the address, so leaving the window mapped would hand it
        // authority it cannot see and cannot drop. Undo, so MAP_BAR is all-or-nothing.
        env.unmap_device();
        syserr::FAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::CapRights;

    /// Base user VA the mock's flat "user memory" is mapped at; pointers below this or
    /// past the end of `mem` are rejected exactly like a real bad user pointer.
    const BASE_VA: u64 = 0x1000_0000;

    /// A synthetic [`HostEnv`] backing every host test: a flat user-memory buffer, a
    /// captured debug stream, a tiny cap table, and a bounded DMA-frame counter.
    struct MockEnv {
        mem: Vec<u8>,
        debug_out: Vec<u8>,
        caps: Vec<(CapId, (CapType, CapRights, u64))>,
        next_frame: u64,
        frames_left: usize,
        info: GpuInfo,
        /// Physical addresses currently handed out and not yet freed (ownership tracking).
        held: Vec<u64>,
        /// Device windows this env was asked to map, as `(phys, pages, writable)`.
        mapped: Vec<(u64, u64, bool)>,
    }

    impl MockEnv {
        fn new(mem_len: usize) -> Self {
            MockEnv {
                mem: vec![0u8; mem_len],
                debug_out: Vec::new(),
                caps: Vec::new(),
                next_frame: 0x8000_0000,
                frames_left: 0,
                info: GpuInfo::default(),
                held: Vec::new(),
                mapped: Vec::new(),
            }
        }
        /// User VA of offset `off` into the flat user-memory buffer.
        fn va(&self, off: usize) -> u64 {
            BASE_VA + off as u64
        }
        /// Read `mem[off..off+len]` back out for assertions.
        fn peek(&self, off: usize, len: usize) -> &[u8] {
            &self.mem[off..off + len]
        }
    }

    impl HostEnv for MockEnv {
        fn debug_write(&mut self, bytes: &[u8]) {
            self.debug_out.extend_from_slice(bytes);
        }
        fn gpu_info(&self) -> GpuInfo {
            self.info
        }
        fn cap_lookup(&self, cap: CapId) -> Option<(CapType, CapRights, u64)> {
            self.caps.iter().find(|(c, _)| *c == cap).map(|(_, v)| *v)
        }
        fn map_device(&mut self, phys: u64, pages: u64, writable: bool) -> Option<u64> {
            // Mock: hand back a deterministic VA derived from the request, and record what
            // was asked for — including the permission, which the tests assert on.
            self.mapped.push((phys, pages, writable));
            Some(BASE_VA + 0x10_0000 + phys)
        }
        fn unmap_device(&mut self) {
            self.mapped.clear();
        }
        fn write_user_bytes(&mut self, uptr: u64, bytes: &[u8]) -> bool {
            if uptr < BASE_VA {
                return false;
            }
            let off = (uptr - BASE_VA) as usize;
            let end = match off.checked_add(bytes.len()) {
                Some(e) => e,
                None => return false,
            };
            if end > self.mem.len() {
                return false;
            }
            self.mem[off..end].copy_from_slice(bytes);
            true
        }
        fn read_user_bytes(&self, uptr: u64, out: &mut [u8]) -> bool {
            if uptr < BASE_VA {
                return false;
            }
            let off = (uptr - BASE_VA) as usize;
            let end = match off.checked_add(out.len()) {
                Some(e) => e,
                None => return false,
            };
            if end > self.mem.len() {
                return false;
            }
            out.copy_from_slice(&self.mem[off..end]);
            true
        }
    }

    /// Decode a `#[repr(C)]` POD back out of a byte slice for round-trip assertions.
    fn decode<T: Copy>(bytes: &[u8]) -> T {
        assert!(bytes.len() >= core::mem::size_of::<T>());
        // SAFETY: test-only; `bytes` was produced by `as_bytes` over the same POD type.
        unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const T) }
    }

    #[test]
    fn debug_write_copies_exact_bytes() {
        let mut env = MockEnv::new(4096);
        let payload = b"hello, host contract";
        let off = 64;
        env.mem[off..off + payload.len()].copy_from_slice(payload);

        let ptr = env.va(off);
        let r = dispatch(
            &mut env,
            sysno::DEBUG_WRITE,
            ptr,
            payload.len() as u64,
            0,
            0,
            0,
        );
        assert_eq!(r, syserr::OK);
        assert_eq!(env.debug_out.as_slice(), payload);
    }

    #[test]
    fn debug_write_chunks_across_buffer() {
        // Longer than DEBUG_CHUNK so the copy loop runs multiple iterations.
        let len = DEBUG_CHUNK * 3 + 7;
        let mut env = MockEnv::new(len + 16);
        for (i, b) in env.mem.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let expected: Vec<u8> = env.mem[..len].to_vec();

        let ptr = env.va(0);
        let r = dispatch(&mut env, sysno::DEBUG_WRITE, ptr, len as u64, 0, 0, 0);
        assert_eq!(r, syserr::OK);
        assert_eq!(env.debug_out, expected);
    }

    #[test]
    fn debug_write_clamps_to_max_total() {
        let mut env = MockEnv::new(DEBUG_MAX_TOTAL as usize + 4096);
        // Ask for far more than the clamp; only DEBUG_MAX_TOTAL bytes are emitted.
        let ptr = env.va(0);
        let r = dispatch(
            &mut env,
            sysno::DEBUG_WRITE,
            ptr,
            DEBUG_MAX_TOTAL + 4096,
            0,
            0,
            0,
        );
        assert_eq!(r, syserr::OK);
        assert_eq!(env.debug_out.len() as u64, DEBUG_MAX_TOTAL);
    }

    #[test]
    fn debug_write_bad_pointer_faults() {
        let mut env = MockEnv::new(256);
        // Pointer well past the end of user memory.
        let ptr = env.va(0x10_0000);
        let r = dispatch(&mut env, sysno::DEBUG_WRITE, ptr, 8, 0, 0, 0);
        assert_eq!(r, syserr::FAULT);
        assert!(env.debug_out.is_empty());
    }

    #[test]
    fn get_info_round_trips() {
        let mut env = MockEnv::new(4096);
        env.info = GpuInfo {
            pci_vendor: 0x1002,
            pci_device: 0x7551,
            gfx_version: 1201,
            vram_bytes: 32u64 * 1024 * 1024 * 1024,
        };
        let want = env.info;
        let off = 128;

        let ptr = env.va(off);
        let r = dispatch(&mut env, sysno::GET_INFO, ptr, 0, 0, 0, 0);
        assert_eq!(r, syserr::OK);
        let got: GpuInfo = decode(env.peek(off, core::mem::size_of::<GpuInfo>()));
        assert_eq!(got, want);
    }

    #[test]
    fn get_info_bad_pointer_faults() {
        let mut env = MockEnv::new(8); // too small to hold GpuInfo
        let ptr = env.va(0);
        let r = dispatch(&mut env, sysno::GET_INFO, ptr, 0, 0, 0, 0);
        assert_eq!(r, syserr::FAULT);
    }

    #[test]
    fn map_bar_with_mmio_cap_ok() {
        let mut env = MockEnv::new(4096);
        let cap = CapId(7);
        let base = 0xE000_0000u64;
        env.caps.push((cap, (CapType::Mmio, CapRights::ALL, base)));
        let off = 256;

        let ptr = env.va(off);
        let r = dispatch(
            &mut env,
            sysno::MAP_BAR,
            cap.0 as u64,
            0,   // BAR index (stub ignores)
            ptr, // resp ptr
            0,
            0,
        );
        assert_eq!(r, syserr::OK);
        let resp: MapBarResp = decode(env.peek(off, core::mem::size_of::<MapBarResp>()));
        assert_eq!(resp.size, STUB_BAR_PAGES * PAGE_SIZE);
        // The VA is whatever the integrator installed the mapping at — the contract is that
        // it asked for exactly the window the capability names, which the mock records.
        assert_eq!(env.mapped, vec![(base, STUB_BAR_PAGES, true)]);
        assert_eq!(resp.user_va, BASE_VA + 0x10_0000 + base);
    }

    #[test]
    fn map_bar_read_only_cap_maps_read_only() {
        // The mapping must carry exactly the capability's authority: READ without WRITE
        // has to produce a non-writable window, or attenuating a capability would not
        // attenuate the access it grants.
        let mut env = MockEnv::new(4096);
        let cap = CapId(7);
        let base = 0xE000_0000u64;
        env.caps.push((cap, (CapType::Mmio, CapRights::READ, base)));
        let ptr = env.va(0);
        assert_eq!(
            dispatch(&mut env, sysno::MAP_BAR, cap.0 as u64, 0, ptr, 0, 0),
            syserr::OK
        );
        assert_eq!(env.mapped, vec![(base, STUB_BAR_PAGES, false)]);
    }

    #[test]
    fn map_bar_undoes_the_mapping_when_the_response_faults() {
        // All-or-nothing: a caller that never learns the address must not be left holding
        // a window it cannot see or drop.
        let mut env = MockEnv::new(4096);
        let cap = CapId(7);
        env.caps
            .push((cap, (CapType::Mmio, CapRights::ALL, 0xE000_0000)));
        // A resp pointer outside the mock's user window faults on write-back.
        assert_eq!(
            dispatch(&mut env, sysno::MAP_BAR, cap.0 as u64, 0, 1, 0, 0),
            syserr::FAULT
        );
        assert!(env.mapped.is_empty());
    }

    #[test]
    fn map_bar_maps_nothing_without_the_capability() {
        // The mapping must be gated by the cap, not merely the response: a refused caller
        // must not have had a window installed behind the error.
        let mut env = MockEnv::new(4096);
        let ptr = env.va(0);
        assert_eq!(
            dispatch(&mut env, sysno::MAP_BAR, 42, 0, ptr, 0, 0),
            syserr::NO_CAP
        );
        assert!(env.mapped.is_empty());
    }

    #[test]
    fn map_bar_wrong_type_is_no_cap() {
        let mut env = MockEnv::new(4096);
        let cap = CapId(3);
        // An Untyped cap must NOT authorize MAP_BAR.
        env.caps.push((cap, (CapType::Untyped, CapRights::ALL, 0)));
        let ptr = env.va(0);
        let r = dispatch(&mut env, sysno::MAP_BAR, cap.0 as u64, 0, ptr, 0, 0);
        assert_eq!(r, syserr::NO_CAP);
    }

    #[test]
    fn map_bar_missing_cap_is_no_cap() {
        let mut env = MockEnv::new(4096);
        let ptr = env.va(0);
        let r = dispatch(&mut env, sysno::MAP_BAR, 99, 0, ptr, 0, 0);
        assert_eq!(r, syserr::NO_CAP);
    }

    #[test]
    fn map_bar_bad_resp_pointer_faults() {
        let mut env = MockEnv::new(4096);
        let cap = CapId(1);
        env.caps
            .push((cap, (CapType::Mmio, CapRights::READ, 0xABCD)));
        // Valid cap, but the response pointer is out of range.
        let ptr = env.va(0x10_0000);
        let r = dispatch(&mut env, sysno::MAP_BAR, cap.0 as u64, 0, ptr, 0, 0);
        assert_eq!(r, syserr::FAULT);
    }

    #[test]
    fn map_bar_without_read_right_is_no_cap() {
        // Right object type, insufficient rights: mapping a BAR exposes device registers.
        let mut env = MockEnv::new(4096);
        let cap = CapId(1);
        env.caps
            .push((cap, (CapType::Mmio, CapRights::WRITE, 0xE000_0000)));
        let ptr = env.va(0);
        let r = dispatch(&mut env, sysno::MAP_BAR, cap.0 as u64, 0, ptr, 0, 0);
        assert_eq!(r, syserr::NO_CAP);
    }

    #[test]
    fn exit_and_unknown_are_bad_syscall() {
        let mut env = MockEnv::new(16);
        assert_eq!(
            dispatch(&mut env, sysno::EXIT, 0, 0, 0, 0, 0),
            syserr::BAD_SYSCALL
        );
        assert_eq!(
            dispatch(&mut env, 0xDEAD_BEEF, 0, 0, 0, 0, 0),
            syserr::BAD_SYSCALL
        );
    }
}

# Rustproof Host Contract — Nucleus ⇄ untrusted `lite::` driver IPC

> **Status:** spec surface (2026-07-21). Companion to
> [`implementation-plan.md`](implementation-plan.md) and [`research-brief.md`](research-brief.md).
> Re-expresses the internal `amdgpu_lite` ioctl surface (the `lite::` driver's kernel-shim
> ioctls, §1L.1/§1L.7 of the internal port notes) as a
> capability-gated IPC contract between the **verified Rust+Verus nucleus** and the
> **untrusted `lite::` gfx1201 driver process**.
>
> This document is the **frozen M0 spec surface** and the **proof target for M1–M3**: every
> `requires`/`ensures` sketched in §7 is what the Verus harness must discharge. It is a *contract*,
> not an implementation. Where it shows Rust, the types are normative (they live in the shared
> `rustproof-abi` crate); where it shows Verus, the snippets are illustrative sketches of the
> obligation, not claimed to compile against a pinned toolchain.

---

## 0. How to read this

Three tags classify every operation, matching the decision doc's V/A/U table (§6 there):

| Tag | Meaning | What the proof owes |
|---|---|---|
| **VERIFIED** | The nucleus *decision* (which frames/VAs/rights the op grants) is guarded by the AS/capability invariant (M2) or the DMA-reach invariant (M3). A Verus `ensures` clause pins the post-state. | The handler is a proven-total transition preserving the invariant. |
| **TRUSTED-STUB** | A small `#[verifier::external_body]` `unsafe` primitive: a raw MMIO store, a doorbell poke, an MSI-X ack. Its *inputs* are proven in-bounds by the VERIFIED caller; the *store itself* is hand-audited + Kani-checked, not proven. | Nothing (Verus); a Kani harness + an assumed-contract comment. |
| **UNTRUSTED** | The driver programs the GPU's own MMU (**GPUVM**). The nucleus neither validates nor trusts the result. Safety comes entirely from the AMD-Vi tables the nucleus owns downstream (see §6). | Nothing — contained, not trusted. Correctness is out of scope by design. |

A single op can split across tags: e.g. `MAP_BAR`'s *choice* of which physical frame to map is
VERIFIED; the PTE store that installs it is a TRUSTED-STUB.

The load-bearing timeline (decision doc §3): under plain VFIO (M0–M2) the **host Linux** programs
the physical IOMMU, so the DMA-reach parts of this contract are enforced by the host, not the
nucleus proof. The nucleus IOMMU proof only becomes load-bearing at **M3** (emulated AMD-Vi) and
hardware-enforced at **M4** (bare-metal AMD-Vi). The *contract shape does not change* across those
milestones — only which agent enforces it. That is deliberate: freeze it once, at M0.

---

## 1. Contract at a glance

```
   UNTRUSTED lite:: driver process (assumed hostile)
   ├─ builds MQD / PM4 / GPUVM page tables in frames it owns
   ├─ holds: Cap<Device>, Cap<Mmio>(GC regs), Cap<DmaMem>*, Cap<Irq>, Cap<Endpoint>
   └─ every privileged effect crosses ONE synchronous IPC boundary ↓
┌──────────────────────────────────────────────────────────────────────────┐
│  nucleus host-contract endpoint  (seL4-style `call`, per-thread IPC buf)   │
├──────────────────────────────────────────────────────────────────────────┤
│                        VERIFIED NUCLEUS (Rust + Verus)                      │
│  op            tag             invariant guarding it                        │
│  GET_INFO      VERIFIED        cap-validity (read-only, no authority)       │
│  MAP_BAR       VERIFIED*       AS/cap  (M2)     *store = TRUSTED-STUB        │
│  ALLOC_VRAM    VERIFIED        AS/cap + DMA-grant (M2→M3)                    │
│  ALLOC_GTT     VERIFIED        AS/cap + DMA-reach (M3, load-bearing)         │
│  FREE          VERIFIED        AS/cap + reclaim-safety (M4)                  │
│  MAP_GPU       UNTRUSTED       — driver programs GPUVM; contained by V3/V4   │
│  UNMAP_GPU     UNTRUSTED       —                                            │
│  SETUP_IRQ     TRUSTED-STUB    routing VERIFIED; MSI-X ack = stub (A3)       │
│  RING_DOORBELL TRUSTED-STUB    offset-bounds VERIFIED; store = stub          │
├──────────────────────────────────────────────────────────────────────────┤
│  audited unsafe stub: ctx-switch, MMIO r/w, TLB/IOTLB invalidate, MSI ack   │
├──────────────────────────────────────────────────────────────────────────┤
│  TRUSTED HW: CPU MMU · AMD-Vi silicon+µcode · gfx1201 + firmware            │
└──────────────────────────────────────────────────────────────────────────┘
```

gfx1201 concrete facts this contract is written against (from the tri-OS bring-up, ``):
PCI `1002:7551` (RX 9070 XT / AI PRO R9700, RDNA4) at BDF e.g. `0000:03:00.0` + HDMI-audio `.1`;
three BARs — an **MMIO register BAR** (GC/GMC/NBIO registers), a **doorbell aperture BAR** (middle
BAR by size, uncached), and a **VRAM aperture BAR** (resizable; 256 MiB with ReBAR off, full size
with ReBAR on); the **doorbell is dead under passthrough**, so submission is an MMIO poke of
`CP_HQD_PQ_WPTR` on the GC block; GPUVM is a 4-level GFXHUB page table (PDB2→PDB1→PDB0→PTB) with
0-based VRAM-offset entries, enabled via `GCVM_CONTEXT0`.

---

## 2. The `rustproof-abi` crate

A single `#![no_std]` crate shared verbatim by three consumers: the verified nucleus (compiled
under Verus), the untrusted `lite::` client shim (compiled by stable rustc), and the M0 C++ bridge
(via `cbindgen`-generated headers). It is **derive-light and pointer-free** so Verus can parse it.

```
rustproof-abi/
├── Cargo.toml            # no_std; deps: bitflags (no_std). NO zerocopy/serde in the verified build.
├── src/
│   ├── lib.rs            #![no_std]  pub use ...
│   ├── ids.rs            # CapId, DmaAddr, GpuVa, PhysLen, BarIndex — repr(transparent) newtypes
│   ├── rights.rs         # Rights bitflags
│   ├── ops.rs            # OpCode enum + per-op Request/Response structs (#[repr(C)])
│   ├── msg.rs            # MessageInfo, CapSlot, IpcBuffer layout constants
│   └── error.rs          # Error enum (repr i32) — the only non-Ok wire result
└── ...
```

Design rules that keep it verifiable and ABI-stable:

- **All wire types are `#[repr(C)]` plain-old-data.** No pointers, no `&T`, no slices, no enums with
  payloads on the wire. Every "reference to memory" is a `(CapId, offset: u64, len: u64)` triple the
  nucleus bounds-checks against the capability's extent — never a raw address it dereferences.
- **No `zerocopy`/`serde` derives in the verified build.** Those macros emit code Verus's frontend
  chokes on. Parsing is a hand-written `from_words(&[u64]) -> Result<Self, Error>` per struct (small,
  total, itself in the M1 proof scope). A `host` cargo feature can turn on `zerocopy` for the
  untrusted client only.
- **Fixed-width fields, explicit padding, `const` size asserts.** `const _: () = assert!(size_of::<MapBarReq>() == 40);`
  so the layout is frozen the same on both sides of the boundary.
- **Versioned.** `pub const ABI_VERSION: u32` is the first word of every session handshake; a
  mismatch is a hard `Error::AbiMismatch`. M0 freezes v1.

```rust
// ids.rs
#[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq)] pub struct CapId(pub u32);
#[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq)] pub struct DmaAddr(pub u64); // IOVA (M3+) / guest-phys (M0–M2)
#[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq)] pub struct GpuVa(pub u64);   // GPU virtual address (GPUVM) — opaque to nucleus
#[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq)] pub struct PhysLen(pub u64);
#[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq)] pub struct BarIndex(pub u8);
pub const CAP_NULL: CapId = CapId(0);
```

The client side wraps `CapId` in compile-time-typed phantoms (`Cap<Device>`, `Cap<DmaMem>`, …) for
ergonomics; on the wire it is always a bare `CapId` plus the `OpCode`, and the nucleus re-checks the
*runtime* type tag (§3.1). Client-side types are a convenience, never the source of safety.

---

## 3. Capability model

Authority is **capabilities only** — there is no ambient authority, no global namespace, no "current
device". If a `CapId` is not in the caller's capability space with the right kind and rights, the op
faults. This is the object the M2 inter-AS-isolation proof reasons over.

### 3.1 Typed handles

A kernel capability is an entry in a per-address-space **capability space** (a `CSpace`, seL4-style
CNode table). The nucleus stores, per AS:

```rust
// nucleus-internal (NOT in abi crate) — shown for the proof model
pub enum CapObject {
    Device   { pci: PciId, bars: [BarExtent; 6], mmio_rights: Rights },
    Mmio     { phys_base: u64, len: u64, mapped_va: u64, rights: Rights },   // a BAR window mapped into this AS
    DmaMem   { kind: MemKind /*Vram|Gtt*/, frames: FrameSet, iova: Option<DmaAddr>, rights: Rights },
    Irq      { source: u16, notify: CapId },                                  // an MSI-X vector bound to a Notification
    Notification { queue: NotifWord },                                        // async signal object (replaces eventfd)
    Endpoint { badge: u64 },                                                  // synchronous IPC endpoint (the host-contract svc)
    GpuvmCtx { pt_root_frames: FrameSet, mmio: CapId },                       // driver-owned GPUVM page-table backing (UNTRUSTED contents)
}

pub struct Cap { obj: CapObject, rights: Rights, derived_from: Option<CapId> }
```

Two invariants make `CapId` sound to pass across the untrusted boundary:

1. **`CapId` is a local index, not a pointer.** It indexes *the caller's own* `CSpace`. A driver
   cannot forge authority by naming a number: the number only resolves inside its own space, and only
   to caps the nucleus put there. (This is the property the M2 proof needs — a process's reachable
   frames are exactly the frames named by caps *in its CSpace*.)
2. **The nucleus re-checks the runtime kind + rights on every op.** `resolve(cspace, id, KIND, need)`
   returns `Err(Error::BadCap)` unless the entry exists, has kind `KIND`, and `rights ⊇ need`.

### 3.2 Rights

```rust
// rights.rs
bitflags::bitflags! {
    #[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct Rights: u32 {
        const READ    = 1 << 0; // read the object's info / CPU-read mapped memory
        const WRITE   = 1 << 1; // CPU-write mapped memory / write MMIO
        const MAP     = 1 << 2; // map into an address space (BAR / DMA frames)
        const DMAGRANT= 1 << 3; // enter into an IOMMU domain (make GPU-reachable)
        const IRQBIND = 1 << 4; // bind an MSI-X source to a Notification
        const DOORBELL= 1 << 5; // ring a doorbell index in the granted sub-aperture
        const GRANT   = 1 << 6; // transfer this cap over IPC (NOT YET CHECKED by the
                                //   nucleus; the "derive children" half has no operation —
                                //   capability spaces are flat, see crates/capabilities)
        const REVOKE  = 1 << 7; // revoke children (usually held only by the manager)
    }
}
```

Rights only ever **monotonically shrink** on derivation (§3.3). No op re-widens rights — that is a
Verus `ensures` on every derivation path (the "no-authority-amplification" property, V6/M5).

### 3.3 Derivation

All driver capabilities descend from **one root the nucleus mints at process spawn**: a
`Cap<Device>` for the gfx1201 BDF, carrying the physical BAR extents and the ceiling `mmio_rights`.
Everything else is derived, sub-setting range and rights:

```
Device(gfx1201, bars, mmio_rights=RW)                       ← minted by nucleus at spawn
├─ MAP_BAR   → Mmio(GC regs,   sub-range of BAR, rights ⊆ RW|MAP)     [driver programs GPUVM/HQD here]
├─ MAP_BAR   → Mmio(doorbell,  usually NOT derived — mediated, see RING_DOORBELL)
├─ ALLOC_VRAM→ DmaMem{Vram}(frames, rights=RW|MAP)                    ← from device VRAM pool
├─ ALLOC_GTT → DmaMem{Gtt}(frames, iova=Some(_), rights=RW|MAP|DMAGRANT) ← host RAM, entered in AMD-Vi domain
├─ (from a DmaMem) GpuvmCtx(pt_root ⊆ that DmaMem)                    ← page-table backing, UNTRUSTED contents
├─ SETUP_IRQ → Irq(vector) bound to a caller-provided Notification
└─ Endpoint(host-contract)                                            ← minted at spawn; the call target
```

Derivation rules the proof enforces (`derive_ok`, §7):

- child range ⊆ parent range; child `rights ⊆` parent `rights`;
- a `DmaMem` cap can only be minted from a pool cap the nucleus owns (frames are never conjured);
- a `GpuvmCtx` cap's page-table root frames must be a subset of an existing `DmaMem` the driver holds
  — i.e. the driver's GPUVM page tables live in frames *it already owns*, so writing them can corrupt
  nothing outside its grant (this is what makes UNTRUSTED `MAP_GPU` memory-safe; see §6).

### 3.4 Revocation & reclaim safety

The research brief (§6 design-choices) recommends *prohibiting fine-grained revocation* to cut proof
cost — but the contract must support `FREE` (the old `FREE_VRAM` ioctl). We reconcile by making
revocation **coarse and protocol-bound**, never mid-dispatch:

- Revocation is a nucleus-mediated *drain → unmap-everywhere → flush → return-to-pool* sequence, not a
  cap-table poke. `FREE(mem)` (a) removes the AMD-Vi domain entry for `mem.iova`, (b) unmaps `mem`'s
  frames from the driver AS if CPU-mapped, (c) **IOTLB/TLB-flushes before the frame returns to the
  pool** and (d) drops the cap and all its children.
- **No frame is ever revoked out from under a live GPU dispatch.** The driver is responsible for
  quiescing (fence retired) before `FREE`; the nucleus does not attempt to interrupt an in-flight
  wave. This preserves "memory can't be revoked from under verified/executing code" while still
  allowing explicit free.
- Step (c) is the **reclaim-safety** property **V5/M4**: a freed frame cannot be reached through a
  stale IOTLB entry before it is re-granted (closes the unmap/flush TOCTOU). Until M4 this is
  host-enforced; the *proof* obligation is written now so the flush call sits on the verified path
  from day one.

Whole-process teardown revokes the entire CSpace atomically (the Atmosphere-style
resources-return-on-container-termination path), which is the cheap, always-available reclaim.

---

## 4. Message-passing mechanism

### 4.1 Endpoints and Notifications

Two IPC objects, both seL4-shaped:

- **Endpoint** — synchronous rendezvous. The driver `call()`s the host-contract endpoint: it sends a
  request and blocks until the nucleus replies. One round-trip = one privileged operation. The
  nucleus handler runs to completion (big-lock, Atmosphere-style — §5 concurrency below) and replies.
- **Notification** — asynchronous single-word signal, the eventfd replacement for IRQs. `SETUP_IRQ`
  binds an MSI-X vector to a Notification the driver waits on (`wait()`/`poll()`); the nucleus ISR
  signals it. No data, just a wakeup + a bitmask of which sources fired.

Big-lock concurrency: the nucleus takes one lock across a host-contract op, so every handler is a
single atomic transition on the shared state (cap spaces, page tables, IOMMU domain). This is the
concurrency model the M1–M3 proofs assume (documented as an acknowledged timing channel, decision
doc §5/M5). It makes each `ensures` a straight-line pre/post pair with no interleaving to reason about.

### 4.2 IPC buffer layout — register + shared-buffer transfer

Each thread has a small fixed **IPC buffer** (one page, mapped RW into the thread and readable by the
nucleus). A `call` transfers:

1. **Message registers (MRs)** — the request struct, marshalled into a fixed array of `u64` words. Fast
   path is *actual CPU registers* on entry (like seL4's first few MRs); the rest spill into the IPC
   buffer. Request/response structs are sized to fit (`≤ MSG_MAX_WORDS`, e.g. 16 words / 128 bytes).
2. **Capability slots** — a small fixed array (`CAP_SLOTS`, e.g. 4) for capabilities *passed in* or
   *returned* (e.g. `MAP_BAR` returns a fresh `Cap<Mmio>` in slot 0). Caps move by the nucleus
   inserting into / reading from the caller's CSpace — the `CapId` on the wire is validated, never
   trusted as a pointer.

```rust
// msg.rs
pub const MSG_MAX_WORDS: usize = 16;
pub const CAP_SLOTS:     usize = 4;

#[repr(C)] #[derive(Clone, Copy)]
pub struct MessageInfo {
    pub op:      u16,   // OpCode
    pub n_words: u16,   // significant MR words (bounds the nucleus's read — no over-read)
    pub n_caps:  u8,    // significant cap slots
    pub flags:   u8,
    pub _pad:    u16,
}

#[repr(C)]
pub struct IpcBuffer {
    pub tag:   MessageInfo,
    pub words: [u64; MSG_MAX_WORDS],
    pub caps:  [CapSlot; CAP_SLOTS],
}
#[repr(C)] #[derive(Clone, Copy)] pub struct CapSlot { pub id: CapId, pub rights: Rights }
```

### 4.3 Bulk transfer — granted shared buffers, cap+offset+len only

Anything larger than the MRs (a firmware blob for a future `LOAD_FW` op, an IP-discovery dump) moves
through a **pre-granted shared buffer**: a `Cap<DmaMem>` (or a plain shared region) the driver already
owns, referenced in the request as `(CapId, offset, len)`. The nucleus:

- resolves the cap, checks kind + `READ`/`WRITE`, and **bounds-checks `[offset, offset+len)` against
  the cap's extent** before touching a byte;
- only ever reads/writes *within* that proven-in-bounds window; it never follows an address the driver
  supplied.

This is the linchpin of the AS/cap invariant: the nucleus's entire view of driver-supplied memory is
a set of bounds-checked `(cap, offset, len)` windows. There is no unbounded pointer chase to reason
about, so "the nucleus only touches frames the caller has a cap for" is a local, checkable property.

### 4.4 The `call` ABI

```rust
// client shim (untrusted side); the nucleus entry is the trap handler that reads the IpcBuffer.
pub fn call(ep: CapId, buf: &mut IpcBuffer) -> Result<(), Error>;
```

The nucleus entry validates `MessageInfo.n_words ≤ MSG_MAX_WORDS` and `n_caps ≤ CAP_SLOTS` *first*
(a malformed header can never make it over-read), dispatches on `op`, and writes the response back
into `buf` (result code in `tag.flags`/a reply `MessageInfo`, payload in `words`, returned caps in
`caps`).

---

## 5. The operations

Each op below gives: the `rustproof-abi` request/response types, the capability required, the
nucleus-side precondition it enforces, and the tag. `Response` types omit the ubiquitous
`Result<_, Error>` framing shown in §9.

Client-side typed-cap signatures are shown as comments for readability; the wire form is always the
`#[repr(C)]` struct.

### 5.1 `GET_INFO` — device probe · **VERIFIED** (read-only, cap-guarded)

```rust
// client:  fn get_info(dev: Cap<Device>) -> GetInfoResp
#[repr(C)] pub struct GetInfoReq  { pub device: CapId }
#[repr(C)] pub struct GetInfoResp {
    pub pci_vendor: u16, pub pci_device: u16, pub pci_revision: u8, pub _pad: u8,
    pub pci_subsystem: u32,
    pub num_bars: u8, pub _pad2: [u8; 7],
    pub bars: [BarInfoWire; 6],     // per-BAR: kind (Mmio|Doorbell|Vram|None), len — NOT phys base
    pub vram_size: u64, pub gtt_max: u64,
    pub doorbell_stride: u32, pub num_doorbells: u32,
}
```

- **Capability:** `Cap<Device>` with `READ`.
- **Precondition:** caller holds a valid `Device` cap. The response carries only *non-authority*
  metadata (sizes, PCI IDs, BAR kinds/lengths, doorbell geometry) — **never a physical base address or
  anything that confers reach**. To touch a BAR the driver must still `MAP_BAR` (§5.2); to get memory
  it must `ALLOC_*` (§5.3).
- **Tag: VERIFIED.** Pure read of nucleus-held device state; guarded by cap validity; cannot alter the
  AS/cap state (`ensures old(state) == state`).

### 5.2 `MAP_BAR` — map a register/aperture window · **VERIFIED** decision / **TRUSTED-STUB** store

```rust
// client:  fn map_bar(dev: Cap<Device>, bar: BarIndex, off: u64, len: u64, r: Rights) -> (Cap<Mmio>, u64)
#[repr(C)] pub struct MapBarReq {
    pub device: CapId, pub bar: BarIndex, pub _pad: [u8; 3],
    pub offset: u64, pub len: u64, pub rights: Rights, pub _pad2: u32,
}
#[repr(C)] pub struct MapBarResp { pub mmio: CapId /* slot 0 */, pub vaddr: u64 }
```

- **Capability:** `Cap<Device>` with `MAP` (and `WRITE` if `rights` includes `WRITE`).
- **Precondition (all enforced before any store):**
  1. `bar < num_bars` and `[offset, offset+len)` ⊆ the physical extent of `device.bars[bar]`
     (no over-map past the BAR);
  2. `rights ⊆ device.mmio_rights` (§3.3 monotonicity);
  3. the target VA range in the driver AS is **currently unmapped** — no aliasing over an existing
     mapping (the AS/cap invariant forbids a frame being reachable via two caps of differing rights);
  4. the window is installed **uncached / device-memory** (matching the tri-OS `pgprot_noncached`
     mapping). **NOT EXPRESSIBLE TODAY** — `vspace::PageFlags` has PRESENT/WRITABLE/USER/HUGE/
     NO_EXEC and no PCD/PWT (crates/vspace/src/lib.rs), and `vspace_riscv::PageFlags` has
     V/R/W/X/U/G/A/D and no PBMT (crates/vspace-riscv/src/lib.rs). Any BAR mapped by this tree
     right now is a CACHED mapping, and QEMU TCG cannot tell the difference — so this is the
     precondition most likely to be silently false on real silicon, and the one the only
     available test rig cannot falsify. Adding the flag is a prerequisite for mapping a real
     BAR, not a detail of it.
  5. **the function is not a bus master unless an IOMMU domain bounds its DMA.** Added
     2026-08-14. docs/nucleus-design.md states the premise this whole contract rests on: the
     nucleus grants the driver an `Mmio` capability for the GPU aperture but never for the
     IOMMU aperture, so "the driver can command arbitrary DMA; it cannot touch the tables that
     bound that DMA." Updated 2026-08-19: `CapType::IommuDomain` is no longer a bare enum
     variant. It has a referent (`crates/iommu::Domain`, which governs the real AMD-Vi page
     table) AND an ABI: `MAP_DMA`/`UNMAP_DMA` take a domain capability carrying WRITE plus a
     `Region` capability carrying READ, and the kernel picks the IOVA. The device may write the
     region only if the caller's own region capability carries WRITE, so a READ-only loan
     produces a read-only I/O mapping.
     With no unit programmed, `MAP_DMA` returns `NO_MEM` rather than succeeding: DMA reach that
     nothing bounds is indistinguishable from access to all of memory, so the nucleus declines
     to hand out what it cannot contain.
     What is still true: this contract's own precondition is that a bus-mastering BAR must not
     reach an untrusted process before its DMA is bounded. `DEVICE_PHYS` therefore remains a
     kernel-allocated RAM frame with no bus master behind it — the IOMMU now exists, but the
     device-capability plumbing that would pair a real BAR with its domain does not.
  A fresh `Cap<Mmio>` recording `(phys_base+offset, len, vaddr, rights)` is derived from the device
  cap and inserted in slot 0.
- **Tag: VERIFIED** for the *mapping decision* — the choice of which physical frames become reachable
  at which VA with which rights is the M2 transition, with an `ensures` that the AS gains exactly that
  window and nothing else. The **actual PTE write** (installing the mapping in the page table) is a
  **TRUSTED-STUB** `write_pte` whose *inputs* (frame, VA, rights) are proven in-bounds by (1)–(3).
- **gfx1201 note:** this is how the driver gets the **GC register BAR** to program the HQD, GPUVM
  page-directory-base, and (§5.6) the `CP_HQD_PQ_WPTR` submission poke. The **doorbell BAR is
  normally *not* mapped this way** — see §5.6 for why it is mediated instead.

### 5.3 `ALLOC_VRAM` / `ALLOC_GTT` / `FREE` — DMA memory grant/revoke · **VERIFIED** (AS/cap + DMA-reach)

```rust
// client:  fn alloc_vram(dev, size, align, flags) -> (Cap<DmaMem>, GpuDevAddr)
// client:  fn alloc_gtt (dev, size, align, flags) -> (Cap<DmaMem>, DmaAddr /*IOVA*/)
#[repr(C)] pub struct AllocReq {
    pub device: CapId, pub size: u64, pub align: u64,
    pub kind: u32 /* MemKind: 0=Vram 1=Gtt */, pub flags: u32 /* cacheable, clear-on-alloc, ... */,
}
#[repr(C)] pub struct AllocResp {
    pub mem: CapId,        // slot 0: the DmaMem cap
    pub dma_addr: DmaAddr, // GTT: the IOVA the nucleus assigned in the GPU's AMD-Vi domain
                           // VRAM: the GPU-local device address (does not traverse AMD-Vi)
    pub actual_size: u64,
}
#[repr(C)] pub struct FreeReq { pub mem: CapId }
```

- **Capability:** `Cap<Device>` with `MAP` (VRAM) / `MAP|DMAGRANT` (GTT). `FREE` needs the `DmaMem`
  cap with `WRITE`.
- **`ALLOC_VRAM` precondition:** allocate `size`-aligned frames from the nucleus-owned **VRAM pool**
  (a bump allocator over the framebuffer MC space, per the tri-OS `MAP_VRAM` FB-MC allocator). VRAM is
  device-local and does **not** traverse AMD-Vi for GPU access, so its "DMA-grant" is trivial: the
  frames are recorded as granted, CPU access comes via `MAP_BAR` on the VRAM aperture. Returns a
  `Cap<DmaMem{Vram}>` and the GPU-local device address.
- **`ALLOC_GTT` precondition (the load-bearing one, M3):** allocate host-RAM frames, then **insert
  IOVA→HPA entries into the gfx1201's AMD-Vi domain** at a nucleus-assigned IOVA, *within*
  `authorized(GPU_domain)`. This insertion **is** the DMA-grant the DMA-reach theorem (V3) governs:
  after the op, `reach(GPU_domain)` grows by exactly `{(frame, perm)}` for the granted frames and
  nothing else. Returns `Cap<DmaMem{Gtt}>` with `iova = Some(dma_addr)`.
- **`FREE` precondition:** the drain→unmap-everywhere→**IOTLB-flush**→return-to-pool sequence (§3.4).
  The AMD-Vi entry is removed *and flushed* before the frame can be re-granted (V5/M4 reclaim safety).
  Children (a `GpuvmCtx` backed by these frames) are revoked with it.
- **Tag: VERIFIED.** The frame-pool bookkeeping and the AMD-Vi domain insert/remove are guarded by the
  AS/cap (M2) and DMA-reach (M3) invariants. The AMD-Vi *table/register store* is a TRUSTED-STUB whose
  inputs (frame, IOVA, perm) are proven ⊆ authorized.
- **DmaAddr semantics shift by milestone, contract unchanged:** M0–M2, `dma_addr` is a guest-physical
  address the *host* VFIO pinned (nucleus tracks it but the host enforces); M3+, it is an IOVA the
  *nucleus* assigned in the (emulated, then real) AMD-Vi domain. The type and the op are identical —
  only the enforcer changes.

### 5.4 `MAP_GPU` / `UNMAP_GPU` — GPUVM programming · **UNTRUSTED**

```rust
// client:  fn map_gpu(ctx: Cap<GpuvmCtx>, gpu_va: GpuVa, mem: Cap<DmaMem>, off, len, gpu_pte_flags) -> ()
#[repr(C)] pub struct MapGpuReq {
    pub gpuvm: CapId,     // the driver's GpuvmCtx (page-table backing it owns)
    pub gpu_va: GpuVa,    // GPU virtual address — OPAQUE to the nucleus
    pub mem: CapId,       // a DmaMem the driver wants reachable at gpu_va (its IOVA/dev-addr is the leaf value)
    pub offset: u64, pub len: u64,
    pub gpu_pte_flags: u64, // gfx12 PTE bits chosen by the DRIVER — nucleus does not interpret them
}
#[repr(C)] pub struct UnmapGpuReq { pub gpuvm: CapId, pub gpu_va: GpuVa, pub len: u64 }
```

**Design of record: `MAP_GPU` is not a privileged nucleus operation at all.** The driver already holds
(a) an `Mmio` cap on the GC block and (b) `DmaMem` frames holding its GPUVM page tables (a
`GpuvmCtx`). Programming GPUVM is therefore *entirely within the driver's own grant*: it writes PDB/PTB
entries into frames it owns and pokes the GPUVM page-directory-base register via its MMIO cap. The
nucleus is not in the loop, does not validate the GPU-VA→leaf mapping, and does not trust it. **The
contract keeps `MAP_GPU`/`UNMAP_GPU` as named operations only so the M0 C++ `lite::` port — which
expects the `AMDGPU_LITE_IOC_MAP_GPU` ioctl — links unchanged.** In that compatibility form the
nucleus provides a *bounded memcpy-into-owned-frame* helper:

- **Capability:** `Cap<GpuvmCtx>` with `WRITE`, plus the `Cap<DmaMem>` whose leaf address is being
  installed (with `READ`).
- **Precondition the nucleus *does* enforce (memory-safety only):** the destination PTE bytes land
  **inside the `GpuvmCtx`'s own page-table frames** (in-bounds of a frame the driver owns) — so a
  malicious `MAP_GPU` can corrupt nothing outside the driver's grant.
- **What the nucleus deliberately does *not* enforce:** which `gpu_va` maps to which leaf, what
  `gpu_pte_flags` say, whether the leaf IOVA is even in the AMD-Vi domain. None of that can breach
  isolation (§6), so validating it would be wasted trust surface.
- **Tag: UNTRUSTED.** Correctness of GPUVM is out of scope forever (U3 in the decision doc). Even in
  the compatibility form, the *GPUVM semantics* are untrusted; only the bounded-write is memory-safe.

Why this is safe is the whole point of the design — see §6.

### 5.5 `SETUP_IRQ` — MSI-X → Notification binding · **TRUSTED-STUB**

```rust
// client:  fn setup_irq(dev: Cap<Device>, source: IrqSource, notify: Cap<Notification>) -> Cap<Irq>
#[repr(C)] pub struct SetupIrqReq {
    pub device: CapId, pub source: u16 /* MSI-X vector index */, pub _pad: u16, pub notify: CapId,
}
#[repr(C)] pub struct SetupIrqResp { pub irq: CapId /* slot 0 */ }
```

- **Capability:** `Cap<Device>` with `IRQBIND`, plus a caller-owned `Cap<Notification>`.
- **Precondition:** `source` is a valid MSI-X vector for the device and not already bound; `notify` is
  a valid `Notification` cap the caller owns. The nucleus records the binding and unmasks the vector.
- **Tag: TRUSTED-STUB.** The *routing decision* (which Notification a vector signals) is cap-guarded
  and VERIFIED — a proof that a vector only ever wakes the Notification it was bound to. The **ISR
  body** (ack the MSI-X, read the IH ring status word, signal the Notification) is a small audited
  `unsafe` stub, and **MSI-X injection/remapping fidelity is an assumed hardware axiom (A3)** — an
  out-of-scope escape channel the invariant does not cover. The driver only ever learns "an interrupt
  fired", never gains reach from it.

### 5.6 `RING_DOORBELL` — submission poke · **TRUSTED-STUB**

```rust
// client:  fn ring_doorbell(db: Cap<Doorbell>, index: u32, value: u64) -> ()
#[repr(C)] pub struct RingDoorbellReq {
    pub doorbell: CapId, // a Doorbell cap = a granted sub-range of doorbell indices
    pub index: u32, pub _pad: u32,
    pub value: u64,      // the wptr value to write
}
```

- **Capability:** `Cap<Doorbell>` (a mediated sub-aperture cap, derived at queue-create) with
  `DOORBELL`.
- **Why mediated, not mapped:** the doorbell aperture **aliases every queue and VMID on the device** —
  a raw `MAP_BAR` of it would let the driver ring queues it was never granted (a cross-tenant / cross-
  VMID authority leak). So instead of handing the driver the aperture, the nucleus keeps it and
  validates each ring: `index ∈ doorbell.granted_range`.
- **gfx1201 reality:** the **doorbell is dead under passthrough**, so the real submission is an MMIO
  poke of `CP_HQD_PQ_WPTR` on the GC block for the queue's pipe/HQD. `RING_DOORBELL` therefore also
  covers the wptr-poke form: the nucleus writes `value` to the doorbell/`WPTR` register **for the
  index the cap authorizes**, and only that index.
- **Precondition:** `index` is within the cap's granted doorbell range; `value` is opaque (a ring
  offset — it cannot confer reach, only advance a queue the driver already owns).
- **Tag: TRUSTED-STUB.** The **offset-bounds check** (`index ∈ range`) is VERIFIED — the proof that a
  driver can only ring doorbells in its grant. The **MMIO store itself** is the audited `unsafe` stub.
- **Fast-path alternative (documented, not default):** if a future single-tenant configuration can
  give the driver a *private* doorbell page with no aliasing, the nucleus may `MAP_BAR` that page
  (cap-guarded, VERIFIED mapping) and let the driver write it directly, dropping the per-ring IPC. The
  aliasing analysis above is why that is off by default.

### Operation summary

| Op | Cap required | Nucleus precondition (enforced) | Tag | Invariant / milestone |
|---|---|---|---|---|
| `GET_INFO` | `Device`+READ | valid device cap; returns no authority | VERIFIED | cap-validity |
| `MAP_BAR` | `Device`+MAP | window ⊆ BAR; rights ⊆ device; VA unmapped | VERIFIED (store=stub) | AS/cap (M2) |
| `ALLOC_VRAM` | `Device`+MAP | frames from VRAM pool; device-local | VERIFIED | AS/cap + DMA-grant (M2→M3) |
| `ALLOC_GTT` | `Device`+MAP\|DMAGRANT | IOVA insert ⊆ authorized(GPU_domain) | VERIFIED (store=stub) | DMA-reach (M3, load-bearing) |
| `FREE` | `DmaMem`+WRITE | unmap-everywhere → IOTLB-flush → pool | VERIFIED | reclaim-safety (M4) |
| `MAP_GPU` | `GpuvmCtx`+WRITE, `DmaMem`+READ | write ⊆ driver-owned PT frames | UNTRUSTED | contained by V3/V4 |
| `UNMAP_GPU` | `GpuvmCtx`+WRITE | write ⊆ driver-owned PT frames | UNTRUSTED | contained by V3/V4 |
| `SETUP_IRQ` | `Device`+IRQBIND, `Notification` | valid unbound vector; owns notify | TRUSTED-STUB | routing VERIFIED; ack=stub (A3) |
| `RING_DOORBELL` | `Doorbell`+DOORBELL | index ∈ granted range | TRUSTED-STUB | offset-bounds VERIFIED; store=stub |

---

## 6. Why untrusted GPUVM (`MAP_GPU`) is safe

This is the crux the decision doc (§3) turns on. The original `amdgpu_lite.ko` made GPU page-table
programming **kernel-resident "for security"** (port doc §1L.1 decision 1). Rustproof **inverts that**:
`MAP_GPU` moves *out* of the trusted core and becomes UNTRUSTED — and it is still safe. Here is the
argument, made concrete on gfx1201.

### 6.1 gfx1201 has two independent translation layers

```
   GPU compute wave issues an access to  GPU-VA  X
        │
        │  (1) GPUVM  — the GPU's OWN 4-level MMU (PDB2→PDB1→PDB0→PTB, GCVM_CONTEXT0)
        │      page tables live in DRIVER-OWNED frames; page-directory-base in a GC register.
        │      *** PROGRAMMED BY THE UNTRUSTED DRIVER.  Produces a leaf value: an IOVA. ***
        ▼
   IOVA  Y  (for VRAM-local targets this stays on-die; for system memory it goes on the bus ↓)
        │
        │  (2) AMD-Vi (system IOMMU) — translates the GPU's OUTBOUND bus-master transaction.
        │      Device Table Entry for the gfx1201 BDF points at a NUCLEUS-OWNED page-table root.
        │      *** OWNED AND VERIFIED BY THE NUCLEUS.  This walk is the containment boundary. ***
        ▼
   HPA  Z   — a host physical frame, reachable ONLY if (Y ↦ Z, perm) ∈ nucleus-granted AMD-Vi table
```

The untrusted driver has **total control of stage (1)** and **zero control of stage (2)**. Whatever
GPU-VA→IOVA relation it builds — correct, buggy, or actively malicious — the *only* IOVAs that resolve
to real host memory are those the nucleus put in the AMD-Vi table when it granted DMA memory (§5.3).

### 6.2 The invariants that make it airtight

**DMA-reach (V3, M3):**
> `reach(GPU_domain) ⊆ authorized(GPU_domain)` — the set of `(frame, perm)` pairs the AMD-Vi walk
> yields over *all* IOVAs in the GPU's domain is always a subset of the frames the nucleus granted to
> the driver's DMA capabilities. Preserved across every nucleus IOMMU op and every `ALLOC_GTT`/`FREE`.

Because the driver can only *produce IOVAs* (stage 1) and the nucleus alone decides which IOVAs map to
which frames (stage 2), a GPUVM entry pointing at an IOVA outside `authorized(GPU_domain)` resolves to
**nothing** — the AMD-Vi walk faults. The driver cannot name a frame it wasn't granted, no matter how
it programs GPUVM.

**DTE-config (V4, M3) — the coupled invariant that closes the bypasses:**
> The gfx1201 BDF's Device Table Entry *always*: translation **enabled**, passthrough/bypass
> **disabled**, ATS/PRI **disabled**, and points at a **nucleus-owned** page-table root.

This is what guarantees there is *no device-side path around stage (2)*: no ATS-translated request
that would let the device supply its own HPA, no bypass mode, no un-remapped path. Without V4, V3 would
be vacuous (the device could just turn translation off).

### 6.3 Even the GPUVM page-table walk is contained

A subtle point: stage (1) itself reads memory — the GPU walks its *own* PDB/PTB entries. If those page
tables live in **GTT (system memory)**, the walk is an outbound bus-master read that **also goes
through AMD-Vi**. So even a GPUVM page-directory-base pointing at arbitrary system memory is contained:
the walk reads fault at AMD-Vi unless the page-table frames are themselves in `authorized(GPU_domain)`
— which they are exactly when they came from `ALLOC_GTT`/`ALLOC_VRAM` (the frames backing the
driver's `GpuvmCtx`, §3.3). A malicious driver cannot point GPUVM's root at the nucleus's private
memory and have the GPU walk it: that read is outside the grant and faults.

### 6.4 What `MAP_GPU` being memory-safe still requires (and what it doesn't)

The *only* thing the nucleus enforces on `MAP_GPU` is that the **PTE bytes it writes land in
driver-owned frames** (§5.4 precondition) — a plain AS/cap bounds check, so the op cannot scribble on
the nucleus or another AS. It does **not** validate the GPU-VA→IOVA mapping, the PTE flags, or the leaf
addresses, because §6.2–6.3 already contain any value they could take. This is the design paying off:
the trusted core shrinks (GPUVM logic leaves it entirely), and nothing is lost, because the AMD-Vi
boundary the nucleus *does* own is strictly downstream of everything the driver controls.

### 6.5 Disclosed residuals (not covered by V3/V4)

Named here so the contract doesn't over-claim (decision doc §6 A2/A3, §3 residuals):

- **MSI/MSI-X injection** — an interrupt-remapping fidelity axiom (A3), an out-of-scope channel; the
  doorbell/IRQ path (§5.5) is trusted-stub, not verified.
- **PCIe peer-to-peer** — depends on ACS being present on the path; assumed, not proven (A2). Confirm
  gpu-host topology provides ACS (decision doc §8 R2).
- **ATS/PRI translated requests** — disabled at the DTE (V4). If a future SVM/unified-memory workload
  forced ATS on, the device would become partially trusted and this clean argument weakens — hence the
  **default decision: no SVM, static pinned DMA buffers only** (decision doc §8 open question).

---

## 7. The invariants this contract is the spec surface for (Verus)

The `requires`/`ensures` below are the obligations M1–M3 discharge against *this* contract. Sketches
in Verus idiom (`spec fn`, `proof fn`, `Tracked`, `#[verifier::external_body]`, `vstd` `Set`/`Map`) —
illustrative of the obligation, not claimed to compile against a pinned Verus/Z3/rustc. Design within
Verus's supported subset from day one (decision doc §4 R5).

### 7.1 AS/cap invariant (M2 — V2)

```rust
use vstd::prelude::*;

/// Frames reachable from address space `a` = exactly the frames named by a mapping cap in a's CSpace.
pub open spec fn reachable_frames(s: State, a: AsId) -> Set<Frame>;
pub open spec fn capd_frames(s: State, a: AsId) -> Set<Frame> {
    Set::new(|f: Frame| exists |c: CapId| #[trigger] s.cspace(a).contains(c)
              && s.cap(a, c).maps(f))
}

pub open spec fn as_cap_inv(s: State) -> bool {
    forall |a: AsId| #[trigger] s.has_as(a) ==> reachable_frames(s, a) == capd_frames(s, a)
}

// Every host-contract handler is a transition that PRESERVES it.
fn h_map_bar(Tracked(s): Tracked<&mut State>, a: AsId, req: MapBarReq)
    requires as_cap_inv(*old(s)),
             window_in_bar(*old(s), a, req),          // §5.2 precond 1
             req.rights.subset_of(device_mmio_rights(*old(s), a)), // precond 2
             va_range_unmapped(*old(s), a, req),      // precond 3 (no aliasing)
    ensures  as_cap_inv(*s),
             reachable_frames(*s, a) == reachable_frames(*old(s), a).union(bar_frames(req)),
             forall |b: AsId| b != a ==> reachable_frames(*s, b) == reachable_frames(*old(s), b), // isolation
{ /* ... */ }
```

The cross-AS clause (`b != a ==> unchanged`) is **inter-address-space non-interference** (M2): a
driver op never changes any other AS's reachable set. Every op in §5 carries the analogous
`ensures`.

### 7.2 DMA-reach + DTE-config invariants (M3 — V3/V4)

```rust
/// (frame, perm) pairs the AMD-Vi walk yields over the GPU domain's IOVAs.
pub open spec fn reach(s: State, d: Domain) -> Set<(Frame, Perm)>;
/// what the nucleus granted to the driver's DMA caps for that domain.
pub open spec fn authorized(s: State, d: Domain) -> Set<(Frame, Perm)>;

pub open spec fn dma_reach_inv(s: State, d: Domain) -> bool { reach(s, d).subset_of(authorized(s, d)) }

pub open spec fn dte_config_inv(s: State, bdf: Bdf) -> bool {
    let e = s.dte(bdf);
    e.translation_enabled() && !e.bypass() && !e.ats_enabled() && e.root() == s.nucleus_owned_root(bdf)
}

// ALLOC_GTT is the load-bearing DMA-grant.
fn h_alloc_gtt(Tracked(s): Tracked<&mut State>, a: AsId, req: AllocReq) -> (r: AllocResp)
    requires dma_reach_inv(*old(s), gpu_domain(a)), dte_config_inv(*old(s), gpu_bdf(a)),
             frames_from_pool(*old(s), req.size),               // frames come from a nucleus pool, not conjured
             iova_within_authorized(*old(s), a, req),           // insert ⊆ authorized
    ensures  dma_reach_inv(*s, gpu_domain(a)), dte_config_inv(*s, gpu_bdf(a)),  // preserved
             reach(*s, gpu_domain(a)) == reach(*old(s), gpu_domain(a)).union(granted(r)),
{ /* insert IOVA→HPA into AMD-Vi table via a TRUSTED-STUB store with proven-in-bounds inputs */ }
```

`MAP_GPU` (§5.4) has **no** `dma_reach` obligation — it is not a nucleus transition on the AMD-Vi
state at all. The theorem "a driver-programmed GPUVM cannot exceed `authorized`" is a *corollary* of
`dma_reach_inv` holding over every reachable IOVA, proven once (§6.2), not per-`MAP_GPU`-call.

### 7.3 Reclaim safety (M4 — V5)

```rust
fn h_free(Tracked(s): Tracked<&mut State>, a: AsId, req: FreeReq)
    requires dma_reach_inv(*old(s), gpu_domain(a)),
    ensures  dma_reach_inv(*s, gpu_domain(a)),
             // the freed frames are unreachable until re-granted — no stale-IOTLB window
             forall |f: Frame| freed(req).contains(f) ==>
                 !reach_any_perm(*s, gpu_domain(a), f),
             iotlb_flushed_before_return(*old(s), *s, freed(req)),  // the flush precedes pool return
{ /* remove DTE/domain entry → flush IOTLB (stub) → return to pool, in that order */ }
```

### 7.4 The trusted stubs' assumed contracts

```rust
// Not proven — its ensures is an ASSUMED axiom (A4). Kani-checked (§8). Inputs proven in-bounds by caller.
#[verifier::external_body]
fn write_pte(pt: PhysAddr, idx: usize, val: Pte)
    requires idx < ENTRIES_PER_TABLE, pt_is_nucleus_owned(pt),
    ensures  pte_at(pt, idx) == val
{ unsafe { core::ptr::write_volatile(/* ... */) } }

#[verifier::external_body]
fn amdvi_invalidate(domain: Domain)
    ensures iotlb_empty(domain)         // ASSUMED: hardware honors the invalidate (A1)
{ /* MMIO to the AMD-Vi invalidation queue */ }
```

Each op in §5 maps to exactly one of these obligation shapes. That mapping — op → invariant → milestone
— is the whole reason to freeze this contract at M0 (decision doc §9 action 3).

---

## 8. Trusted-stub inventory (audited `unsafe`, Kani targets)

The **complete** set of `#[verifier::external_body]` primitives this contract depends on (sized at M1,
decision doc §5/§8 open question). Everything outside this list must be proven `unsafe`-free.

| Stub | Used by | Assumed contract | Axiom | Kani harness |
|---|---|---|---|---|
| `read_mmio32`/`write_mmio32` | MAP_BAR (store), RING_DOORBELL, bring-up | volatile access at proven-in-bounds addr | A4 | bounds + no-alias |
| `write_pte` | MAP_BAR, ALLOC_* | writes one in-bounds entry, no neighbor effect | A4 | idx < ENTRIES; single-entry write |
| `amdvi_write_dte` | ALLOC_GTT, boot | installs DTE with translation-on/bypass-off/ATS-off | A1 | DTE bit-field encoding |
| `amdvi_map`/`amdvi_unmap` | ALLOC_GTT/FREE | one IOVA→HPA entry, in-bounds | A1 | perm/level encoding |
| `amdvi_invalidate` (IOTLB) | FREE | empties IOTLB for domain | A1 | — (hardware) |
| `tlb_invalidate` | MAP_BAR/FREE | empties CPU TLB for range | A4 | — |
| `msix_ack` + IH read | SETUP_IRQ ISR | acks vector, reads status word | A3 | — |
| `ctx_switch` (asm) | scheduler | saves/restores register file | A4 | — |

Kani *finds bugs* in these; it does not prove their absence (decision doc §4). Their `ensures` are the
`A*` axioms — a green nucleus proof is bounded by them (decision doc §1 TCB).

---

## 9. Error model

One `#[repr(i32)]` error enum; every op returns `Result<Resp, Error>` marshalled as a status word +
payload. Errors are **total and non-authority-bearing** (they never leak a frame/address a cap didn't
already confer):

```rust
// error.rs
#[repr(i32)] #[derive(Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Ok = 0,
    BadCap        = 1, // CapId absent / wrong kind / insufficient rights
    OutOfRange    = 2, // window/offset/len exceeds the cap's extent
    RightsExceeded= 3, // requested rights ⊄ parent rights
    Aliased       = 4, // target VA already mapped (MAP_BAR precond 3)
    NoMem         = 5, // pool exhausted (ALLOC_*)
    Busy          = 6, // FREE on memory with in-flight reference / unquiesced dispatch
    BadIova       = 7, // ALLOC_GTT could not place inside authorized(GPU_domain)
    IrqInUse      = 8, // MSI-X vector already bound
    AbiMismatch   = 9, // ABI_VERSION handshake failed
    Malformed     = 10,// n_words/n_caps out of the IPC-buffer bounds
}
```

A faulting op is a **no-op on nucleus state** — `ensures ret.is_err() ==> *s == *old(s)` on every
handler, so error paths never partially mutate the AS/cap/IOMMU state (this matters for the proof:
there is one post-state to reason about per op, success or failure).

---

## 10. What's frozen for M0, what moves later

- **Frozen at M0 (this doc):** the op set, the request/response types, the capability kinds + rights,
  the IPC-buffer layout, the tags. The C++ `lite::` port (`AMDGPU_LITE_IOC_*` → these ops) links
  against `ABI_VERSION = 1` and does not change through M4. `MAP_GPU`/`UNMAP_GPU` ship as the
  bounded-write compatibility helper so the C++ driver is unmodified.
- **Enforcement moves, contract does not:** M0–M2 the host VFIO enforces the DMA parts (`dma_addr` =
  host-pinned guest-physical); M3 the nucleus enforces them against an emulated AMD-Vi (`dma_addr` =
  nucleus-assigned IOVA); M4 against real AMD-Vi. Same types, same preconditions, different enforcer —
  see the §5.3 note and decision doc §3 staging table.
- **Owed decisions that touch this contract (decision doc §8):** confirm gpu-host provides **ACS** (§6.5
  P2P residual); confirm QEMU can present an **emulated/nested AMD-Vi with shadowing** (else M3's
  `ALLOC_GTT` load-bearing path jumps straight to bare metal); confirm the x86 **ReBAR/BAR-size**
  posture (affects the VRAM-aperture `MAP_BAR` window and the top-of-VRAM IP-discovery read).
- **Explicitly out, forever:** that GPUVM (`MAP_GPU`) is correct (U3); that the GPU computed the right
  answer (G2); that MSI-X/P2P/ATS are non-escaping beyond the disclosed axioms (A2/A3).
```

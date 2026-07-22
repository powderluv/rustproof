# Rustproof — Verified Nucleus Internal Architecture

`docs/nucleus-design.md` · status: **design, pre-M1 (staffing-gated)** · target: KVM guest on `gpu-host` (x86_64) dispatching gfx1201 compute via the untrusted C++ `lite::` driver

---

## 0. Scope and framing

This document specifies the internals of the Rustproof **nucleus**: a new ~6–8K-SLOC Rust+Verus isolation kernel. It does **not** re-open decided questions. For grounding, the load-bearing decisions are:

- The base is a **new** small nucleus, not Redox and not seL4. seL4 and Atmosphere are borrowed **as patterns**, not code.
- The gfx1201 `lite::` driver is an **untrusted user process**. The nucleus trusts nothing it writes to GPU registers or GPUVM.
- Verification is **Verus** for the safe-Rust core; **Kani** (bounded model checking) for the parts of the unsafe stub that are logic rather than asm.
- **Two guarantees, split permanently:**
  1. *Isolation / DMA-containment* — **in scope, verifiable.**
  2. *GPU compute correctness* — **permanently out of scope.** The nucleus never reasons about what a wave computes, only about what memory the device can touch.
- The **crux proof** is a DMA-reach invariant over **nucleus-owned AMD-Vi IOMMU tables**. GPUVM stays untrusted and produces only IOVAs.
- **When the IOMMU proof is load-bearing:**
  - **M0–M2** run under **plain VFIO**: the *host* programs the physical IOMMU, so guest isolation is host-enforced. The nucleus's IOMMU code is dormant/stubbed. The M1/M2 proofs (memory-safety, inter-AS isolation) stand on their own without the IOMMU proof.
  - **M3** introduces an **emulated vIOMMU** (QEMU `intel-iommu`/`amd-iommu` device model in the guest) — the first point where the nucleus's DMA-reach proof does real work.
  - **M4** is **bare-metal AMD-Vi** on real hardware, where the proof is the whole ballgame.
- **No proof engineer yet.** M1 and beyond are gated on that hire. This doc is written so the design is fixed before proof work starts (the expensive mistake is discovering the state representation is unprovable *after* writing the exec code).

### 0.1 What is trusted vs. verified (the one table that matters)

| Component | Status | Why |
|---|---|---|
| Capability table + derivation | **Verified (Verus)** | Bounded, pure state machine |
| Address-space / page-table manager | **Verified (Verus)** | Reach invariant is the M2 result |
| IPC (endpoints/notifications) | **Verified (Verus)** | No-authority-amplification is a state invariant |
| Scheduler + sched-contexts | **Verified (Verus)**, functional only | Big-lock removes interleaving; timing **not** covered |
| AMD-Vi domain manager | **Verified (Verus)** from M3 | Device-reach invariant is the crux |
| asm context switch | **Trusted, hand-audited** | Verus has no model of inline asm |
| TLB / IOMMU invalidate primitives | **Trusted, hand-audited** | MMIO side effects outside Verus's memory model |
| MMIO accessors | **Trusted, hand-audited** | Volatile hardware access is not a Rust/Verus value semantics |
| The hardware itself (MMU walker, AMD-Vi engine) | **Trusted assumption** | We prove properties of our *software model* of the tables; soundness requires HW to interpret them per spec. seL4 has the identical assumption. |
| gfx1201 `lite::` driver, GPUVM contents | **Untrusted** | Confined by the IOMMU, never trusted |

Everything in the "Trusted" rows is enumerable and small by construction — that is the entire point of the boundary in §7.

### 0.2 Crate / file layout

```
nucleus/
  Cargo.toml                     # verus + kani dev-dep; no_std
  src/
    lib.rs                       # #![no_std], big-lock entry, syscall demux
    cap/
      mod.rs                     # Cap, CapType, Rights, CapTable
      derive.rs                  # retype(), the append-only derivation forest
      spec.rs                    # ghost model + invariants (Verus)
    mm/
      frame_table.rs             # global physical frame view (manual mgmt)
      pt.rs                      # x86_64 4-level walk/map/unmap (exec)
      reach.rs                   # reachable_frames / mapped_cap_frames (spec)
      as.rs                      # AddressSpace object
    ipc/
      endpoint.rs                # synchronous rendezvous
      notification.rs            # async signal words (IRQ delivery)
      spec.rs                    # no-amplification invariant
    sched/
      tcb.rs                     # thread control block
      sctx.rs                    # scheduling contexts (MCS-style)
      sched.rs                   # big-lock round/priority scheduler
    iommu/                       # added at M3
      dte.rs                     # AMD-Vi device-table-entry programming
      iopt.rs                    # I/O page tables (v1 long-mode-like)
      cmdq.rs                    # command buffer + COMPLETION_WAIT
      domain.rs                  # IOMMUDomain object
      reach.rs                   # device_reachable (spec) — the crux
    trusted/                     # the TCB stub; every fn documented + audited
      switch.rs                  # #[verifier::external_body] asm context switch
      tlb.rs                     # invlpg / cr3 reload
      mmio.rs                    # volatile read/write, IOMMU cmd doorbell
      contracts.md               # the audit ledger (see §7)
```

Illustrative SLOC budget (exec + inline spec/proof, the "6–8K" figure):

| Area | exec | spec+proof | notes |
|---|---:|---:|---:|
| cap | 500 | 700 | append-only forest is *cheap* to prove |
| mm | 900 | 1800 | reach invariant dominates cost |
| ipc | 400 | 500 | |
| sched | 500 | 400 | functional only |
| iommu (M3+) | 700 | 1400 | crux proof |
| trusted stub | 300 | — | not proven; audited |
| **total** | **~3.3K** | **~4.8K** | **≈ 8.1K**, trim to budget by simplifying `mm`/`iommu` walkers |

The proof-to-code ratio here (~1.5:1 inline) is far below seL4/Isabelle's ~20:1 because Verus proofs live next to the code and the **no-revocation decision** deletes the single most expensive proof obligation in a capability kernel (the seL4 mapping-database `revoke` invariants). That deletion is worth more to tractability than any tooling choice.

---

## 1. Capability system

### 1.1 Object and capability model

Eight typed capabilities, each a rights-decorated reference to a kernel object:

| CapType | Object | Confers |
|---|---|---|
| `Untyped` | contiguous physical region + watermark | retype into other objects |
| `Frame` | one 4 KiB (or 2 MiB) physical frame | map into a `PageTable` |
| `PageTable` | one paging structure (PML4/PDPT/PD/PT) | install mappings; root = an AS |
| `Endpoint` | synchronous IPC port | send/recv (rights-gated) |
| `Notification` | async signal word | signal/wait; IRQ target |
| `Tcb` | thread control block | configure/resume a thread |
| `IommuDomain` | AMD-Vi domain (DTE + I/O page tables) | map Frames for device DMA (M3+) |
| `Mmio` | a device register aperture (phys range) | map device regs into an AS |

```rust
// cap/mod.rs — illustrative, not compiled
pub enum CapType { Untyped, Frame, PageTable, Endpoint,
                   Notification, Tcb, IommuDomain, Mmio }

pub struct Rights { pub read: bool, pub write: bool, pub grant: bool }

pub struct Cap {
    pub ctype:  CapType,
    pub obj:    ObjId,        // index into the global object table
    pub rights: Rights,
    pub badge:  u64,          // endpoint sender identity; 0 otherwise
}
```

**CSpace representation — deliberately not seL4's guarded CNode.** seL4's radix-guarded CNode tree is powerful but is one of the hardest parts of its proof. For a 6–8K-SLOC nucleus we use a **single-level, fixed-radix capability table per address space** (e.g. 1024 slots), indexed by a small integer `CapHandle`. Bounded array + integer index → every access is a decidable bounds check, and the CSpace invariants are quantifier-light. We lose sparse/hierarchical CSpaces; for a system whose "userspace" is one GPU driver plus a handful of service threads, that is a non-cost.

`Mmio` and `IommuDomain` are the two caps that make the untrusted-device story work: the nucleus grants the `lite::` driver an `Mmio` cap for the **GPU register + doorbell aperture** (so it can drive the GPU directly) but **never** an `Mmio` cap for the **IOMMU aperture** — the nucleus holds that exclusively. The driver can therefore command arbitrary DMA; it cannot touch the tables that bound that DMA. This is exactly Atmosphere's "untrusted device behind an IOMMU the trusted base owns" pattern, made explicit as two distinct MMIO caps.

### 1.2 Derivation model — append-only forest, **no mid-execution revocation**

**Decision (from the plan, not re-litigated):** the nucleus **prohibits fine-grained, mid-execution revocation** in the style of Atmosphere's flat permission model. A cap, once derived and mapped, cannot have its backing memory yanked while the holder runs. **Resources return only on address-space termination**, atomically.

This is the single most consequential structural decision, for two reasons:

1. **Verified user code can't be sabotaged by the kernel.** A user thread whose safety proof assumes its stack/heap stay mapped would be unsound if the kernel could revoke a frame under it. Prohibiting revocation makes "the frames I hold stay mapped for my lifetime" a *kernel-guaranteed* premise that user proofs may rely on.
2. **It deletes seL4's mapping database.** seL4 tracks a full capability-derivation tree (the CDT / mdb) precisely so `revoke` can recursively invalidate descendants; the CDT invariants are a large fraction of its proof. Rustproof needs none of it.

Concretely, derivation is an **append-only forest**:

- Each `Untyped` cap carries a **monotonic watermark** (seL4-style). `retype(untyped, ctype, n)` bump-allocates `n` objects from the free tail of the region and advances the watermark. There is no free-list and no in-place free.
- A derived object's provenance is a static parent edge (which Untyped it came from). Edges are never rewritten.
- **Reclamation happens once, at teardown.** When an `AddressSpace` (equivalently, its `Tcb`'s process) terminates, the nucleus walks its owned objects, unmaps them (IOMMU domains too, §5), and resets the watermarks of the Untypeds it exclusively owned. Shared Untypeds are reference-counted at the granularity of whole subtrees, not individual frames.

The Verus invariant is a **disjointness + containment** property, not a reachability-of-revoke property:

```rust
// cap/spec.rs — the derivation well-formedness invariant
spec fn derivation_wf(objs: Map<ObjId, Obj>) -> bool {
    // every non-Untyped object's physical range lies inside exactly one
    // parent Untyped, below that Untyped's watermark, and sibling ranges
    // carved from the same Untyped are pairwise disjoint.
    forall|id: ObjId| #[trigger] objs.contains_key(id) ==>
        match objs[id].kind {
            Obj::Derived { parent, range } =>
                objs.contains_key(parent)
                && objs[parent].is_untyped()
                && range.below(objs[parent].watermark())
                && (forall|sib: ObjId| siblings(objs, id, sib)
                        ==> range.disjoint(objs[sib].range())),
            _ => true,
        }
}
```

Because the forest is append-only, `retype` only ever has to prove it *extends* a well-formed map to a well-formed map — a local, monotone step. No `retype` step can ever violate an existing object's premises, which is the machine-checked form of "no memory revoked under a running thread."

**Cost of this choice (stated honestly):** no memory reclamation until process exit means a long-lived driver that churns buffers must recycle within its own already-mapped frames (the `lite::` driver already does bump/pool allocation, so this is a fit). A future "revoke" would be a major re-verification, not an incremental change. That trade is accepted.

---

## 2. Address-space & page-table manager

### 2.1 Structure

Standard x86_64 4-level paging: PML4 → PDPT → PD → PT, 4 KiB base pages (2 MiB large pages optional at PD level). An `AddressSpace` object owns exactly one PML4 (a `PageTable` cap installed as root). `map(as, vaddr, frame_cap, rights)` walks/allocates intermediate tables (each backed by a `PageTable` cap the AS holds) and installs a leaf PTE whose permission bits are `rights ∩ frame_cap.rights`.

Interrupts are masked while the nucleus holds the big lock (§4), so page-table edits are never concurrent with a hardware walk on the same core; cross-core is out of scope (single-vCPU guest at M0–M4).

### 2.2 Manual, non-borrow-checker memory management

Page-table frames alias in ways Rust's ownership model cannot express: a physical frame is simultaneously (a) described by a `PageTable`/`Frame` cap, (b) pointed at by a parent PTE, and (c) an entry in the **global frame table**. The borrow checker cannot represent "the nucleus has a global, mutable view of all of physical memory, sliced by page tables that cross-reference each other." This is precisely the situation Atmosphere handles with a **flat, global memory view under manual management** — and we borrow that shape.

Verus lets us keep the manual structure *and* prove it sound using **tracked ghost permissions** rather than the borrow checker:

```rust
// mm/frame_table.rs — global physical view, manual management
// The nucleus holds a tracked permission per owned frame; possession of the
// PointsTo<PtFrame> token *is* the proof of exclusive access to that frame.
struct FrameTable {
    perms: Tracked<Map<PAddr, PointsTo<PtFrame>>>,  // ghost: who may write what
    // exec-visible frame metadata lives alongside, indexed by PAddr>>12
}
```

The `Tracked<Map<PAddr, PointsTo<..>>>` is erased at runtime (zero cost) but lets Verus enforce that no two code paths mutate the same frame without the corresponding token — the soundness the borrow checker would give us if it could see through the aliasing. `map`/`unmap` thread these tokens explicitly; a leaf PTE install requires the token for both the leaf table frame and (read-only) the target frame's cap.

### 2.3 The Address-Space Reach Invariant (ASRI) — the M2 result

The isolation-relevant property: **the set of physical frames reachable through an address space's page tables equals exactly the set of frames it holds mapped `Frame` caps for, and no mapping grants more than its cap's rights.** "Reachable == capabilitied."

```rust
// mm/reach.rs
// Ghost walk of the actual page-table bytes: follow present PML4E→PDPTE→PDE→PTE
// and collect leaf physical frames with their effective rights.
spec fn reachable(pml4: PAddr, ft: FrameTable) -> Map<PAddr, Rights>;

// The frames this AS holds Frame caps for that are currently mapped.
spec fn capabilitied(cspace: CapTable, as: AddressSpace) -> Map<PAddr, Rights>;

// M2 theorem, maintained by every map/unmap:
proof fn asri(as: AddressSpace, ft: FrameTable, cs: CapTable)
    requires as_wf(as, ft, cs)
    ensures  reachable(as.pml4, ft) =~= capabilitied(cs, as)
{ /* maintained inductively; each map/unmap is a local edit proof */ }
```

`=~=` is Verus's extensional-equality operator on maps. `reachable` walks the **real bytes** of the tables (via the frame-table tokens), so the theorem is not about an abstract model that might drift from the installed tables — it is about the tables the hardware will actually walk. **Inter-AS isolation (M2)** is then a corollary: two address spaces with disjoint `capabilitied` sets on writable frames have disjoint writable reach; no AS can reach a frame it holds no cap for. The only thing left trusted is that the CPU's MMU interprets these tables per the Intel/AMD paging spec — the standard hardware-model assumption (see §0.1), identical to seL4's.

---

## 3. IPC

### 3.1 Synchronous endpoints (seL4-style rendezvous)

`Endpoint` caps implement blocking rendezvous. A `send` blocks until a matching `recv` (and vice versa); at rendezvous the nucleus copies a small fixed message (message registers — a handful of machine words) sender→receiver under the big lock, so the transfer is atomic and non-interleaved. Sender identity is conveyed by the endpoint cap's **badge** (seL4's mechanism): a service mints badged endpoint caps for its clients, reads the badge on receive, and thereby authenticates callers without a nameserver.

`Notification` caps are asynchronous signal words (a bitfield): `signal` sets bits, `wait` blocks until nonzero and clears. Their primary job here is **IRQ delivery**: the nucleus's interrupt handler `signal`s a Notification bound to the driver thread (§5.4), turning a hardware GPU interrupt into a wakeable userspace event. This keeps the `lite::` driver's IRQ path entirely in userspace, mirroring seL4's user-mode driver model.

### 3.2 No authority amplification

The invariant: **an IPC never grants the receiver authority the sender did not explicitly and legitimately delegate.** Concretely:

- Message registers carry **data**, not capabilities, by default.
- A capability transfer happens **only** when the sender uses an endpoint cap bearing the `grant` right and names a cap it *already holds*; the receiver gains a copy no stronger than the sender's (rights are intersected, never widened).
- Receiving a message can never let you name a kernel object you couldn't already name — endpoints are not an object-lookup channel.

```rust
// ipc/spec.rs
// After any IPC step, the receiver's CSpace is its prior CSpace plus, at most,
// caps the sender held and explicitly granted, each with rights ⊆ the sender's.
spec fn no_amplification(pre: CapTable, post: CapTable,
                         sender: CapTable, granted: Set<CapHandle>) -> bool {
    forall|h: CapHandle| #[trigger] post.holds(h) ==>
           pre.holds(h)
        || (granted.contains(h)
            && exists|sh: CapHandle| sender.holds(sh)
                 && post[h].obj == sender[sh].obj
                 && post[h].rights.subset_of(sender[sh].rights))
}
```

This is what makes the confinement story compositional: the driver process can talk to the nucleus and to service threads without any of those exchanges silently enlarging what memory or devices it can reach.

---

## 4. Scheduling + scheduling contexts

### 4.1 Scheduling contexts (MCS-style)

We adopt seL4's **MCS** separation of *authority to run a thread* (`Tcb`) from *authority to consume CPU time* (`SchedContext`). A `SchedContext` cap carries a `(budget, period)` reservation; a thread runs only while a SchedContext is bound to it and has budget. This makes CPU time a **capability-controlled resource** on the same footing as memory, which the M5 confinement assurance case needs: a confined component's CPU consumption is bounded by the SchedContexts it was granted, not left to scheduler goodwill.

```rust
// sched/sctx.rs
pub struct SchedContext {
    pub budget:   Ticks,   // per period
    pub period:   Ticks,
    pub refill:   Ticks,   // remaining this period
    pub bound_to: Option<TcbId>,
}
```

The scheduler itself is a small fixed-priority round-robin over runnable, budgeted threads — deliberately boring, because the interesting resource-accounting is in the SchedContext, not the queue discipline.

### 4.2 Big-lock concurrency

The **entire nucleus runs under one big lock**, and interrupts are masked while held. Rationale: it removes interleaving from the proof obligation entirely — every syscall is a single atomic transition of the nucleus state machine, and Verus reasons about sequential code. This is the right call for a first verified nucleus and costs nothing at M0–M4, where the guest is single-vCPU.

**SMP caveat (stated, not solved):** scaling past one core requires either (a) a multikernel partition — one nucleus instance per core, sharing nothing, which preserves the big-lock proof per instance — or (b) fine-grained locking, which reintroduces interleaving reasoning that Verus can do but that is a large new verification effort. Neither is in scope. The design does not paint us into a corner: the multikernel route keeps the existing proofs verbatim.

### 4.3 Timing-channel caveat (important, honest)

**Verus proves functional and safety properties. It does not prove the absence of timing side channels, and Rustproof does not claim to.** The big lock and masked interrupts make the nucleus *functionally* deterministic per transition, but scheduling decisions, shared-cache contention, and IOMMU/TLB state remain **covert/timing channels** between mutually distrusting components. This is the same boundary seL4 draws: its functional-correctness and integrity proofs do not cover timing, and timing-channel mitigation (cache coloring, kernel-page flushing, deterministic scheduling) is separate, partial, and hardware-dependent work. We inherit that limitation deliberately. Anyone reading the M5 assurance case must read "confinement" as *storage/DMA confinement*, not *timing confinement*.

---

## 5. AMD-Vi IOMMU domain manager (added at M3)

This is where the crux proof lives. It is dormant at M0–M2 (host owns the physical IOMMU under plain VFIO), exercised against QEMU's emulated `amd-iommu` at M3, and load-bearing on real hardware at M4.

### 5.1 Objects and ownership

An `IommuDomain` cap owns: (a) a **Device Table Entry (DTE)** for the passed-through gfx1201's BDF, (b) an **I/O page table** tree, and (c) exclusive use of the nucleus's single **command buffer** for invalidations. The nucleus holds the sole `Mmio` cap for the IOMMU register aperture; no user process, including the driver, can reach it (§1.1).

The passthrough GPU appears at a fixed guest PCI address under `start-gpu-vm.sh`'s VFIO topology; the nucleus reads its BDF from the (host-provided, at M3 emulated) ACPI IVRS table and indexes the Device Table by that 16-bit BDF.

### 5.2 DTE programming (trusted format, verified contents)

Each DTE is a 256-bit entry. The nucleus programs, for the gfx1201 BDF:

- **V** (valid) and **TV** (translation valid) set.
- **Page-table root pointer** → the root of the nucleus-owned I/O page table.
- **Mode** field = number of I/O page-table levels (1–6); we use a fixed level count sized to the guest's physical address width.
- **IR / IW** (I/O read / write allowed) — the coarse device permission gate.
- **ATS disabled.** We do **not** trust a device-side IOTLB. If ATS/PRI were enabled, the device could cache translations we'd then have to invalidate remotely and reason about; disabling it makes the in-IOMMU tables the *sole* authority and our invalidation (5.3) authoritative. Concrete soundness win for a small config cost.
- **Interrupt remapping enabled**, with the IRTE pointing at a nucleus-owned interrupt-remapping table (§5.4). This is not optional decoration: an MSI is a DMA write to `0xFEEx_xxxx`, so *interrupt containment is part of DMA containment*. Without remapping, a malicious driver could program the GPU to issue MSIs impersonating any vector — a full escape. The nucleus maps exactly the GPU's allowed vector to the driver's Notification and nothing else.

The bit-packing of the DTE and the I/O PTE format live in `iommu/dte.rs` / `iommu/iopt.rs` and are **trusted format code** (like the CPU PTE packing) — Verus proves the *semantic content* (which frames are reachable) over a spec model of the entries, and we hand-audit that the packing realizes that model per the AMD I/O Virtualization spec.

### 5.3 I/O page tables and invalidation strictness

The AMD-Vi v1 I/O page table is long-mode-like: up to 6 levels, 4 KiB leaves, per-entry present + R/W + a "next-level" encoding. `iommu/iopt.rs` walks/installs exactly as `mm/pt.rs` does for the CPU, and the same tracked-permission discipline (§2.2) applies.

**Invalidation strictness is the load-bearing runtime obligation.** Any change that *removes or reduces* device access — unmap, permission downgrade, domain reassignment, or a DTE change — is not effective until the nucleus has:

1. issued `INVALIDATE_IOMMU_PAGES` (and `INVALIDATE_DEVTAB_ENTRY` for DTE edits) into the command buffer,
2. issued a `COMPLETION_WAIT` with a completion store/fence, and
3. **polled that completion to success** before the freed frame may be returned to any other domain or the change relied upon.

No lazy or deferred invalidation of removed mappings is permitted. This is the DMA analogue of a TLB shootdown, and the device-reach proof is only sound because stale IOTLB entries cannot outlive an unmap. The **no-revocation decision pays off here too**: because unmaps happen only at domain teardown (driver process exit), this synchronous, completion-waited path is exercised on a rare, non-latency-critical path, not on every buffer recycle.

### 5.4 Interrupt path

GPU IRQ → AMD-Vi interrupt remapping (nucleus-owned IRTE) → remapped vector → nucleus IDT handler (under big lock) → `signal` on the driver's bound `Notification` → driver `wait` returns in userspace. The driver never sees a raw device vector and cannot originate one.

### 5.5 The Device Reach Invariant (DRI) — the crux

```rust
// iommu/reach.rs — the crux proof
// Physical frames the device can reach = frames actually mapped in this
// domain's I/O page tables (ATS off ⇒ no device-cached bypass).
spec fn device_reachable(dom: IommuDomain, ft: FrameTable) -> Map<PAddr, Rights>;

// Frames the driver holds Frame caps for and explicitly granted to the domain.
spec fn granted(dom: IommuDomain, cs: CapTable) -> Map<PAddr, Rights>;

// M3/M4 theorem, maintained by every iommu map/unmap+invalidate:
proof fn dri(dom: IommuDomain, ft: FrameTable, cs: CapTable)
    requires iommu_domain_wf(dom, ft, cs), invalidations_complete(dom)
    ensures  device_reachable(dom, ft) =~= granted(dom, cs)
{ /* structurally identical to asri (§2.3), over I/O tables */ }
```

The whole confinement argument, in one paragraph: the untrusted `lite::` driver drives the GPU directly via its `Mmio` cap and programs **GPUVM** however it likes — buggy, malicious, doesn't matter. GPUVM output is nothing but **IOVAs** presented to the IOMMU. The IOMMU is the single choke point, and its tables are owned and proven by the nucleus. `dri` says every IOVA the IOMMU will translate lands only on a frame the driver was explicitly *granted* — so the maximum blast radius of an arbitrarily-compromised GPU stack is exactly the frames the driver already legitimately holds. That is Atmosphere's "untrusted device behind an IOMMU" pattern, upgraded from architecture to a machine-checked theorem. GPU *compute correctness* is never touched — only *reach* — which is precisely the two-guarantee split.

---

## 6. The trusted unsafe stub

### 6.1 What is in it (and only this)

A few hundred SLOC, enumerated in `trusted/contracts.md` (the audit ledger). Nothing else in the nucleus may contain `unsafe`, inline asm, or volatile access.

| Primitive | File | Why it can't be Verus-proven |
|---|---|---|
| Context switch (save/restore GPRs, `CR3` swap, seg/FS-GS, lazy XSAVE) | `trusted/switch.rs` | Inline asm has no Verus value semantics |
| TLB invalidate (`invlpg`, `CR3` reload, PCID) | `trusted/tlb.rs` | Hardware side effect, not a memory value |
| IOMMU invalidate (write command buffer, ring doorbell, poll completion) | `trusted/mmio.rs` | MMIO + hardware fence semantics |
| MMIO accessors (volatile `read`/`write` to device & IOMMU regs) | `trusted/mmio.rs` | Volatile is outside Verus's memory model |
| Interrupt entry stubs, GDT/IDT install | `trusted/switch.rs` | Asm trampolines |

Boot into long mode is handled by the KVM/OVMF path before the nucleus runs, so it is **not** in the stub.

### 6.2 How the boundary is made explicit

Every trusted primitive is a Verus `#[verifier::external_body]` function: the **body is not checked**, but it carries a `requires`/`ensures` contract that the verified core relies on. Verus assumes the `ensures`; the human audit verifies the real body actually delivers it.

```rust
// trusted/tlb.rs
#[verifier::external_body]
pub fn invalidate_page(as: &AddressSpace, va: VAddr)
    requires as.big_lock_held(), va.page_aligned()
    ensures  no_stale_tlb(as, va)   // ASSUMED by verified mm; DISCHARGED by audit
{
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) va.0, options(nostack)); }
}
```

The complete set of `external_body` functions **is** the trusted computing base's proof interface — it is finite, grep-able (`grep -rn 'external_body' src/trusted`), and each entry has a corresponding audit note in `contracts.md` recording who verified that the body satisfies the contract and against which spec (Intel SDM §4.10 for `invlpg`, AMD IOMV spec for the IOMMU commands, etc.).

### 6.3 Kani for the non-asm parts

Where a stub function is *logic wrapped around* a hardware poke — command-buffer ring-index arithmetic, MMIO offset bounds, completion-poll loop termination — the logic is factored out of the asm and **Kani-checked** by bounded model checking, which handles unsafe Rust and raw pointers that Verus's model does not. So the truly-unaudited surface shrinks to the irreducible asm/volatile instructions themselves. Example split: `cmdq.rs` computes and bounds-checks the next ring slot (Kani-proven, no unsafe), then calls a two-line `external_body` that does only the volatile store + doorbell write.

### 6.4 Keeping it minimal — the discipline

- Any new `unsafe` requires a new `contracts.md` entry and an audit sign-off; there is no ambient `unsafe` anywhere else.
- Contracts are written *narrow*: a stub that could over-promise (e.g. "invalidates all TLBs") is instead scoped to exactly what the caller needs, so the audit obligation stays small and local.
- The stub has **no policy**: it never decides *what* to switch to or *which* frame to invalidate — the verified core computes all arguments and holds the big lock. The stub is pure mechanism. That is what lets a small hand-audit substitute for a proof.

---

## 7. Milestone-by-milestone: what becomes true when

| Milestone | New capability | New proof that carries weight | Isolation enforced by |
|---|---|---|---|
| **M0** | Boots as KVM guest via `start-gpu-vm.sh`; `lite::` dispatches one wave | none (bring-up) | **Host** physical IOMMU (plain VFIO) |
| **M1** | — | Nucleus core **memory-safety** (Verus, cap+mm+ipc+sched well-formedness) | Host |
| **M2** | Multiple address spaces | **Inter-AS isolation** = ASRI corollary (§2.3) | Host |
| **M3** | Emulated vIOMMU (QEMU `amd-iommu`) | **First load-bearing DRI** (§5.5) over emulated tables | **Nucleus** IOMMU proof (emulated) |
| **M4** | Bare-metal real AMD-Vi on hardware | DRI over real hardware; hardware-model assumption now against real AMD-Vi | **Nucleus** IOMMU proof (real) |
| **M5** | — | **Composed confinement assurance case**: ASRI ⊕ no-amplification ⊕ DRI ⊕ SchedContext bounds ⇒ storage+DMA confinement of the untrusted GPU stack | Nucleus (composed) |

M1 and everything after are **staffing-gated** on a proof engineer. This document is the fixed target they build against; the state representations above (bounded Cap table, tracked-permission frame view, append-only forest, reach-as-map-equality) are chosen specifically so the M2 and M3 proofs are *possible*, not merely desirable.

---

## 8. Alternatives considered

- **Fork seL4 / build on its proofs.** Rejected (decided): seL4's proofs are Isabelle over C, not reusable in a Rust+Verus setting, and its CDT/revoke machinery is exactly what the no-revocation decision lets us drop. We borrow its *patterns* (caps, badges, MCS sched-contexts, user-mode drivers) without its proof debt.
- **Guarded-CNode CSpace (seL4-faithful).** Rejected for the bounded single-level CapTable — sparse hierarchical CSpaces buy nothing for a one-driver system and are a top-three proof cost in seL4.
- **Support mid-execution revocation (Atmosphere-flat but revocable).** Rejected (decided): revocation reintroduces the mapping-database proof burden *and* would let the kernel pull memory from under verified user code, breaking the premise that makes user-side proofs composable.
- **Trust the physical IOMMU forever (never emulate).** Rejected: it would make the DMA-reach proof never load-bearing, collapsing the project's central claim. M3's emulated vIOMMU is the cheapest place to make the proof do real work before real hardware.
- **Enable device-side ATS/PRI for GPU translation caching.** Rejected: a device IOTLB adds a cache we must remotely invalidate and reason about; disabling ATS makes the nucleus-owned tables the sole authority and keeps §5.3's invalidation authoritative.
- **Fine-grained locking from day one (SMP).** Rejected for the big lock: interleaving reasoning is a large, separable verification effort with zero payoff on a single-vCPU M0–M4 target; the multikernel route preserves today's proofs if SMP is ever needed.

---

*End of `docs/nucleus-design.md`. Cross-references: capability caps and user-mode drivers after seL4; flat global memory view, manual management, and untrusted-device-behind-IOMMU after Atmosphere; MCS scheduling contexts after seL4-MCS.*

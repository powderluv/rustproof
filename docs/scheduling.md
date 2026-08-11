# Scheduling & multi-process

How Rustproof runs more than one isolated user process on one CPU, and how the
same machinery reaches preemption. Arch-generic (written against `hal::Arch`);
x86-64 lands first and RISC-V mirrors it — both arches now preempt.

## The model: trap frames + a single kernel stack + one resume path

Every kernel entry from user mode — a `syscall`, later a timer interrupt —
saves the **full** user register state into a `hal::UserFrame` and runs the
scheduler on one kernel stack. There is exactly one way back to user mode:
`Arch::resume(token, &frame)`, which loads the target's page-table root
(`cr3` / `satp`) and returns to ring 3 / U-mode via `iretq` / `sret`, restoring
that frame. The kernel stack is re-entered fresh on each trap; it never parks a
half-finished handler, so a single stack suffices.

```
user (proc A) --syscall--> [stub saves TrapFrame on kstack] --> syscall_trap::<A>(frame)
   copy frame -> PROCS[cur].frame ;  service / YIELD / EXIT ;  pick next
   Arch::resume(PROCS[next].token, &PROCS[next].frame)   // load cr3, iretq -> user (proc B)
```

`YIELD` picks the next ready process (round-robin, reusing `sched::Scheduler`)
and resumes it; value-returning syscalls (`GET_INFO`, …) service under the
current process's page tables (still active — we have not switched yet), write
`rax`, and resume the *same* process; `EXIT` removes the process and resumes the
next, or — when the run queue drains — prints `rustproof: BOOT OK` and halts.

A process is `{ token, frame: UserFrame, caps: CapSpace, role, id, state, +
frame/VRAM ownership lists }` in a fixed `PROCS: [Process; N]` static (no heap). Each process has its **own** address
space (own page-table root, kernel shared in via `share_kernel`) and its **own**
capability space, so `map_bar`/`alloc_vram` are gated per process. First entry
and later resume are identical: a fresh process just starts with a `frame`
synthesized by `Arch::frame_init(entry, sp, arg0)` (arg0 = its id, delivered in
the first-arg register), so `resume` drops it into `_start`.

The `UserFrame` is an opaque fixed-size POD (`[u64; 40]`, room for both ISAs);
each arch casts it to its concrete `TrapFrame` and reads the syscall number/args
and writes the return value through `Arch::frame_{num,arg,set_ret}`. Keeping it
concrete lets the process table be a plain non-generic static.

## Why this shape

- **One resume path, preemption-ready.** A timer ISR saves the identical
  `TrapFrame` (the CPU pushes `rip/cs/rflags/rsp/ss`; the stub pushes the GPRs
  below, matching the layout the `syscall` stub synthesizes). Preemption is then
  "run `syscall_trap`'s scheduler tail from the timer instead of a syscall" — no
  new context-switch mechanism.
- **Per-process address + capability space** is the isolation boundary the whole
  project exists to (eventually) verify: a process can only touch what its own
  page tables map and only invoke authority its own `CapSpace` holds — including
  *who it can talk to*, since IPC endpoints are capabilities (see `ipc-caps`
  below) rather than integers any process could name.

Every authority-granting op now checks BOTH halves of the capability — the object
type and `rights ⊇ need`, as `host-contract.md` specifies: `SEND` needs `WRITE`
and `RECV` needs `READ` on an `Endpoint`; `MAP_BAR` needs `READ` on an `Mmio`
(mapping exposes the device's registers); `MAKE_REGION` and `SPAWN` need `WRITE`
on an `Untyped` (both CONSUME memory, so both need the write half — but not out of
the capability: an `Untyped` names no extent, and the two draw from disjoint
pools, `MAKE_REGION` from the shared DMA arena and `SPAWN` from the general one.
See nucleus-design.md §1.2 for why that means revoking it reclaims neither).
`FREE_REGION` needs `WRITE` on the
region capability *and* ownership by process identity — a read-only loan must not
be able to destroy what it was lent.
So that the rights half is never vacuously true, `load_process` also mints two
deliberately under-powered caps (a `WRITE`-less `Untyped`, a `READ`-less `Mmio`)
that the demo uses to prove each refusal on real hardware.

The grant *policy* is per-role, not a uniform hand-out: `load_process` gives a
process exactly its role's capability table (`grants_for`), so a **producer**
holds `WRITE` on the shared endpoint and nothing else — it cannot receive on the
endpoint it sends to, map a device, allocate, or spawn — a **consumer** holds
only `READ` there, and a **worker** gets the device/memory authority but a
no-rights placeholder on the shared endpoint, so possession of a slot is not
authority. Cap ids stay aligned across roles because `insert` fills a fresh space
in order. The demo asserts each of those refusals on hardware.

Authority is never derived from a table index. Slots are recycled by `EXIT` /
`SPAWN`, so `load_process` takes the role as an explicit parameter: the boot
policy (`boot_role`) applies only to the initial, never-recycled ids, and a
`SPAWN`ed process is always a `Child` — a role whose grant table is EMPTY, so it
begins with no authority of its own and holds exactly what its parent delegated.
Deriving it
from the slot instead would let a worker spawn into the exited producer's slot
and receive `WRITE` on the shared endpoint — authority no worker holds, i.e. a
principal minting a stronger principal. Identity is likewise separate from the
slot (a monotonic counter), so a slot's current and former occupants can never be
conflated. The demo spawns late, into a deliberately recycled slot, and asserts
the child holds NOTHING of its own — no endpoint, no interrupt, no device
authority — rather than the exited occupant's grants.

Capabilities are no longer only grantable at load time: `SPAWN` takes an optional
capability of the caller's to **delegate** to the child, plus the rights to hand
over. The child receives `caller_rights ∩ requested` — `abi::CapRights::intersect`,
applied at the SPAWN site — and delegation `insert`s a fresh root: a capability
space here is FLAT, with no intra-space derivation of any kind, and the
parent/child relation lives in the cross-space `deleg` ledger, keyed by process
identity. A parent may attenuate but
never amplify, and requesting more
than it holds yields only what it holds. Asking to delegate a capability the
caller does not hold refuses the whole spawn rather than quietly producing a
child without it.

A spawned process gets `Role::Child`, whose grant table is **empty**: it begins
with no authority whatsoever, so everything it can do is exactly what its parent
delegated. That makes "spawn cannot mint authority" true by construction rather
than by an argument about who is allowed to spawn — and it is what makes the
demo's positive case meaningful: a child with an empty role allocates through a
delegated `Untyped`, which no role table could have given it. (The old negative
case — handing a child a `WRITE`-less `Untyped` with full rights requested — was
retired when the late spawn switched to delegating the device capability; the
refusal it demonstrated is now asserted directly, by the worker calling
`MAKE_REGION` through a `WRITE`-less `Untyped` and through an `Endpoint`.)

An IPC message carries a word **and** an optional byte payload (up to
`abi::MAX_MSG_BYTES`), so a message can hold a request rather than just a
token. The payload crosses address spaces through a per-process kernel buffer,
which is what makes it work at all: at `SEND` time the *sender's* space is
active and the receiver's is not, so the kernel cannot write the receiver's
buffer directly. Two paths follow from that:

- **Receiver already blocked** — copy the payload into the receiver's kernel
  buffer now, and copy it out into its user buffer in `resume_process`, after
  switching to its space and just before returning to it (the deferred path).
- **Sender blocks first** — park the payload in the *sender's* kernel buffer;
  when a receiver arrives, its space is active, so the copy out is immediate.

`MAP_BAR` installs a **real** mapping rather than reporting an address, and the
mapping carries exactly the capability's authority: a `READ`-only `Mmio`
capability produces a read-only window, so attenuating a capability (on
delegation, say) attenuates the access it grants. Revoking the capability tears
the mapping down — otherwise the authority would outlive the capability that
conferred it, which is the one thing revocation exists to prevent. And the call
is all-or-nothing: if the response cannot be delivered, the window is unmapped
again, since a caller that never learns the address cannot see or drop it. The
capability's object is the physical base it names, so a caller can only map a
window one of its own capabilities authorises — and the kernel independently
bounds the request to the device window it reserved at boot, so a mismatch
between the contract layer's idea of the window size and the kernel's can never
hand a process the ordinary RAM that happens to follow it. The page-table frames
the mapping consumes are charged to that process and reclaimed on exit like the
rest of its address space. The demo reads a signature the kernel wrote into that
physical page, then writes through the mapping and reads it back — neither would
work if `MAP_BAR` had only reported an address.

Copies on a process's behalf are **permission-checked, not just range-checked**.
`copy_to_user`/`copy_from_user` walk the active page tables and require the pages
to be mapped, user-accessible, and (for writes) writable — a range check alone
would let a ring-0 copy fault on an unmapped page (fatal: the exception handler
halts) or write straight through a page the loader mapped read-only, since x86
ring-0 stores bypass the R/W bit unless `CR0.WP` is set, which `boot.s` now also
does. `RECV` vets the buffer it offers at entry, before any rendezvous is
consumed, so the deferred copy cannot fail after a sender has been told `OK`.

A payload larger than the kernel buffer is rejected (`FAULT`) rather than
silently truncated — the sender could not otherwise learn it was cut. A payload
larger than the *receiver's* buffer IS truncated, since the sender cannot know
that size; `RECV` returns the byte count actually copied, in a third register
distinct from both the status and the word. The demo exercises both copy paths
and checks the bytes arrive intact.

Device **interrupts** are delivered as authority too. `CapType::Irq` names an
interrupt line; on each interrupt the kernel credits every process holding a
capability for that line, and `POLL_IRQ` returns and clears the caller's count
for the line ITS capability names — a capability for one line can never read or
clear another's, so a driver holding two devices can tell them apart and cannot
lose one by polling the other. A process with no such capability cannot observe
those interrupts at all, and revoking the capability drops the credits accrued
under it, the same doctrine `MAP_BAR` mappings follow. `WAIT_IRQ` is the blocking form: a process parks until its line fires, and the
hardware wakes it with the count. That needed two things beyond the poll. The
deadlock detector had to learn that a process waiting on an interrupt is *idle*,
not deadlocked — the hardware will answer it — so instead of declaring failure
the kernel parks the CPU with interrupts enabled (`Arch::idle`). And the timer
handler had to stop assuming it always preempts user code: an interrupt taken
during that park interrupted the KERNEL, so there is no user frame to save and
`CURRENT` names nobody. `idle` also resets the stack pointer to a dedicated idle
stack, because an interrupt taken while already in the kernel pushes onto the
current stack — without that reset, each tick would grow it without bound.

Blocking made a second thing load-bearing that polling did not care about:
**authority is only half of it**. A capability is permission to receive a line;
it is not a promise that anything ever fires. A process parked on a line nothing
delivers is asleep forever — and because the kernel reads "someone is waiting on
an interrupt" as *idle* rather than deadlocked, one such process parks the whole
machine, with no deadlock report and no failing exit. That is a hang an
unprivileged process can cause, so the two halves have to be tied together, not
merely documented next to each other.

`DELIVERED_IRQ_LINES` is where they are tied. It is a bitmask of the lines the
kernel actually credits, and it is consulted at three places: the grant boundary
(a role whose table hands out an undelivered line is refused at boot, and again
at load time, so a role added later cannot slip past a hand-written list),
`WAIT_IRQ` (which returns 0 rather than parking outside the mask), and the idle
path (which wakes a waiter it can never credit instead of parking for it). The
same predicate — `creditable` = delivers **and** still holds the capability —
now guards every blocking and idling decision about interrupts, because the
revocation hang and the never-delivered hang are the same bug reached two ways.
Wiring a real device line means adding it to the mask *and* crediting it from
the handler; doing one without the other fails the boot check loudly instead of
hanging the machine later.

For a long time the kernel reported parking **zero** times: some process was
always runnable, so the idle path was reachable by construction and never once
executed. Reasoning about a path is not running it — two defects had already
been found in this one by review alone — so the demo now contains the workload
that actually idles a machine. The worker delegates its interrupt line to a
**helper child**, attenuated to READ, whose entire job is to block on it. That
is the shape a driver has, and it is also the only way to reach the park: once
the other processes exit, the helper is blocked in the kernel with nothing
runnable behind it, which is precisely what `Arch::idle` is for.

The counter now reads in the low hundreds on both arches (x86 244, riscv 258 in
the runs this text was written from — it is a duration in timer ticks, not a
fixed number, so it moves). The helper doubles as the first test of *delegated*
interrupt authority: no role table grants a child an `Irq` capability, so a
child that can block on a line got that right from its parent and nowhere else.

Adding it surfaced a separate problem worth recording, because it is the same
shape as the one above. The demo's children self-replicate — each one delegates
its capability onward — so the chain grows until the process table is full, *at
any table size*. It was started early enough to take the recycled slots before
the worker's own late spawn could, and that spawn is the only thing that creates
the process exercising delegated-MMIO attenuation, revocation teardown, and the
deliberate wild write. On RISC-V it lost that race every time: four assertions —
including the only test that a ring-3 fault kills just the faulting process —
never ran, and the boot still printed `BOOT OK`, because the demo printed the
failed spawn's `u64::MAX` sentinel as if it were a pid. The chain now starts
after that spawn, and a refused spawn is a `(bug)` line rather than a number.
RISC-V had therefore never once executed its user-fault kill path; it does now.

There is now a **second** interrupt line, and it changes what the earlier claims
are worth. The kernel numbers lines itself — 0 the timer, 1 the console — and
each arch maps its own hardware onto them (x86 IRQ0/IRQ4 through the 8259,
riscv the Sstc timer and PLIC source 10), so a capability names the same thing
on both and the role tables stay arch-neutral. Until it existed, "a capability
for one line can never read or clear another's" was true only because there was
no other line; the demo now holds both as separate capabilities and checks that
the timer's count accrues while the console's stays at zero.

The console is also the kernel's only *quiet* source. The timer fires whether or
not anything happened, so a process blocked on it always wakes by itself, and
every park up to now was ended by the clock merely re-parking us. A process
blocked on the console cannot be woken by the kernel at all: the timer fires
throughout and cannot credit that line, so the machine parks until a byte
actually arrives from outside. The demo ends on exactly that wait, and the
harness supplies the byte. Withhold it and the guest stays parked and the run
times out — the success line is reachable only by a real device interrupt.

Getting the evidence right took two corrections worth recording, because both
were cases of a test proving less than it appeared to. First, *when* the byte is
sent cannot be a delay. Sent too early it is consumed by the per-line check
above, which reads and clears the console count, leaving nothing to wake the
final wait: a correct kernel then reports a false leak and hangs. Sent while
other processes are still runnable it is taken on the device handler's ordinary
path, waking nobody from a park — the run passes having never exercised the one
new path. So the harness waits for two *observations* instead: the process
announcing it is blocked, and the interrupt helper finishing. Silence alone is
not enough, because the helper spends seconds blocking and waking on the timer
while printing nothing, so a quiet log there means busy, not parked.

Second, the guest cannot testify to this. Its success line is satisfied by any
console credit, park or no park, so the kernel counts the parks a device
actually ended and prints that separately from the park count; the runner fails
the boot if it is zero. That counter immediately paid for itself: with the
timing-based harness, x86 ended a park and **riscv did not** — the byte landed
while its helper still ran — and nothing else in the run distinguished the two.

One honest note remains. The helper's loop is bounded by a tick count chosen to
outlast the rest of the demo, not by observing that it is alone — a process
cannot see the run queue, and giving it a way to would be authority it should
not have.

With that, a driver process has all four things the host contract owes it:
device registers (`MAP_BAR`), DMA memory (`MAKE_REGION`/`FREE_REGION`), a way to
talk to clients (IPC), and its device's interrupts.

Delegated capabilities can be **revoked**. `REVOKE(cap)` destroys everything
derived from one of the caller's own capabilities — transitively: the children it
was handed to and the grandchildren they passed it on to. The caller keeps its
own capability; revoking grants does not disarm you.

There is ONE mechanism, not two. The edges live in a kernel ledger (`deleg`)
keyed by process IDENTITY, and inside each holder the capability is simply freed.
This paragraph used to describe a second, intra-space half — a `parent` slot
index and a revocation fixpoint in `CapSpace` — which the kernel never built:
every write into a live capability space is an `insert`, so the fixpoint never
iterated and `revoke_subtree` freed exactly one slot while its name said
otherwise. `docs/nucleus-design.md` had already rejected that design
("Rustproof needs none of it"); the code has now caught up with it.

Three properties that mattered enough to get wrong once each:

- **Both endpoints of an edge carry an identity**, not just a slot. Slots are
  recycled, so matching a revocation source by slot alone would hand whoever next
  occupies that slot revocation authority over a third party's capabilities.
- **`EXIT` splices, it does not cut.** An edge *into* the exiting process is
  re-parented onto that process's own source before being dropped, so an
  ancestor's `REVOKE` still reaches grandchildren delegated onward. Dropping the
  edge outright would make revocation report success while silently
  under-revoking.
- **Revoking an endpoint capability ends the rendezvous it is parked in.** The IPC
  matcher keys on a blocked process's endpoint, not on present authority, so a
  process left blocked would go on sending or receiving through an endpoint it no
  longer holds; it is woken with `NO_CAP` instead — unless it still holds another
  capability naming that endpoint.

A `SPAWN` that would delegate with the ledger full is refused, since an untracked
delegation is one that could never be revoked.

Honest scope on what remains: the role table is a static boot policy; delegation
is one capability per spawn; revocation is by capability, so there is no "revoke
everything I gave process X" (loop over your caps instead); and a process exiting
does not revoke what it delegated — the children keep those capabilities until
someone with the source capability revokes them.

A CPU fault taken while USER code was running kills **that process**, not the
machine: the arch handler checks the saved privilege level (x86 `CS` RPL 3,
RISC-V `sstatus.SPP == 0`) and routes to `fault_trap`, which tears the process
down exactly as `EXIT` would — frames and VRAM reclaimed, delegation edges
spliced, slot freed — and resumes the next runnable one. A fault taken in the
KERNEL stays fatal (dump + halt), because that means the nucleus itself is
broken. Before this, any process could halt the whole guest with a wild pointer,
which is the opposite of what an isolation kernel is for. The demo has a process
dereference a null pointer on purpose and asserts the boot still completes.

Two things that classification depends on. First, only *synchronous* vectors may
be blamed on the running process: NMI, #MC and #DF are machine events (or a
failure to deliver an earlier exception) that merely arrived while a process
happened to be running, so they stay fatal. Second, the syscall entry mask must
strip every RFLAGS bit a user could weaponise — not just `IF`/`DF`/`TF` but `NT`
and `AC`: ring 3 can set `NT` with `popfq`, and in 64-bit mode a set `NT` makes
`iretq` raise #GP, so a leaked one would fault the kernel's OWN return-to-user
path, be reported with kernel `CS`, and halt the guest — the very DoS this
mechanism removes. The `init` demo sets `NT` before a syscall and asserts the
boot survives, next to the `DF` test.

A process that dies — by fault or by `EXIT` — must not strand a peer parked in a
rendezvous. Teardown wakes any process blocked on an endpoint that no surviving
process can still answer, or the run would end reporting a deadlock that is
really just a dead counterparty. And a boot in which anything was killed says so
(`BOOT OK (N process(es) killed)`), so a crash cannot read as a clean run.

Load-bearing detail: an x86 interrupt/exception gate clears `IF`/`TF` but **not**
`DF`, and `std` is unprivileged — so a ring-3 process can enter the kernel with
`DF=1`. The Rust handler's `rep movs`-lowered copies (e.g. the trap-frame save)
assume the SysV `DF=0` invariant, so every interrupt/exception stub `cld`s on
entry (the `syscall` path gets this from `FMASK` bit `0x400`). Omitting it lets
untrusted code corrupt kernel memory; the `init` demo deliberately runs its
compute loop with `DF=1` as a standing regression test.

## Milestones

| Milestone | Goal | State |
|---|---|---|
| **x86-M1** ✅ | Cooperative multi-process: `YIELD` syscall, N processes each isolated (own AS + caps), round-robin, run to `EXIT`. Generic; x86 + RISC-V. | done |
| **x86-M2** ✅ | Preemptive on x86: an 8259 PIC + 8254 PIT timer interrupt (IRQ0 → vector 0x20) drives the scheduler; a compute-bound process is time-sliced without cooperating. The timer ISR builds the identical `TrapFrame` and reuses `resume`. | done |
| **riscv-timer** ✅ | Preemptive on RISC-V too: the Sstc `stimecmp` supervisor timer (`scause` int code 5) routes to the same generic `preempt_trap`. Both arches now time-slice. | done |
| **x86-M3a** ✅ | Cross-address-space IPC: `SEND`/`RECV` synchronous 1-word rendezvous with process blocking (`ProcState` + run-queue add/remove), deadlock detection. Generic; x86 + RISC-V. | done |
| **x86-M3b** ✅ | A real `SPAWN` syscall (Untyped-cap-gated): load the embedded image into a fresh process at runtime, with full frame reclamation on `EXIT` (a spawn/exit cycle leaks no address space). Generic; x86 + RISC-V. | done |
| **region-quota** ✅ | DMA memory IS a region — `ALLOC_VRAM`/`FREE_VRAM` were deleted as a strictly weaker duplicate of `MAKE_REGION` (they returned a raw host physical address into a bare list, with no id, no capability, no delegation and no revocation). A per-owner quota (`abi::REGION_QUOTA`) replaces the per-process frame quota, checked before a frame is taken or an id burned; the demo asserts the exact headroom, since `NO_MEM` alone cannot distinguish a quota refusal from a full table. Generic; x86 + RISC-V. | done |
| **ipc-caps** ✅ | IPC endpoints are capabilities, not raw integers: `SEND`/`RECV` take a `CapId`, require `CapType::Endpoint` with `WRITE`/`READ` respectively, and rendezvous on the cap's *object* — so two processes meet only when their caps name the same endpoint, and an unauthorized caller gets `NO_CAP` without blocking. Generic; x86 + RISC-V. | done |
| **role-caps** ✅ | Per-role grants: each process is loaded with only its role's capability table (producer = send-only, consumer = receive-only, worker = device/memory but no shared-endpoint authority), so the policy is least-authority rather than uniform. Generic; x86 + RISC-V. | done |
| **cap-delegation** ✅ | `SPAWN` can hand the child one of the caller's own capabilities, attenuated: the child gets `caller_rights ∩ requested`, so a parent may narrow but never widen authority, and delegating a cap it does not hold refuses the spawn. Generic; x86 + RISC-V. | done |
| **cap-revocation** ✅ | `REVOKE(cap)` destroys every capability derived from one of the caller's own, transitively across spaces via the `deleg` ledger (identity-keyed edges, exhaustively tested); the caller keeps its own. Capability spaces are flat — there is no intra-space derivation tree. Generic; x86 + RISC-V. | done |
| **ipc-payload** ✅ | IPC messages carry a byte payload across address spaces (per-process kernel buffer; deferred copy-out when the receiver blocked first), with the copied length returned in a third register. Generic; x86 + RISC-V. | done |
| **irq-delivery** ✅ | Device interrupts delivered to user processes as capability-gated authority: `CapType::Irq` + `POLL_IRQ`, counted per line, invisible without the capability, dropped on revocation. Generic; x86 + RISC-V. | done |
| **fault-isolation** ✅ | A user-mode fault kills the faulting process (frames reclaimed, ledger spliced, slot freed) and the scheduler carries on; kernel faults stay fatal. Generic; x86 + RISC-V. | done |
| **real-mapbar** ✅ | `MAP_BAR` installs a REAL page-table mapping of the window its `Mmio` capability names, instead of reporting a placeholder address: a holder can read and write that physical memory, a non-holder cannot name it, and the kernel bounds every request to the window it reserved. Generic; x86 + RISC-V. | done |

## Alternatives Considered

- **Per-process kernel stacks + a kernel↔kernel context switch (`sched::switch`)
  for `YIELD`.** Each process parks its syscall handler mid-stack and a
  callee-saved context switch resumes another's. Rejected as the foundation: it
  reuses the existing `switch` with less new code for the *cooperative* case, but
  it does **not** generalize to preemption (an interrupt cannot cooperatively
  unwind to a park point), so timer preemption would need the trap-frame model
  anyway. Building it once, here, avoids a throwaway.
- **A generic `enter_user` per syscall (the pre-M1 single-process path) plus an
  ad-hoc second process.** Rejected: no uniform save/restore, so no path to
  preemption and no clean place to store per-process state.
- **Keeping multi-process x86-only, leaving RISC-V on the single-process path.**
  Rejected: it would fork `kernel::run` back into per-arch tails right after the
  HAL unified them. RISC-V already assembles a full trap frame, so mirroring the
  `UserFrame` surface is cheap and keeps one generic scheduler.
- **Heap-allocated process table / green threads.** Rejected: no allocator in the
  nucleus by design; a fixed `[Process; N]` static is enough and keeps the state
  auditable.


## Shared memory: regions

A driver operates on its clients' buffers, so processes have to be able to share
memory without copying. `CapType::Region` is that capability, and it is the first
one in this kernel whose object is owned by a **process** rather than by the
kernel. Every earlier object — an endpoint number, the boot-reserved device
window, the DMA pool, an interrupt line — outlived every process, which is why
nothing ever revalidated a capability's object after `SPAWN` copied it verbatim.
A region can be destroyed while a capability still names it.

So the object is a **monotonic region id**, resolved in a kernel table, never a
physical address and never a table index, and never reused. That single choice is
the safety argument: a capability that outlives its region resolves to nothing,
where an address or an index would resolve to whatever occupies that address or
slot next. Region frames live in the region and in no process list — `teardown`
frees a process's `frames` and `vram` unconditionally, with no refcount anywhere,
so a frame in both places would be freed under a borrower still mapped to it.

Sharing needed no new transfer mechanism. `SPAWN` delegation already attenuates
rights, records a ledger edge and revokes transitively, so a region capability
rides it unchanged; revocation then unmaps the window, the same doctrine device
mappings already followed. The borrower pulls the mapping into its own space at
an address the **kernel** picks, so no user-supplied address ever reaches the
mapping path and none has to be validated.

### What the review changed

The kernel came out of review nearly intact; the *tests* did not. Neither run
script grepped for the `(bug)` lines the demo prints on a failed assertion, so
all 47 assertions per demo were comments rather than gates — demonstrated by
making `MAP_REGION` ignore capability rights, a real amplification, and watching
it pass green. That check exists now, and the region assertions were rewritten
until breaking the kernel actually breaks the run:

| break the kernel like this | the run now |
|---|---|
| `MAP_REGION` ignores capability rights | FAIL — a READ-only loan gave a WRITABLE window |
| revocation stops unmapping the region | FAIL — kept the window after revocation |
| `teardown` stops destroying owned regions | FAIL — `[mm] LEAK: 128989 -> 128985` |
| region frames are not scrubbed on destroy | still passes — see below |

Two lessons stuck. A test must not choose *which* assertions to run by probing
the property under test: the borrower originally decided whether it held a
read-only or a writable loan by trying to write, so a kernel that ignored rights
simply ran the other branch and reported nothing. It now discriminates on region
size, which the property cannot influence. And fresh memory proves nothing about
zeroing, because early-boot frames are zero anyway — the assertion now poisons a
region, destroys it, and requires the *next* region to come back clean.

That last row is fixed now, and how it got fixed is the more useful part.
Scrubbing happened at three sites — region creation, region destruction, and
stack mapping — so removing any one was masked by the others and no test could
tell whether the kernel scrubbed at all. It now happens in one function,
`zero_frame`, applied where a frame leaves the pool toward a process.

One *function*, though, is not one *call site*, and review caught the difference.
Deleting the whole body does fail the run, but deleting the single call in
`RecordingAlloc::alloc_frame` — the one behind every process's stack — left both
arches green while spawned processes read whole pages of an exited process's
stack at ring 3 with no capability at all. That was the original disclosure,
guarded by a line no assertion could reach. Every process now checks, before
anything else, that the untouched pages below its own stack pointer are zero, and
then paints them so a future leak is attributable rather than merely nonzero.
Both mutations fail now, on both arches — which took adding the probe to *both*
user programs, since x86 boots `crates/init` and riscv boots `crates/riscv-init`
and a probe in one says nothing about the other.

Scrubbing on *release* would read as the more natural choice and is not enough on
its own: a frame that has never been allocated still holds whatever the firmware
left in it. Zeroing on the way out covers both. Region allocation goes through it too,
and there it is not hypothetical: `MAP_REGION` installs a region's frames in the
caller's own address space, and the demo's borrower reads every page of a
delegated region at ring 3 today. Pointing a DEVICE at those frames, once a
driver exists, is the same disclosure with one more step.


## Time, and why the kernel has none

A driver needs to bound a wait: *ask the device, give up if it never answers.* The obvious
shapes are a deadline argument on `WAIT_IRQ` or a `GET_TIME` syscall. Neither was built, and
the reason is worth recording, because "add a timeout" is the kind of thing that looks
obviously right.

The kernel already ends a wait exactly when the wait has become **unanswerable** — the
capability was revoked, or the line is one it does not deliver — and those are facts only the
kernel knows. "My device is slow" is not one of them. How slow is too slow is the driver's
judgement, and a kernel deadline would be the kernel guessing on its behalf.

And a bounded wait already composes from primitives that exist: poll the device line, block one
tick on the timer line, repeat until a bound. `POLL_IRQ` on a timer capability returns elapsed
ticks, and credits accrue even while the process is blocked on a *different* line. So the gap
was never the mechanism — it was that **nothing in the tree performed the composition**, which
by this project's own standard makes "a process can bound its wait" asserted rather than
implemented. Both demos now do it, and the assertion takes three facts together, because any
one of them alone passes for the wrong reason: it ended on its bound and not on a credit
(`hit == 0`), it genuinely blocked (`ticks >= bound` — a process cannot manufacture timer
credits by spinning), and the quiet line is still untouched (so a byte did not race in and
answer the wait).

**Revisit when** a Linux-personality server needs `clock_gettime`/`nanosleep`, or at the first
real driver bring-up, or if that composition ever surfaces a kernel bug — that last one would
be evidence *for* the deadline. A microsecond-denominated ABI would also need a better timer
than the PIT.

### Observing time is not authority here

Reading elapsed time is **denied to ring 3 by default**, and the two arches required different
work to say so — one of them cannot fully say it at all.

x86 now sets `CR4.TSD`, so `rdtsc` traps. Before this it did not: every process, including the
least-authority producer holding one send-only endpoint capability, had a free-running
nanosecond clock that no capability gated.

RISC-V clears `scounteren`, which denies `rdcycle` and `rdinstret` — but **not** `rdtime`, and
that was measured rather than assumed. A differential probe settled it: `rdcycle` traps while
`rdtime` returns, so `scounteren` is being honoured and `rdtime` is emulated *above us*. The
illegal-instruction trap is not delegated to S-mode; it lands in M-mode firmware (OpenSBI),
which services the read and returns to U-mode without this kernel ever seeing it. Only
`mcounteren` could deny it, and we do not run in M-mode.

Both demos assert this the only way a denial like it can be asserted: a process reads the
counter as its final act and must be **killed** for it. A process that is allowed to read it
simply lives, so there is no failure line to print — the runners grep for the kill instead, and
removing either denial fails the run.

Consuming CPU time is a separate question and stays open: it is what an MCS `SchedContext`
would make capability-controlled.

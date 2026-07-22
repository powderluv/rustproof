# docs/milestone-M0.md — Rustproof M0: boot the nucleus as a KVM guest and dispatch one gfx1201 wave through untrusted `lite::`

> **Status:** engineering task breakdown, 2026-07-21. Derives directly from the decision doc `plans/verified-gpu-host-os.md` §5-M0 and research brief `plans/verified-gpu-host-os-research-brief.md` §8. Concrete file/flag anchors are from `plans/cpp-windows-hip-port.md`, `gist-tri-os/start-gpu-vm.sh`, `gist-tri-os/README.md`, `multi_dispatch_test.cpp`, `run-multi-dispatch-test.sh`.

## 0. What M0 is, and what it is *not*

**M0 goal.** A minimal Rustproof nucleus boots as a **KVM guest on shark-a** (handed the gfx1201 via `gist-tri-os/start-gpu-vm.sh`, no-FLR), hosts the existing C++ `lite::` gfx1201 stack as an **untrusted user process**, and that process dispatches **one real compute wave** — reusing `multi_dispatch_test.cpp` / `run-multi-dispatch-test.sh` (`inc` kernel; the result value is verified in VRAM, `x[i] == N`).

**M0 verifies nothing.** There is **no machine-checked property at M0**. It is a physical-feasibility gate for the untrusted-driver architecture. Per the decision doc's staging table (§3) and rows A7/A8 of the V/A/U table (§6), device isolation at M0 is enforced entirely by the **host Linux / KVM / VFIO** stack — the host programs the physical AMD-Vi and pins guest RAM to the device. Any address-space or capability machinery the nucleus grows here is exercised, not proven. The first Verus property is M1; the first *load-bearing* DMA-reach proof is M3. Say this in every M0 status report.

**Budget honestly.** This is easy to under-scope. The decision doc R6 puts M0 at **~6–12 months for one strong systems engineer**, not 3. The dominant costs are not the microkernel — they are (a) hosting a large, multithreaded C++ ROCr/HIP stack with **no POSIX**, and (b) reproducing a fragile per-power-cycle gfx1201 bring-up (PSP wedges after ~1 attempt) inside a from-scratch guest. The effort table in §4 sums to ~8–16 engineer-weeks-of-uncertainty on those two items alone.

### 0.1 Reuse vs. new work (the load-bearing distinction)

| Component | M0 disposition |
|---|---|
| `.../core/driver/lite/amd_lite_direct_queue.cpp` (+ `.h`) — shared MES/direct-MEC queue core | **Reuse unchanged.** It only calls the `DirectQueuePlatform` virtuals. |
| `.../core/runtime/amd_lite_aql_queue.cpp` (`LiteAqlQueue`, AQL→PM4) | **Reuse unchanged** (this is the Linux AQL queue). |
| `.../core/driver/lite/linux/{amd_lite_linux_driver.cpp, amd_lite_linux_transport.cpp, amdgpu_lite_uapi.h}` — `LinuxAmdgpuLiteDriver`, `DriverType::LINUX_AMDGPU_LITE` | **Reuse unchanged *if* the guest presents an `amdgpu_lite` ioctl ABI** (recommended, T0.4). Its transport does `ioctl()`/`mmap()` on `/dev/amdgpu_lite0`; we satisfy that ABI from the nucleus instead of adding a new `DriverType`. |
| ROCr (`libhsa-runtime64`), HIP/clr (`libamdhip64`), comgr **loader** path | **Reuse as prebuilt Linux binaries** under the personality shim (T0.5), or statically relink. Runtime *compilation* (HIPRTC/comgr/LLVM) is **not** needed — the kernel is AOT-compiled by `hipcc --offload-arch=gfx1201`. |
| Python bring-up `userspace_driver/python/amd_gpu_driver/backends/amdgpu_lite/bringup.py` (+ `windows/psp_init.py`, `windows/ring_init.py`) — `LITE_MES_RECIPE` | **Reuse on the host** for the fragile firmware-load stages; the RAM-ring stages must run in-guest (T0.8) — **port to C++/Rust or run under a CPython personality (a real fork, see T0.8)**. |
| `multi_dispatch_test.cpp` / `run-multi-dispatch-test.sh` | **Reuse as the M0 workload** (adapt the runner for the guest; the `.cpp` is unchanged). |
| `gist-tri-os/start-gpu-vm.sh` (VFIO bind, `reset_method` cleared, no-FLR) | **Reuse as-is** for host-side handover. |
| `amdgpu_lite.ko` (`userspace_driver/amdgpu_lite/{main,pci_setup,memory,irq}.c`) | **Not run in the guest.** Its *services* (BAR mmap, VRAM/GTT alloc, GPU-PT program, MSI-X→eventfd) are **absorbed into the nucleus + host-contract server** (T0.4). Read it as the spec for what those services must do. |
| **The nucleus, the POSIX/Linux personality, the host-contract server, firmware provisioning, in-guest MES/IH bring-up** | **All new work.** |

---

## 1. Architecture of the M0 guest (concrete)

Under VFIO passthrough the guest sees gfx1201 (`1002:7551`) as an ordinary PCIe device on the guest's PCIe bus. So inside the guest the nucleus does **not** touch AMD-Vi (that is M3); it does PCIe enumeration + BAR mapping + guest-physical DMA + (optionally) MSI-X. Guest-physical→host-physical is the host VFIO/IOMMU's job.

```
 shark-a host (Linux, TRUSTED at M0):
   start-gpu-vm.sh  ->  vfio-pci bind, reset_method="" (no FLR), virsh start
   (optionally) LITE_MES_RECIPE bring-up to BOOTLOAD_COMPLETE on the raw card first
        │  no-FLR handover: freshly-POSTed card enters the guest
        ▼
 KVM guest = Rustproof nucleus (NEW):
   ┌───────────────────────────────────────────────────────────────┐
   │ UNTRUSTED driver process (Linux ABI):                          │
   │   multi_dispatch_test  →  HIP/clr  →  ROCr lite::              │
   │     LiteAqlQueue (AQL→PM4) → amd_lite_direct_queue.cpp         │
   │       → LinuxAmdgpuLiteDriver → ioctl()/mmap() on              │
   │         /dev/amdgpu_lite0  (satisfied by the nucleus)          │
   │   + in-guest MES/IH bring-up (ported from bringup.py)          │
   ├───────────────────────────────────────────────────────────────┤
   │ Linux-personality shim  (mmap/ioctl/futex/threads/file/…)      │
   ├───────────────────────────────────────────────────────────────┤
   │ host-contract server  = amdgpu_lite ioctl surface over        │
   │   PCIe/BAR/DMA/MSI primitives  (absorbs amdgpu_lite.ko)        │
   ├───────────────────────────────────────────────────────────────┤
   │ ★ nucleus: PVH boot, GDT/IDT, paging, serial, timer,          │
   │   frame allocator, AS/threads, tiny unsafe MMIO/DMA stub       │
   └───────────────────────────────────────────────────────────────┘
   gfx1201 (VFIO passthrough) — isolated by the HOST IOMMU at M0
```

**Two M0 simplifications worth taking early:**

1. **Poll, don't interrupt.** The macOS `lite::` path has no working IRQ and polls the EOP/completion fence in memory; ROCr honors `HSA_ENABLE_INTERRUPT=0` to poll a signal in host-visible memory. So **real MSI-X delivery is not on the M0 critical path** — defer it to T0.10. `multi_dispatch_test` uses `hipDeviceSynchronize()`, which becomes a memory poll.
2. **Doorbell is dead on passthrough → MMIO-WPTR poke.** As on Windows/macOS (`ROCR_WINDOWS_MES_MMIO_WPTR`, `ROCR_MACOS_DIRECT_QUEUE_MMIO_WPTR`), the "ring doorbell" op is really an MMIO write to `CP_HQD_PQ_WPTR_*`. `RING_DOORBELL` in the host contract is a trusted MMIO-write stub, not a doorbell-aperture write.

---

## 2. Ordered task breakdown

Ordering is the build order. Hard dependencies are noted; several tasks parallelize once T0.1–T0.3 exist.

---

### T0.0 — Reproduce the known-good baseline on shark-a (host, no nucleus)
- **Goal.** Establish the reference the M0 port is measured against: the exact same `lite::` dispatch works on host Linux today. (Decision-doc action #5.)
- **Steps.**
  1. Cold power-cycle shark-a (BMC) for a fresh POST.
  2. `insmod amdgpu_lite.ko`; decompress firmware (`psp_14_0_3_*`, `gc_12_0_1_*`, `sdma_7_0_*` `.bin.zst` → `/lib/firmware/amdgpu/`).
  3. Run `LITE_MES_RECIPE=1` bring-up to `BOOTLOAD_COMPLETE` (`RLC_RLCS_BOOTLOAD_STATUS == 0x8000003f`) + MES start; then `run-multi-dispatch-test.sh` (or its shark-a analog) → one wave + VRAM verify.
- **Files/tools.** `userspace_driver/amdgpu_lite/`, `.../backends/amdgpu_lite/bringup.py`, `.../backends/windows/{psp_init,ring_init}.py`, `multi_dispatch_test.cpp`.
- **Acceptance.** Captured log: `BOOTLOAD_STATUS=0x8000003f` → MES/direct-MEC → `SURVIVED N dispatches; verify=PASS`. This log is the M0 target signature.
- **Effort.** 0.5–1 wk. **Risk.** Low (already works per README), except PSP-wedge flakiness → the retry loop in `run-multi-dispatch-test.sh` + BMC cycle. **Deps.** none.

---

### T0.1 — Minimal bootable nucleus as a KVM guest
- **Goal.** The nucleus boots under QEMU/KVM and prints to serial. Core plumbing: boot, GDT/IDT, paging, serial, timer, physical-frame allocator, kernel heap.
- **Steps.**
  1. **Boot path: PVH direct-kernel.** Build `x86_64-unknown-none`, emit the PVH ELF note (`XEN_ELFNOTE_PHYS32_ENTRY`) so libvirt/QEMU `-kernel` boots straight into the nucleus in 32-bit protected mode; switch to long mode in early asm. This avoids writing a BIOS/UEFI bootloader (cleaner than the `bootloader` crate's disk-image path for a passthrough guest).
  2. **GDT/IDT.** `x86_64` crate: `GlobalDescriptorTable`, `InterruptDescriptorTable`, TSS with an IST for double-fault. Install exception handlers (page-fault, GP, DF) that dump to serial.
  3. **Paging.** 4-level page tables via `x86_64::structures::paging`; identity-map low memory + a higher-half kernel; a `Mapper` over the PVH-provided memory map (e820/HVM `memmap`).
  4. **Serial.** `uart_16550` on `0x3F8` for all early logging (QEMU `-serial mon:stdio`).
  5. **Timer.** LAPIC timer (calibrate against PIT/PM-timer) or TSC-deadline; a monotonic tick for `nanosleep`/`SleepUs` and the bring-up's settle delays.
  6. **Frame allocator.** Bitmap/free-list over the PVH memory map; separate a **DMA-capable, contiguous** pool for GTT rings (T0.4). Kernel heap via `linked_list_allocator`.
- **Files/tools.** New `nucleus/` crate. Crates: `x86_64`, `uart_16550`, `linked_list_allocator`, `spin`, `bitflags`, `volatile`. QEMU direct `-kernel`.
- **Acceptance.** `virsh start` (or bare `qemu-system-x86_64 -enable-kvm -kernel nucleus.elf`) → serial shows GDT/IDT/paging up, heap alloc works, timer ticks; a forced page-fault prints a clean handler dump.
- **Effort.** 3–5 wk. **Risk.** Med (PVH long-mode trampoline + memory-map parsing are fiddly but well-trodden). **Deps.** none.

---

### T0.2 — Address spaces, threads, context switch, minimal capability table
- **Goal.** Enough kernel structure to run isolated user processes and to grant them BAR/DMA regions. Full capability derivation/revocation is M1/M2; M0 needs only a working handle table.
- **Steps.**
  1. **Address-space object.** Per-AS page-table root; `map(frame, va, perms)` / `unmap`. This is the object M2's non-interference proof will later be about — build it with that in mind (single owner of PT memory).
  2. **Threads + scheduler.** Kernel/user thread objects; round-robin (or cooperative) scheduler; **the context switch is the first entry in the trusted `unsafe` stub** (asm save/restore) — inventory it now for M1.
  3. **Handle/capability table.** Per-process table of typed handles: `MmioCap{bar, off, len}`, `DmaCap{frames, perms}`, `EventCap`. M0 semantics can be coarse (grant-only, no revoke) — the point is the ioctl→handle mapping exists.
  4. **Syscall/IPC entry.** `syscall`/`sysret` MSRs; a tiny synchronous IPC (call/reply) for the host-contract and personality servers.
- **Files/tools.** `nucleus/{addrspace,sched,cap,ipc}.rs`; `x86_64` MSR/segment support.
- **Acceptance.** Two kernel threads round-robin; a user AS is created, a frame mapped, and an intentional cross-AS access faults.
- **Effort.** 3–5 wk. **Risk.** Med (ctx-switch asm, TLB shootdown even if UP-only). **Deps.** T0.1.

---

### T0.3 — Userland loader + first user process
- **Goal.** Load and run a static ELF at ring 3 that calls back into the nucleus.
- **Steps.**
  1. **Static ELF64 loader** (`goblin` or `object` crate, or hand-rolled `PT_LOAD` walk): map segments with correct perms into a fresh AS, set up user stack, enter ring 3.
  2. **Syscall dispatch** for a handful of nucleus calls (write-serial, map-cap, exit).
  3. Run a trivial hand-written static user binary (`-nostdlib`) that prints via syscall and exits.
- **Files/tools.** `nucleus/loader.rs`; `object`/`goblin`.
- **Acceptance.** User binary prints and exits cleanly; a bad syscall arg is rejected, not a nucleus fault.
- **Effort.** 2–3 wk. **Risk.** Low-Med. **Deps.** T0.2.

---

### T0.4 — Host-contract server: the `amdgpu_lite` ioctl surface over PCIe/BAR/DMA/MSI
- **Goal.** Provide, from the nucleus, exactly the privileged services `amdgpu_lite.ko` provides, so the **unmodified** `LinuxAmdgpuLiteDriver` transport works. This is the frozen M0 spec surface for M1–M3.
- **Steps.**
  1. **PCIe enumeration.** Guest ECAM/MMCONFIG (or `0xCF8/0xCFC`) walk; find `1002:7551`; read BAR sizes/addresses, enable bus-master + memory decode. → `IOC_GET_INFO`.
  2. **BAR mapping.** Map MMIO/doorbell/VRAM BAR windows **uncached** (`pgprot_noncached` equivalent — mirror `pci_setup.c`) into the driver AS, gated by an `MmioCap`. → `IOC_MAP_BAR`.
  3. **VRAM alloc.** FB bump allocator over the VRAM BAR aperture (the Windows note's "FB-MC bump allocator"); return `(fb_offset, cpu_window)`. → `IOC_ALLOC_VRAM`/`IOC_FREE_VRAM`.
  4. **GTT alloc.** Pinned, contiguous, DMA-capable guest-physical pages from the T0.1 DMA pool; GPU-visible (at M0 trivially, since host VFIO maps all guest RAM). → `IOC_ALLOC_GTT`.
  5. **GPU page tables (`IOC_MAP_GPU`).** GPUVM is **UNTRUSTED** in Rustproof (it yields only IOVAs). Simplest M0: a thin pass-through — the driver programs GPUVM through MMIO it already holds via `MAP_BAR`; the nucleus need not mediate it. (Contrast the Linux `.ko`, which made this kernel-side "for security" — that guarantee moves to the M3 IOMMU proof, not here.)
  6. **`IOC_RING_DOORBELL` = MMIO-WPTR poke.** Trusted MMIO-write stub to `CP_HQD_PQ_WPTR_*` (doorbell dead on passthrough).
  7. **`IOC_SETUP_IRQ`.** Stub at M0 (return a pollable `EventCap`); real MSI-X is T0.10.
  8. **ABI shim.** Present a synthetic `/dev/amdgpu_lite0` fd whose `ioctl()`/`mmap()` (via T0.5) route to this server, so `amd_lite_linux_transport.cpp` is reused verbatim.
- **Files/tools.** New `hostcontract/` server; spec = `plans/cpp-windows-hip-port.md §1L.1` + `amdgpu_lite/{pci_setup,memory,irq}.c`; consumer = `amd_lite_linux_transport.cpp`, `amdgpu_lite_uapi.h`.
- **Acceptance.** From a user process: open `/dev/amdgpu_lite0`, `GET_INFO` returns `1002:7551` + BAR sizes; `mmap` the MMIO BAR and read a known GC register; `ALLOC_GTT` a page, write via CPU, read back.
- **Effort.** 4–8 wk. **Risk.** **High** — BAR/DMA/ECAM correctness in a from-scratch guest; confirm shark-a's **ReBAR/BAR-size** posture for x86 passthrough (decision-doc R4 open item — differs from the Mac 256 MB path). M0 workload is tiny, so even a small VRAM window suffices. **Deps.** T0.2 (caps), T0.3 (user process). **Freeze the ioctl→capability mapping here** as the M1–M3 spec surface.

---

### T0.5 — The hard part: hosting the C++ `lite::` stack with **no POSIX**
- **Goal.** Run the large, multithreaded C++ ROCr/HIP/`lite::` stack (and the test) with no Linux kernel under it. This is the single biggest M0 item and the one most often under-budgeted (R6).
- **Decision (recommended): a Linux-syscall *personality*, not a fresh libc.** Three options:
  - **(a) relibc-derived shim (Redox libc).** Rust-native, Redox synergy — but **relibc's C++/libstdc++ support is one of Redox's weakest areas**, and ROCr/HIP is heavy C++ (std::thread, exceptions, RTTI, iostreams). High porting risk.
  - **(b) musl-static + Linux-syscall emulation (recommended).** Link the stack against static **musl + libstdc++/libc++** (the well-trodden Alpine path), and implement the *bounded subset of the x86_64 Linux syscall ABI* those binaries actually issue, backed by nucleus services. The driver ELF is then **the same binary that runs on shark-a today**. Trap `syscall` in the guest; the nucleus reflects unknown numbers to a userland **"Linux personality" server** (keeps the nucleus small and out of the future TCB).
  - **(c) bespoke libc.** Most control, most work — rejected for M0.
- **Concrete syscall/service surface the stack needs** (enumerate empirically with `strace` on shark-a during T0.0, then implement exactly this set):
  - **Memory:** `mmap`/`munmap`/`mprotect`/`brk` (heap + BAR/DMA mappings) → nucleus map-cap.
  - **Device:** `openat`/`close`/`ioctl` on `/dev/amdgpu_lite0` → T0.4 host contract.
  - **Firmware file reads:** `openat`/`read`/`pread`/`fstat`/`close` on `/lib/firmware/amdgpu/*.bin` → T0.6 RO file service.
  - **Threads/sync:** `clone`(thread)/`futex`/`set_robust_list`/`sched_yield`/`exit` → nucleus threads + a futex over an in-AS word. **ROCr/HIP spawn completion + signal-handler threads — this is the make-or-break piece.**
  - **Time:** `clock_gettime`/`nanosleep` → T0.1 timer.
  - **Env/query:** `getenv` table (many `ROCR_*`/`HSA_*`); `sysconf`/`getpagesize`; minimal `/proc/self/*` if unavoidable.
  - **Completion:** `ppoll`/`read` on the event fd — **avoidable at M0** with `HSA_ENABLE_INTERRUPT=0` (poll a memory fence).
  - **`dlopen`/`dlsym`:** resolve statically. Prefer **fully static linking** of ROCr+HIP+clr into the test binary; stub `dlopen` to a static symbol registry. Runtime compile (HIPRTC/comgr/LLVM) not loaded (AOT kernel).
  - **Signals:** minimal `rt_sigaction` (ROCr's SIGSEGV memory-fault handler) — a stub that logs is acceptable at M0.
- **Files/tools.** New `personality/` (Rust) + musl/libstdc++ static toolchain; `strace` capture from T0.0 as the authoritative syscall list.
- **Acceptance.** A C++ test binary using `std::thread` + `std::mutex` + `std::vector` + iostreams + `dlopen`(static) runs to completion in the guest. Then `hsa_init()` links and returns (T0.7 gate).
- **Effort.** **8–16 wk** (dominant M0 cost). **Risk.** **Highest** — thread/futex fidelity under a large C++ runtime; static libstdc++ corner cases. **Deps.** T0.2, T0.3, T0.4.

---

### T0.6 — Firmware-blob provisioning
- **Goal.** Make `/lib/firmware/amdgpu/*.bin` readable in the guest.
- **Steps.**
  1. Build a small read-only RAM image (cpio/tar) containing exactly the gfx1201 blobs the recipe reads: **`psp_14_0_3_*`, `gc_12_0_1_*` (incl. `gc_*_uni_mes.bin`), `sdma_7_0_*`** — decompressed from `.bin.zst`.
  2. Load it via QEMU `-initrd` (or embed in the nucleus image); a tiny nucleus RO file service answers T0.5's `openat`/`read` at `/lib/firmware/amdgpu`.
- **Files/tools.** cpio; the blob list from `gist-tri-os/README.md` §"Firmware staging"; recipe reads with `--fw-dir /lib/firmware/amdgpu`.
- **Acceptance.** In-guest, the driver `open`/`read`s each required blob and byte-length matches the host copy.
- **Effort.** 1–2 wk. **Risk.** Low. **Deps.** T0.5 (file syscalls).

---

### T0.7 — Bring the C++ ROCr/HIP/`lite::` stack up in the guest (attach, no dispatch yet)
- **Goal.** `hsa_init()` + agent enumeration succeed against the passed-through, already-POSTed gfx1201; HIP sees the device.
- **Steps.**
  1. Statically link `libhsa-runtime64`(`lite::`) + `libamdhip64`(clr) + comgr-loader; wire `LinuxAmdgpuLiteDriver` to T0.4 via the `/dev/amdgpu_lite0` ABI.
  2. Bring up ROCr topology from **IP discovery read over the mmap'd BAR** (not `/sys` — the `lite::` path parses IP discovery itself), so no sysfs dependency.
  3. `hipGetDeviceCount()` / `hipMalloc` / `hipMemset` against VRAM/GTT from T0.4.
- **Files/tools.** `amd_lite_direct_queue.cpp`, `amd_lite_aql_queue.cpp`, `amd_lite_linux_driver.cpp`; `multi_dispatch_test.cpp` lines 36–38 (`hipMalloc`/`hipMemset`/`hipDeviceSynchronize`).
- **Acceptance.** Serial log: `hsa_init` OK, one agent = gfx1201; `hipMalloc(256 floats)` + `hipMemset` + sync succeed (no dispatch yet).
- **Effort.** 4–8 wk. **Risk.** High (linker/loader + first real hardware attach under the shim). **Deps.** T0.4, T0.5, T0.6.

---

### T0.8 — GPU bring-up: PSP → SMU → GFX → MEC → MES → (IH), partitioned host/guest
- **Goal.** Get the card from "freshly POSTed" to "MES engine alive, compute queue mappable" — the fragile core.
- **Recommended partition.**
  - **Host does the fragile firmware load** to `BOOTLOAD_COMPLETE` on known-good Linux (the "PSP wedges after ~1 bring-up" part), state preserved into the guest by the no-FLR handover in `start-gpu-vm.sh`. Reuses the proven **4 recipe fixes**: (1) TOC parsed from the **SOS container** (`PSP_TOC=4`, 2304 B), (2) `use_cmd_buffer=True` (1024-byte LOAD_IP_FW ABI), (3) RS64 ucode offset from the **v2 gfx-header +40** (not common-header +24), (4) **RLC_G loaded last → AUTOLOAD_RLC → SMU mailbox (`SetDriverDramAddr*`, `EnableAllSmuFeatures(0)`) → poll bootload bit31**.
  - **Guest must (re)do the system-RAM-ring stages** — **IH ring init + MES engine start + KIQ/scheduler** — because those rings live in system memory that does *not* survive the handover (on-card SRAM/register state does). This is the crux uncertainty.
- **The real fork (decide early).** The post-bootload stages are today **Python** (`_recipe_mes_start`, `init_mes_for_compute`, IH init in `.../backends/amdgpu_lite/` + `.../backends/windows/ring_init.py`). To run them in the guest:
  - **(i) Port them to C++/Rust** into the in-guest driver (aligns with the `cpp-windows-hip-port.md` C++-port plan; keeps CPython out of the guest — **recommended**), or
  - **(ii) Run the existing Python under a CPython personality** (larger syscall/file surface; heavier — a fallback).
- **Files/tools.** `bringup.py::_recipe_bringup/_poll_bootload_complete/_recipe_mes_start`; `windows/ring_init.py::{init_gfx_for_compute,init_compute_queue,init_mes_for_compute}`; `try_phase9_doorbell.py` (macOS analog for the MES-start shape); `LITE_STOP_AFTER={toc,autoload,smu,bootload,mes,queue}`.
- **Acceptance.** In-guest serial: `RLC_RLCS_BOOTLOAD_STATUS == 0x8000003f` (carried from host), then `MES: KIQ and scheduler rings initialized` established from **guest** GTT.
- **Effort.** 4–10 wk. **Risk.** **High/uncertain** — the Python→native port (or CPython personality) plus proving MES-start actually re-establishes from guest RAM against a host-loaded firmware image. **This is the top schedule risk after T0.5.** **Deps.** T0.7; host T0.0.

---

### T0.9 — Single AQL dispatch + VRAM verify (the M0 payoff)
- **Goal.** One real compute wave completes and the VRAM result is correct.
- **Steps.**
  1. Create an **MES-backed** compute queue in guest GTT via `MapLegacyQueueWithMes` (`use_mes_queue=true`) — the path that lifted the ~13–15-dispatch ceiling.
  2. Run `multi_dispatch_test` with small `N` (start `N=1`): `LiteAqlQueue` translates the `inc` AQL packet → PM4 into the ring (with the TYPE-3 NOP ring-wrap guard), bumps the VRAM wptr, `FlushHdp`, **MMIO-poke `CP_HQD_PQ_WPTR`** (doorbell dead).
  3. Complete via **polling** (`HSA_ENABLE_INTERRUPT=0`) on the EOP/completion fence; `hipMemcpy` back; assert `h[i] == N` (`multi_dispatch_test.cpp` lines 52–58).
- **Files/tools.** `amd_lite_direct_queue.cpp` (`SubmitDirectQueue`, `MapLegacyQueueWithMes`), `multi_dispatch_test.cpp`; env `ROCR_..._MMIO_WPTR`, `HSA_ENABLE_INTERRUPT=0`.
- **Acceptance.** Serial: `SURVIVED 1 dispatches; verify=PASS (x[0]=1.0 expected=1)`; then ramp `N` (e.g. 200) as a robustness bonus (not required for M0).
- **Effort.** 2–4 wk. **Risk.** Med (given T0.7/T0.8, the residual is ring/wptr/fence details already solved on other OSes). **Deps.** T0.8.

---

### T0.10 — (Optional/stretch) real MSI-X IRQ delivery
- **Goal.** Remove the polling crutch: MSI-X → guest vector → nucleus IDT → driver `EventCap`, so ROCr's interrupt-driven waits work (`IOC_SETUP_IRQ` + eventfd-equivalent).
- **Steps.** Program the device MSI-X table (in a BAR) to a guest vector; route to a nucleus ISR that signals the process's event port; back T0.5's `ppoll`/`read` on the event fd.
- **Acceptance.** With `HSA_ENABLE_INTERRUPT=1`, dispatch completes via interrupt, not poll.
- **Effort.** 2–4 wk. **Risk.** Med (MSI-X remap fidelity in the guest). **Deps.** T0.9. **Not required to declare M0.**

---

### T0.11 — One-command repro + freeze the spec surface
- **Goal.** M0 exit criteria (decision doc §5-M0).
- **Steps.**
  1. A single script: host `start-gpu-vm.sh` (no-FLR) [+ host firmware-load] → `virsh start` nucleus → in-guest dispatch → captured serial log.
  2. Capture the log showing `BOOTLOAD_COMPLETE` → MES/direct-MEC → completed wave → expected VRAM value.
  3. **Freeze** the T0.4 ioctl→capability mapping as the spec surface M1–M3 verify against (tag each op VERIFIED / TRUSTED-STUB / UNTRUSTED per decision doc §3).
- **Acceptance.** Fresh-checkout engineer reproduces from the one command; the mapping doc is committed.
- **Effort.** ~1 wk. **Risk.** Low. **Deps.** T0.9.

---

## 3. libvirt/QEMU specifics (shark-a)

Extend the `start-gpu-vm.sh` domain to **direct-boot the nucleus** and pass through the GPU (+ its `.1` HDMI-audio function, per the script's `GPU_AUDIO`):

```xml
<os>
  <type arch='x86_64' machine='q35'>hvm</type>
  <kernel>/var/lib/rustproof/nucleus.elf</kernel>          <!-- PVH-noted ELF, QEMU -kernel -->
  <cmdline>console=ttyS0 rustproof.loglevel=trace</cmdline>
  <initrd>/var/lib/rustproof/firmware.cpio</initrd>        <!-- T0.6 firmware RAM image -->
</os>
<features><acpi/></features>
<devices>
  <serial type='pty'/><console type='pty'/>                <!-- ttyS0 to the host -->
  <hostdev mode='subsystem' type='pci' managed='no'>       <!-- managed='no': start-gpu-vm.sh already bound vfio-pci, no FLR -->
    <source><address domain='0x0000' bus='0xc3' slot='0x00' function='0x0'/></source>
  </hostdev>
  <hostdev mode='subsystem' type='pci' managed='no'>
    <source><address domain='0x0000' bus='0xc3' slot='0x00' function='0x1'/></source>
  </hostdev>
</devices>
```

Notes: keep `managed='no'` so libvirt does **not** re-bind/FLR the device (the script owns bind + `reset_method=""`). `q35` for PCIe. `-serial mon:stdio` (bare QEMU) or the `<serial>` pty for the nucleus log. **No `<iommu>` device at M0** — the emulated vIOMMU is introduced at **M3** (decision doc §3); at M0 the host physical AMD-Vi is the only enforcer.

---

## 4. Effort, ordering, top risks

| Task | Effort (1 eng) | Risk | Hard deps |
|---|---|---|---|
| T0.0 baseline on host | 0.5–1 wk | Low | — |
| T0.1 bootable nucleus | 3–5 wk | Med | — |
| T0.2 AS/threads/caps | 3–5 wk | Med | T0.1 |
| T0.3 userland loader | 2–3 wk | Low-Med | T0.2 |
| T0.4 host-contract (ioctls) | 4–8 wk | **High** | T0.2, T0.3 |
| **T0.5 no-POSIX C++ hosting** | **8–16 wk** | **Highest** | T0.2–T0.4 |
| T0.6 firmware provisioning | 1–2 wk | Low | T0.5 |
| T0.7 ROCr/HIP attach | 4–8 wk | High | T0.4–T0.6 |
| **T0.8 in-guest MES/IH bring-up** | **4–10 wk** | **High/uncertain** | T0.7, T0.0 |
| T0.9 single dispatch + verify | 2–4 wk | Med | T0.8 |
| T0.10 MSI-X IRQ (optional) | 2–4 wk | Med | T0.9 |
| T0.11 repro + freeze spec | ~1 wk | Low | T0.9 |

Sum of the mandatory path ≈ **33–63 engineer-weeks ≈ 8–15 months solo**; overlap (T0.1–T0.3 vs. an early strace-driven T0.5 spike; T0.4 vs. T0.6) pulls it toward the decision doc's **~6–12 months**. Do not present a shorter number.

**The two items that will actually set the schedule:**
1. **T0.5 (no-POSIX C++ hosting).** A large, multithreaded C++ runtime with `std::thread`/futex/exceptions and static libstdc++, on a from-scratch kernel. De-risk with an early `strace` capture in T0.0 to fix the exact syscall set, and prototype the personality against a stock ROCr "hello agent" before the real stack.
2. **T0.8 (in-guest MES/IH bring-up).** The RAM-ring stages don't survive the no-FLR handover, so the guest must re-establish MES/KIQ/IH — today Python. Choose the C++/Rust port (recommended) vs. CPython personality **before** T0.7, because it changes T0.5's surface.

**Carry-forward reminders:** confirm shark-a's **ReBAR/BAR-size** and **ACS** posture (decision doc R2/R4 — shapes T0.4 and later M3/M4); keep the fragile PSP load on known-good Linux (T0.8 host side); and repeat in every M0 write-up that **M0 proves nothing — isolation here is the host IOMMU, not the nucleus** (§0, decision doc A7/A8).

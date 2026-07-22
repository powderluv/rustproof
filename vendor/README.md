# vendor/ — pinned external sources (git submodules)

These are added during M0 (not yet present in the scaffold):

- `vendor/rocr-lite/` — a pinned subset of the `rocm-systems` ROCr `lite::`
  direct-queue driver (`amd_lite_direct_queue.cpp` + the gfx1201 dispatch core).
  Built by CMake into a static `libamd_lite.a` and linked into the UNTRUSTED
  `driver-host` process. See docs/repo-structure.md sec 4 and docs/milestone-M0.md.
- `vendor/limine/` — pinned Limine bootloader binaries (the boot protocol the
  nucleus targets). See docs/repo-structure.md sec 3.2.

Add with e.g. `git submodule add <url> vendor/rocr-lite` once the exact upstream
revisions are chosen.

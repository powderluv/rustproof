#!/usr/bin/env python3
"""Mutate load-bearing predicates and report which mutations the host suite does not notice.

A test that never fails is worth nothing, and this repository has already shipped two of them:
`Domain::contained` was `fn contained() -> bool { true }` for a while and passed all 221 tests,
and `PageFlags::NO_CACHE` could be zeroed with the whole suite still green. Both were found by
accident. This finds them on purpose.

Each entry is a small, PLAUSIBLE edit — the kind a careless change would make — applied to one
file, with the suite run against it. A mutation the suite still passes is a SURVIVOR: some
property is stated somewhere and checked nowhere. Survivors are not automatically defects; some
are equivalent mutants that cannot change behaviour on any reachable input, and the report says
to check that before acting. What they are never is noise to skip.

Usage:  tools/mutate.py            # run them all
        tools/mutate.py <substr>   # only mutations whose label contains <substr>
"""

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# (label, file, find, replace) — `find` must occur EXACTLY once, or the mutation is reported as
# STALE rather than silently skipped. A mutation that did not apply looks exactly like one the
# suite killed, which is the failure mode this whole file exists to avoid.
MUTATIONS = [
    ("capabilities: rights check always passes", "crates/abi/src/lib.rs",
     "self.0 & other.0 == other.0", "true"),
    ("capabilities: a freed slot still resolves", "crates/capabilities/src/lib.rs",
     "if idx < N && !self.slots[idx].is_free() {", "if idx < N {"),
    ("iommu: contained() cannot fail", "crates/iommu/src/lib.rs",
     "self.maps.iter().filter(|m| m.live).all(|m| {", "self.maps.iter().take(0).all(|m| {"),
    ("iommu: granted() scans only the first slot", "crates/iommu/src/lib.rs",
     "        self.grants\n            .iter()\n            .find(|g| g.live && g.frame == frame)",
     "        self.grants\n            .iter()\n            .take(1)\n            .find(|g| g.live && g.frame == frame)"),
    ("iommu: revoke drops the grant but keeps the mappings", "crates/iommu/src/lib.rs",
     "        for m in self.maps.iter_mut() {\n            if m.live && m.frame == frame {",
     "        for m in self.maps.iter_mut() {\n            if false && m.live && m.frame == frame {"),
    ("vspace: NO_CACHE is not uncached", "crates/vspace/src/lib.rs",
     "PageFlags((1 << 3) | (1 << 4))", "PageFlags(0)"),
    ("regions: a borrower counts as the owner", "crates/regions/src/lib.rs",
     "let owned = regions.iter().any(|x| x.live && x.id == r && x.owner == id);",
     "let owned = regions.iter().any(|x| x.live && x.id == r);"),
    ("deleg: revoke_from finds no grandchildren", "crates/deleg/src/lib.rs",
     "        loop {\n            let mut progress = false;", "        for _ in 0..1 {\n            let mut progress = false;"),
    ("mm: alloc_contiguous returns overlapping runs", "crates/mm/src/lib.rs",
     "        if pages == 0 || pages > self.total {", "        if pages == 0 {"),
    ("runstate: classify counts dead slots as live", "crates/runstate/src/lib.rs",
     "    for (i, s) in slots.iter().enumerate() {\n        if !s.live {",
     "    for (i, s) in slots.iter().enumerate() {\n        if false {"),
    # --- crates that predate this session's scrutiny ---
    ("abi: delegation INTERSECT becomes union (rights can be gained)", "crates/abi/src/lib.rs",
     "        CapRights(self.0 & other.0)", "        CapRights(self.0 | other.0)"),
    ("capabilities: a Null capability can be inserted", "crates/capabilities/src/lib.rs",
     "        if cap_type == CapType::Null {\n            return None;\n        }",
     "        if false {\n            return None;\n        }"),
    ("sched: the run queue accepts duplicates", "crates/sched/src/lib.rs",
     "        if self.len == N || self.contains(tid) {", "        if self.len == N {"),
    ("sched: next() never advances (one thread starves the rest)", "crates/sched/src/lib.rs",
     "        self.cur = (self.cur + 1) % self.len;", "        self.cur %= self.len;"),
    ("ipc: a direct delivery also wakes a sender that never blocked", "crates/ipc/src/lib.rs",
     "                words: buf,\n                wake_sender: false,\n            };",
     "                words: buf,\n                wake_sender: true,\n            };"),
    ("loader: a program header may run past the image", "crates/loader/src/lib.rs",
     "        if ph_end > elf.len() {", "        if false {"),
    ("hostcontract: MAP_BAR no longer requires READ", "crates/hostcontract/src/lib.rs",
     "if rights.contains(CapRights::READ) =>", "if true =>"),
    ("hostcontract: a BAR window is writable regardless of WRITE", "crates/hostcontract/src/lib.rs",
     "            (base, rights.contains(CapRights::WRITE))", "            (base, true)"),
    ("abi: from_user does not mask undefined right bits", "crates/abi/src/lib.rs",
     "        CapRights((word as u8) & CapRights::ALL.0)", "        CapRights(word as u8)"),
    ("capabilities: revoke ignores an out-of-range slot id", "crates/capabilities/src/lib.rs",
     "        if cap.0 >= N {\n            return;\n        }", "        if cap.0 >= N + 1 {\n            return;\n        }"),
    ("iommu: an IOVA can be silently repointed", "crates/iommu/src/lib.rs",
     "        if self.maps.iter().any(|m| m.live && m.iova == iova) {", "        if false {"),
    ("mm: alloc_frame ignores the general floor", "crates/mm/src/lib.rs",
     "        let frame = self.first_free(self.cursor.max(self.general_floor))?;",
     "        let frame = self.first_free(self.cursor)?;"),
    ("mm: free_frame accepts a frame below the reserve floor", "crates/mm/src/lib.rs",
     "        if frame.as_u64() < self.reserve_below {\n            return;\n        }",
     "        if false {\n            return;\n        }"),
    ("runstate: well_formed misses a sender parked beside a live receiver", "crates/runstate/src/lib.rs",
     "            if find_recv(slots, ep).is_some() {\n                return false;\n            }",
     "            if false {\n                return false;\n            }"),
    ("vspace: map accepts an unaligned virtual address", "crates/vspace/src/lib.rs",
     "        if !va.is_page_aligned() {", "        if false {"),
    ("vspace: map walks THROUGH a huge page instead of refusing", "crates/vspace/src/lib.rs",
     "                if entry.is_huge() {\n                    return Err(MapError::HugePagePresent);",
     "                if false {\n                    return Err(MapError::HugePagePresent);"),
    ("vspace: unmap treats a huge entry as a table", "crates/vspace/src/lib.rs",
     "            if !entry.is_present() || entry.is_huge() {", "            if !entry.is_present() {"),
    ("deleg: splice_out never re-parents a grandchild", "crates/deleg/src/lib.rs",
     "                if out.live && out.parent == inc.child {", "                if false {"),
    ("regions: a plan may free a region before unmapping it", "crates/regions/src/lib.rs",
     "                        if r == region {", "                        if false {"),
    # --- fourth expansion: the syscall clamps, the ELF segment guards, the run-state verdict ---
    ("hostcontract: DEBUG_WRITE's output cap is not a cap", "crates/hostcontract/src/lib.rs",
     "    let total = len.min(DEBUG_MAX_TOTAL);", "    let total = len.max(DEBUG_MAX_TOTAL);"),
    ("loader: p_filesz > p_memsz copies past the segment", "crates/loader/src/lib.rs",
     "    let copy_end_va = file_end_va.min(seg_end_va);",
     "    let copy_end_va = file_end_va.max(seg_end_va);"),
    ("loader: a non-executable segment is mapped executable", "crates/loader/src/lib.rs",
     "    if p_flags & PF_X == 0 {", "    if p_flags & PF_X != 0 {"),
    ("loader: a read-only segment is mapped writable", "crates/loader/src/lib.rs",
     "    if p_flags & PF_W != 0 {", "    if p_flags & PF_W == 0 {"),
    ("loader: a zero-length segment is mapped anyway", "crates/loader/src/lib.rs",
     "    if p_memsz == 0 {\n        return Ok(());", "    if false {\n        return Ok(());"),
    ("runstate: a live-but-unparked state reports Park, not Deadlock", "crates/runstate/src/lib.rs",
     "    } else if any_live {\n        Next::Deadlock", "    } else if any_live {\n        Next::Park"),
    ("sched: remove() compacts one slot short (a stale duplicate survives)", "crates/sched/src/lib.rs",
     "        for i in idx..self.len - 1 {", "        for i in idx + 1..self.len - 1 {"),
    # --- fifth expansion: riscv64 is a supported architecture and the table had NEVER
    # --- touched it. Its crates are in the host suite, so they are TESTED; nothing had
    # --- ever asked whether those tests can FAIL. Same shape as CI never running FIRMWARE=1.
    ("loader-riscv: a program header may run past the image", "crates/loader-riscv/src/lib.rs",
     "        if ph_end > elf.len() {", "        if false {"),
    ("loader-riscv: p_filesz > p_memsz copies past the segment", "crates/loader-riscv/src/lib.rs",
     "    let copy_end_va = file_end_va.min(seg_end_va);",
     "    let copy_end_va = file_end_va.max(seg_end_va);"),
    ("loader-riscv: a zero-length segment is mapped anyway", "crates/loader-riscv/src/lib.rs",
     "    if p_memsz == 0 {\n        return Ok(());", "    if false {\n        return Ok(());"),
    ("loader-riscv: a non-executable segment is mapped executable", "crates/loader-riscv/src/lib.rs",
     "    if p_flags & PF_X != 0 {", "    if p_flags & PF_X == 0 {"),
    ("loader-riscv: a read-only segment is mapped writable", "crates/loader-riscv/src/lib.rs",
     "    if p_flags & PF_W != 0 {", "    if p_flags & PF_W == 0 {"),
    ("vspace-riscv: map accepts an unaligned virtual address", "crates/vspace-riscv/src/lib.rs",
     "        if !va.is_page_aligned() {\n            return Err(MapError::UnalignedVirt);",
     "        if false {\n            return Err(MapError::UnalignedVirt);"),
    ("vspace-riscv: a leaf with no R/W/X is written (it decodes as a POINTER)",
     "crates/vspace-riscv/src/lib.rs",
     "        if !flags.intersects(PageFlags::RWX) {", "        if false {"),
    ("vspace-riscv: map walks THROUGH a superpage instead of refusing",
     "crates/vspace-riscv/src/lib.rs",
     "                if entry.is_leaf() {\n                    return Err(MapError::SuperpagePresent);",
     "                if false {\n                    return Err(MapError::SuperpagePresent);"),
]


def run_suite() -> bool:
    """True if the host suite passes."""
    r = subprocess.run(["bash", "tools/host-tests.sh"], cwd=ROOT,
                       capture_output=True, text=True)
    return r.returncode == 0


def main() -> int:
    only = sys.argv[1] if len(sys.argv) > 1 else None
    picked = [m for m in MUTATIONS if only is None or only in m[0]]
    if not picked:
        print(f"no mutation matches {only!r}")
        return 2

    print("baseline: ", end="", flush=True)
    if not run_suite():
        print("the suite FAILS before any mutation — fix that first")
        return 2
    print("suite passes")

    survivors, stale = [], []
    for label, rel, find, repl in picked:
        path = ROOT / rel
        original = path.read_text()
        if original.count(find) != 1:
            stale.append((label, original.count(find)))
            print(f"  STALE   {label} (pattern occurs {original.count(find)}x)")
            continue
        path.write_text(original.replace(find, repl, 1))
        try:
            survived = run_suite()
        finally:
            path.write_text(original)
        print(f"  {'SURVIVED' if survived else 'killed  '} {label}")
        if survived:
            survivors.append(label)

    print()
    print(f"{len(picked)} mutations: {len(survivors)} survived, {len(stale)} stale")
    for s in survivors:
        print(f"  SURVIVOR: {s}")
    if stale:
        print("  stale patterns no longer match the source — update tools/mutate.py")
    # A survivor FAILS. The table stands at zero, so any new one is a coverage regression, and
    # "look at it later" is how the two historical vacuous tests survived as long as they did.
    # If a survivor turns out to be an EQUIVALENT mutant — unable to change behaviour on any
    # reachable input — delete it from the table with a note saying why, rather than leaving it
    # to be re-triaged every run. Stale patterns fail too: a mutation that did not apply looks
    # exactly like one the suite killed.
    return 1 if (survivors or stale) else 0


if __name__ == "__main__":
    sys.exit(main())

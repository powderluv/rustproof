#!/usr/bin/env bash
# Run the host unit tests, and REFUSE to run if a crate that has tests is not in the list.
#
# The list is hand-maintained because most crates in this workspace are bare-metal and cannot
# build for the host, so `cargo test --workspace` is not an option. A hand-maintained list
# goes stale silently: a crate was added with 13 tests covering the kernel's most defect-dense
# logic, and none of them ran in CI, because adding the crate and adding it to the list are two
# separate acts and only one of them is obvious. The check below makes the omission loud.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST_CRATES=(
    abi
    mm
    vspace
    vspace-riscv
    capabilities
    deleg
    iommu
    runstate
    regions
    ipc
    sched
    loader
    loader-riscv
    hostcontract
    kernel
)

# Every crate whose source contains a #[test] must appear above.
#
# The comment here used to add: "Bare-metal crates that genuinely cannot be host-tested have
# no #[test] in them, so they do not trip this." That reasoning is backwards, and it exempted
# the largest crate in the tree. `kernel` -- 2360 lines, the whole syscall surface -- had NO
# tests, so it could never trip a guard that fires on the PRESENCE of #[test]; and it could
# not be host-built only because `sched::Context` had no off-target stub. "Cannot build for
# the host" and "has nothing worth testing" are different claims, and neither was checked.
# Both are fixed: see crates/sched/src/context_host.rs.
#
# So the guard still cannot see a crate that is testable but untested. What it can now see is
# a crate that HAS tests and is not listed, which is the failure it was written for.
missing=()
while IFS= read -r dir; do
    name=$(basename "$dir")
    if ! grep -rq '#\[test\]' "$dir/src" 2>/dev/null; then
        continue
    fi
    found=0
    for c in "${HOST_CRATES[@]}"; do
        [ "$c" = "$name" ] && found=1 && break
    done
    [ "$found" = 0 ] && missing+=("$name")
done < <(find crates -mindepth 1 -maxdepth 1 -type d | sort)

if [ ${#missing[@]} -ne 0 ]; then
    echo "host-tests: these crates have #[test] but are not run:" >&2
    printf '  %s\n' "${missing[@]}" >&2
    echo "Add them to HOST_CRATES in tools/host-tests.sh (or explain why they cannot build for the host)." >&2
    exit 1
fi

# A source-shape check, not a cargo test, but it belongs here: it needs no QEMU, so it gates
# every CI run. It refuses a demo that puts a live assertion behind a deliberately-fatal
# instruction — the defect that made the FREE_REGION owner-identity check dead code for
# fifteen commits while the boot kept reporting PASS.
python3 tools/check-demo-order.py

args=()
for c in "${HOST_CRATES[@]}"; do args+=(-p "$c"); done
exec cargo test "${args[@]}" "$@"

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
    runstate
    regions
    ipc
    sched
    loader
    loader-riscv
    hostcontract
)

# Every crate whose source contains a #[test] must appear above. Bare-metal crates that
# genuinely cannot be host-tested have no #[test] in them, so they do not trip this.
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

args=()
for c in "${HOST_CRATES[@]}"; do args+=(-p "$c"); done
exec cargo test "${args[@]}" "$@"

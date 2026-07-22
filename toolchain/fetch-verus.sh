#!/usr/bin/env bash
# Download the exact pinned Verus release (verus, rust_verify, vstd, z3),
# verify its SHA-256, and install under toolchain/verus/. Verification ALWAYS
# uses this local z3 (--z3-path toolchain/verus/z3), never a system z3.
# TODO(M1): implement against the values in toolchain/verus.lock.
set -euo pipefail
echo "fetch-verus.sh: not yet implemented -- see toolchain/verus.lock and docs/verification.md" >&2
exit 1

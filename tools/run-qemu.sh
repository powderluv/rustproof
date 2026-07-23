#!/usr/bin/env bash
# Build the Rustproof nucleus and boot it under QEMU (no GPU), checking the serial
# console. Cross-builds x86_64-unknown-none from any host.
#
#   tools/run-qemu.sh                    # normal boot; expect "rustproof: BOOT OK"
#   PROVOKE_FAULT=1 tools/run-qemu.sh    # force a #PF; expect the exception dump
#   PROFILE=debug tools/run-qemu.sh      # debug profile (default: release)
# note: no `-u` — bash 3.2 (macOS) errors on empty-array expansion under nounset
set -eo pipefail
cd "$(dirname "$0")/.."   # repo root

TARGET=x86_64-unknown-none
PROFILE="${PROFILE:-release}"
BUILD_FLAG=()
[ "$PROFILE" = "release" ] && BUILD_FLAG+=(--release)

FEATURES=()
EXPECT="rustproof: BOOT OK"
MODE="boot"
if [ "${PROVOKE_FAULT:-0}" = "1" ]; then
    FEATURES+=(--features provoke-fault)
    EXPECT="CPU EXCEPTION 14 (page fault)"
    MODE="fault-dump"
fi

echo "== building nucleus ($PROFILE, mode=$MODE) for $TARGET =="
cargo build -p nucleus --target "$TARGET" "${BUILD_FLAG[@]}" "${FEATURES[@]}"

KERNEL="target/$TARGET/$PROFILE/nucleus"
echo "== image: $(file "$KERNEL") =="

echo "== booting under QEMU (TCG) =="
set +e
OUT=$(timeout 30 qemu-system-x86_64 \
    -kernel "$KERNEL" \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -serial stdio -display none -no-reboot 2>&1)
RC=$?
set -e
echo "----- guest serial -----"
echo "$OUT"
echo "------------------------"
echo "== qemu process exit code: $RC =="

if echo "$OUT" | grep -qF "$EXPECT"; then
    echo "RESULT: PASS ($MODE) — saw: $EXPECT"
    exit 0
fi
echo "RESULT: FAIL ($MODE) — expected: $EXPECT"
exit 1

#!/usr/bin/env bash
# Build the Rustproof nucleus and boot it under QEMU (no GPU), checking for the
# boot banner on the serial console. Cross-builds x86_64-unknown-none from any host.
#
#   PROFILE=release tools/run-qemu.sh    (default)
#   PROFILE=debug   tools/run-qemu.sh
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

TARGET=x86_64-unknown-none
PROFILE="${PROFILE:-release}"
BANNER="rustproof: BOOT OK"
BUILD_FLAG=""
[ "$PROFILE" = "release" ] && BUILD_FLAG="--release"

echo "== building nucleus ($PROFILE) for $TARGET =="
cargo build -p nucleus --target "$TARGET" $BUILD_FLAG

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
echo "== qemu process exit code: $RC (33 = isa-debug-exit success) =="

if echo "$OUT" | grep -q "$BANNER"; then
    echo "RESULT: PASS — boot banner seen"
    exit 0
fi
echo "RESULT: FAIL — boot banner not seen"
exit 1

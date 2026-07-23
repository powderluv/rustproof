#!/usr/bin/env bash
# Build the Rustproof RISC-V nucleus and boot it under qemu-system-riscv64, checking
# the serial console. Cross-builds riscv64gc-unknown-none-elf from any host.
#
#   tools/run-qemu-riscv.sh                 # normal boot; expect "rustproof: RV BOOT OK"
#   PROFILE=debug tools/run-qemu-riscv.sh   # debug profile (default: release)
# note: no `-u` — bash 3.2 (macOS) errors on empty-array expansion under nounset
set -eo pipefail
cd "$(dirname "$0")/.."   # repo root

TARGET=riscv64gc-unknown-none-elf
PROFILE="${PROFILE:-release}"
BUILD_FLAG=()
[ "$PROFILE" = "release" ] && BUILD_FLAG+=(--release)

EXPECT="rustproof: RV BOOT OK"

echo "== building nucleus-riscv ($PROFILE) for $TARGET =="
cargo build -p nucleus-riscv --target "$TARGET" "${BUILD_FLAG[@]}"

KERNEL="target/$TARGET/$PROFILE/nucleus-riscv"
echo "== image: $(file "$KERNEL") =="

# OpenSBI (-bios default) loads the -kernel ELF at 0x8020_0000 and jumps to _start in
# S-mode. Serial goes to stdio via -nographic; the SiFive test device powers off QEMU.
echo "== booting under qemu-system-riscv64 (OpenSBI) =="
set +e
OUT=$(timeout 30 qemu-system-riscv64 \
    -machine virt \
    -bios default \
    -kernel "$KERNEL" \
    -m 512M \
    -nographic \
    -no-reboot 2>&1)
RC=$?
set -e
echo "----- guest serial -----"
echo "$OUT"
echo "------------------------"
echo "== qemu process exit code: $RC =="

if echo "$OUT" | grep -qF "$EXPECT"; then
    echo "RESULT: PASS — saw: $EXPECT"
    exit 0
fi
echo "RESULT: FAIL — expected: $EXPECT"
exit 1

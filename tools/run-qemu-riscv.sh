#!/usr/bin/env bash
# Build the Rustproof RISC-V nucleus and boot it under qemu-system-riscv64, checking
# the serial console. Cross-builds riscv64gc-unknown-none-elf from any host.
#
#   tools/run-qemu-riscv.sh                 # normal boot; expect "rustproof: BOOT OK"
#   PROFILE=debug tools/run-qemu-riscv.sh   # debug profile (default: release)
# note: no `-u` — bash 3.2 (macOS) errors on empty-array expansion under nounset
set -eo pipefail
cd "$(dirname "$0")/.."   # repo root

TARGET=riscv64gc-unknown-none-elf
PROFILE="${PROFILE:-release}"
BUILD_FLAG=()
[ "$PROFILE" = "release" ] && BUILD_FLAG+=(--release)

EXPECT="rustproof: BOOT OK"

echo "== building riscv-init (U-mode user program) =="
cargo build -p riscv-init --target "$TARGET" "${BUILD_FLAG[@]}"
cp "target/$TARGET/$PROFILE/riscv-init" crates/nucleus-riscv/user.elf

echo "== building nucleus-riscv ($PROFILE) for $TARGET =="
cargo build -p nucleus-riscv --target "$TARGET" "${BUILD_FLAG[@]}"

KERNEL="target/$TARGET/$PROFILE/nucleus-riscv"
echo "== image: $(file "$KERNEL") =="

# OpenSBI (-bios default) loads the -kernel ELF at 0x8020_0000 and jumps to _start in
# S-mode. Serial goes to stdio via -nographic; the SiFive test device powers off QEMU.
echo "== booting under qemu-system-riscv64 (OpenSBI) =="
# The guest ends by BLOCKING on the console interrupt: it parks the CPU until a byte really
# arrives, because that is the one wake-up the timer cannot provide. So we have to supply
# one -- but WHEN matters, and a fixed delay gets it wrong in both directions. Too early and
# the credit is consumed by the demo's own per-line check (which reads and clears the
# console count), leaving nothing to wake the final wait: a correct kernel then reports a
# false leak and hangs until the timeout. Too late, or on a host slow enough that the guest
# has not blocked yet, and the byte is taken while other processes are still runnable, which
# wakes nobody from a park and quietly stops testing the thing this exists to test.
#
# So do not guess. Wait for the guest to announce it is blocked, then wait for it to go
# SILENT -- a parked kernel produces no output -- and only then send. Both conditions are
# observations, not timings, so this behaves the same on a fast host and a loaded one.
feed_console_byte() {
    # Two things must have happened, and neither is a time: the process must have announced
    # it is blocked on the console, and the interrupt helper must have finished its timer
    # loop. Waiting for silence alone is NOT enough -- the helper spends seconds blocking and
    # waking on the timer while printing nothing, so a quiet log there means "busy", not
    # "parked". Getting this wrong is not a hang, it is worse: the byte lands while the
    # helper is runnable, wakes nobody from a park, and the run still passes. That is exactly
    # what riscv did, and the kernel's device-wake counter is what caught it.
    local waited=0
    while ! grep -q 'blocking on the console line' "$LOG" 2>/dev/null ||
        ! grep -q 'irq: helper woke' "$LOG" 2>/dev/null; do
        sleep 0.1
        waited=$((waited + 1))
        [ "$waited" -gt 600 ] && return 0   # never got there: send nothing, let it time out
    done
    # Now silence really does mean parked: everything that prints has printed.
    local last=-1 cur quiet=0
    while [ "$quiet" -lt 3 ]; do
        cur=$(wc -c <"$LOG" 2>/dev/null || echo 0)
        if [ "$cur" = "$last" ]; then quiet=$((quiet + 1)); else quiet=0; fi
        last=$cur
        sleep 0.5
    done
    printf 'k'
}

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

set +e
feed_console_byte | timeout 30 qemu-system-riscv64 \
    -machine virt \
    -bios default \
    -kernel "$KERNEL" \
    -m 512M \
    -nographic \
    -no-reboot >"$LOG" 2>&1
RC=$?
set -e
OUT=$(cat "$LOG")
echo "----- guest serial -----"
echo "$OUT"
echo "------------------------"
echo "== qemu process exit code: $RC =="


# The guest is supposed to have been woken from a PARK by the console device. A console byte
# that arrives while something is still runnable also satisfies the guest's own success line
# but ends no park, so check the kernel's counter instead of trusting the narrative.
# Only for a normal boot: the fault-provoking run dies on purpose long before this point.
if [ "${PROVOKE_FAULT:-0}" != "1" ] &&
    ! echo "$OUT" | grep -qE '\[kernel\] a device ended the park [1-9][0-9]* time\(s\)'; then
    echo "RESULT: FAIL — a device never ended an idle park (the console byte arrived too early)"
    exit 1
fi

if echo "$OUT" | grep -qF "$EXPECT"; then
    echo "RESULT: PASS — saw: $EXPECT"
    exit 0
fi
echo "RESULT: FAIL — expected: $EXPECT"
exit 1

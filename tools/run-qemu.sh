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

echo "== building the init user program (ring 3) =="
# init is linked at 512 GiB (user VA window), which needs the large code model; a
# repo-root build does not read init's crate-local .cargo/config, so pass it here.
RUSTFLAGS="-C relocation-model=static -C code-model=large" \
    cargo build -p init --target "$TARGET" "${BUILD_FLAG[@]}"
cp "target/$TARGET/$PROFILE/init" crates/nucleus/user.elf

echo "== building nucleus ($PROFILE, mode=$MODE) for $TARGET =="
cargo build -p nucleus --target "$TARGET" "${BUILD_FLAG[@]}" "${FEATURES[@]}"

KERNEL="target/$TARGET/$PROFILE/nucleus"
echo "== image: $(file "$KERNEL") =="

echo "== booting under QEMU (TCG) =="
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
feed_console_byte | timeout 30 qemu-system-x86_64 \
    -kernel "$KERNEL" \
    -m 512M \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -serial stdio -display none -no-reboot >"$LOG" 2>&1
RC=$?
set -e
OUT=$(cat "$LOG")
echo "----- guest serial -----"
echo "$OUT"
echo "------------------------"
echo "== qemu process exit code: $RC =="


# The guest's assertions print a line ending in "(bug)" when they fail, and then the guest
# keeps going and still reaches BOOT OK. Until this check existed, that made every one of
# them a comment rather than a gate: a review demonstrated a REAL capability-rights
# amplification (a READ-only loan mapped writable) printing one "(bug)" line and passing
# green in CI on both arches. Any such line now fails the run.
if echo "$OUT" | grep -q '(bug)'; then
    echo "RESULT: FAIL — the guest reported a failed assertion:"
    echo "$OUT" | grep '(bug)' | sed 's/^/    /'
    exit 1
fi

# A frame leak makes the kernel exit early, so check it BEFORE anything else: otherwise the
# first unrelated check to fail gets the blame and the real cause scrolls past.
if echo "$OUT" | grep -q '\[mm\] LEAK:'; then
    echo "RESULT: FAIL — $(echo "$OUT" | grep '\[mm\] LEAK:' | head -1)"
    exit 1
fi

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
    echo "RESULT: PASS ($MODE) — saw: $EXPECT"
    exit 0
fi
echo "RESULT: FAIL ($MODE) — expected: $EXPECT"
exit 1

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
# ICOUNT=<shift> ties guest time to instructions retired, which SHIFTS where the timer lands
# relative to the code -- a different interleaving per shift value, and a REPRODUCIBLE one.
# Repeating an identical run proves only that the schedule is deterministic; it is the same
# run again. This is the knob that asks whether an assertion depends on one interleaving.
ICOUNT_FLAG=()
[ -n "${ICOUNT:-}" ] && ICOUNT_FLAG=(-icount "shift=$ICOUNT")

feed_console_byte | timeout 60 qemu-system-riscv64 \
    -machine virt \
    -bios default \
    -kernel "$KERNEL" \
    -m 512M \
    "${ICOUNT_FLAG[@]}" \
    -nographic \
    -no-reboot >"$LOG" 2>&1
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
# The verdict below is a grep over a text stream. If that stream can be silently corrupted,
# every gate in this file is grading noise -- so check the stream itself before trusting it.
# Under `-icount` the console picks up long runs of a single repeated byte (thousands of 'R's
# in idle stretches), and output around them goes MISSING: one run lost the "[proc 10" of a
# tag. Cause not established -- it is not the console feeder (it reproduces with stdin closed)
# and the guest writes nothing in its idle path (`wfi`) -- so this refuses to grade rather
# than guessing. Legitimate output tops out near 25 repeats (OpenSBI's banner rules).
if printf '%s' "$OUT" | LC_ALL=C grep -qE '(.)\1{39}'; then
    echo "RESULT: FAIL — the console stream is corrupted (a run of 40+ identical bytes), so"
    echo "  the log cannot be graded. Output around such runs has been observed MISSING."
    printf '%s' "$OUT" | LC_ALL=C grep -oE '(.)\1{39,}' | head -3 | cut -c1-60 | sed 's/^/    /'
    exit 1
fi

if echo "$OUT" | grep -q '(bug)'; then
    echo "RESULT: FAIL — the guest reported a failed assertion:"
    echo "$OUT" | grep '(bug)' | sed 's/^/    /'
    exit 1
fi

# The clock denial is asserted by a process being KILLED, so the guest cannot print a failure
# line for it — a process that is allowed to read the counter simply lives. Require the kill.
if [ "${PROVOKE_FAULT:-0}" != "1" ] &&
    ! echo "$OUT" | grep -qE '\[kernel\] proc [0-9]+ killed: (general protection fault|illegal instruction)'; then
    echo "RESULT: FAIL — ring 3 was not refused the hardware clock (CR4.TSD / scounteren)"
    exit 1
fi

# A frame leak makes the kernel exit early, so check it BEFORE anything else: otherwise the
# first unrelated check to fail gets the blame and the real cause scrolls past.
# An assertion that never RUNS never prints its "(bug)" line, so the check above cannot see
# a demo whose later half was silently disabled. That happened: a ring-3 clock probe — which is
# expected to kill its process — was inserted ABOVE the FREE_REGION owner check, and every boot
# went on passing while the owner check no longer executed. Require the lines that come LAST in
# each process, so losing the tail of a demo fails the run.
# Scoped off under PROVOKE_FAULT, which kills the guest long before the demo gets here —
# the same scoping the clock gate above needs, and for the same reason.
# The revocation checks are a WAIT: the child polls until the owner revokes. A timeout is a
# scheduling fact, not a violation, and the demo says so instead of accusing the kernel — so the
# RUNNER is what turns an inconclusive run into a failure, with an accurate message. Without
# this gate the demo's honest "inconclusive" would simply PASS, which is worse than the "(bug)"
# it replaced: a check that cannot fail, wearing the words of one that can.
if [ "${PROVOKE_FAULT:-0}" != "1" ] && echo "$OUT" | grep -q 'inconclusive'; then
    echo "RESULT: FAIL — a check never observed the condition it was waiting for:"
    echo "$OUT" | grep -a 'inconclusive' | sed 's/^/    /'
    echo "  (this is a scheduling or resource outcome, not a containment defect — raise the"
    echo "   budget, or find out why the process being waited for is not making progress)"
    exit 1
fi

for want in 'share: a WRITABLE borrower still cannot destroy what it borrowed'; do
    if [ "${PROVOKE_FAULT:-0}" != "1" ] && ! echo "$OUT" | grep -qF "$want"; then
        echo "RESULT: FAIL — a required assertion never ran: $want"
        echo "  (an assertion that does not execute prints no '(bug)' line; something above it"
        echo "   probably killed or exited the process first)"
        exit 1
    fi
done

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
    echo "RESULT: PASS — saw: $EXPECT"
    exit 0
fi
echo "RESULT: FAIL — expected: $EXPECT"
exit 1

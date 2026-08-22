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
if [ "${FIRMWARE:-0}" = "1" ]; then
    FEATURES+=(--features firmware-boot)
fi
cargo build -p nucleus --target "$TARGET" "${BUILD_FLAG[@]}" "${FEATURES[@]}"

KERNEL="target/$TARGET/$PROFILE/nucleus"

# The firmware path needs a FLAT image: QEMU's multiboot loader refuses a 64-bit ELF ("give a
# 32bit one"), so the a.out kludge in the header describes an objcopy'd binary instead. Booting
# through SeaBIOS is what assigns PCI BARs, which is what a DMA-capable device needs before it
# can be told to transfer.
if [ "${FIRMWARE:-0}" = "1" ]; then
    OBJCOPY=$(command -v llvm-objcopy || command -v objcopy || ls "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-objcopy 2>/dev/null | head -1)
    [ -n "$OBJCOPY" ] || { echo "FIRMWARE=1 needs llvm-objcopy or objcopy" >&2; exit 1; }
    "$OBJCOPY" -O binary "$KERNEL" "$KERNEL.bin"
    KERNEL="$KERNEL.bin"
    echo "== firmware path: flat image $KERNEL (SeaBIOS + multiboot) =="
fi
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
# Optional rig for IOMMU work. Default is unchanged — the i440fx `pc` machine with no IOMMU —
# because that is what every gate in this file was calibrated against. `IOMMU=1` switches to q35
# (which has ECAM) and attaches an emulated AMD-Vi unit, matching the design's target rather
# than VT-d. Opt-in, so a regression in the IOMMU rig cannot silently become a regression in the
# boot everyone runs.
MACHINE=()
if [ "${IOMMU:-0}" = "1" ]; then
    # A DMA-capable function to program a DTE for. The guest never drives it — the demo
    # touches only the serial port and debug-exit, neither of which does DMA — so enabling
    # translation cannot break the boot. That is exactly why it is safe to enable at all.
    # `edu` is a register-driven DMA engine (source/dest/count/command) — the device the
    # containment proof drives. The e1000 stays as a second DMA-capable function so the scan
    # has to distinguish them rather than take whatever it finds first.
    MACHINE=(-machine q35 -device amd-iommu -device e1000,bus=pcie.0 -device edu,bus=pcie.0)
    # A DMA-capable function BEHIND A BRIDGE. The bus scan started at bus 0 and stopped there,
    # so a device on a secondary bus was enumerated by nothing, got no device-table entry, and
    # was therefore PASSED THROUGH — the same hole the default-deny sweep closes on bus 0. The
    # flat rig cannot show that; this one can.
    if [ "${BRIDGE:-0}" = "1" ]; then
        MACHINE+=(-device pcie-pci-bridge,id=br0,bus=pcie.0 -device e1000,bus=br0,addr=1)
        echo "== plus a DMA-capable function behind a PCI bridge =="
    fi
    echo "== IOMMU rig: q35 + emulated AMD-Vi =="
fi

# Diagnostic hook: QEMU_TRACE='amdvi_*' turns on the emulator's own trace points, which is the
# only way to see which path a refusal took inside the unit rather than inferring it from the
# outside. Reasoning about QEMU's internals from memory is what produced several of the oracle
# failures in docs/verification.md; this observes them instead. Off by default — the extra output
# is noise for every other run.
# Trace output goes to its OWN file via -D. It used to land on stderr, which run-qemu.sh merges
# into the serial stream — and 392 interleaved trace lines chop serial lines in half, so a gate
# string stops matching and the run "fails" for a reason that has nothing to do with the guest.
# That cost one confusing FAIL: the assertion was present in the output and the gate still missed
# it.
TRACE=()
if [ -n "${QEMU_TRACE:-}" ]; then
    : "${QEMU_TRACE_FILE:=/tmp/qemu-amdvi-trace.log}"
    TRACE=(-trace "$QEMU_TRACE" -D "$QEMU_TRACE_FILE")
    echo "== qemu trace: $QEMU_TRACE -> $QEMU_TRACE_FILE =="
fi

feed_console_byte | timeout 30 qemu-system-x86_64 \
    "${MACHINE[@]}" \
    "${TRACE[@]}" \
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
# The IOMMU scan is asserted in BOTH directions, because either failure reads as success
# otherwise: a scan that silently finds nothing is indistinguishable from a machine that has no
# IOMMU, and a scan that matches anything would "find" one on a machine without.
if [ "${PROVOKE_FAULT:-0}" != "1" ]; then
    if [ "${IOMMU:-0}" = "1" ]; then
        if ! echo "$OUT" | grep -qE '\[iommu\] AMD-Vi at [0-9a-f]{2}:[0-9a-f]{2}\.[0-9] vendor=1022'; then
            echo "RESULT: FAIL — the IOMMU rig was launched but the nucleus found no AMD-Vi unit"
            exit 1
        fi
    elif ! echo "$OUT" | grep -qF '[iommu] no IOMMU on this machine'; then
        echo "RESULT: FAIL — an IOMMU was reported on a machine launched without one"
        exit 1
    fi
fi

# Conditional, so it is host-independent: ACPI availability turns out to be a property of the
# QEMU BUILD (8.2.1 on macOS supplies no RSDP; 8.2.2 on Ubuntu does). Requiring IVRS outright
# would fail on the former for no fault of the nucleus. But WHERE an RSDP exists and the IOMMU
# rig is up, the walk must reach a base — otherwise a silently broken parse reads exactly like
# a machine without an IOMMU.
if [ "${PROVOKE_FAULT:-0}" != "1" ] && [ "${IOMMU:-0}" = "1" ] &&
    echo "$OUT" | grep -q '\[acpi\] RSDP at'; then
    if ! echo "$OUT" | grep -qE '\[iommu\] IVRS names [0-9]+ IOMMU\(s\); AMD-Vi register base 0x[0-9a-f]+'; then
        echo "RESULT: FAIL — ACPI is present and the IOMMU rig is up, but the IVRS walk found"
        echo "  no AMD-Vi register base. A broken walk is indistinguishable from no IOMMU."
        exit 1
    fi
    # Having found the aperture, the kernel must reach it. EFR is a feature bitmap and is
    # never zero on a real unit, so requiring non-zero catches a mapping that silently did not
    # take effect — which otherwise reads as a device that happens to report nothing.
    if ! echo "$OUT" | grep -qE 'aperture mapped uncached at 0x[0-9a-f]+: EFR=0x0*[1-9a-f][0-9a-f]*'; then
        echo "RESULT: FAIL — the AMD-Vi aperture was located but not readable (EFR zero or absent)"
        exit 1
    fi
    # The Device Table base must WRITE and read back. A blind write has no oracle: a store into
    # an unmapped hole looks exactly like a store the unit accepted, so the read-back is the
    # only thing that distinguishes them.
    if ! echo "$OUT" | grep -qE 'device table 0x[0-9a-f]+ \(2 MiB, 64K BDFs\) installed; DTBR reads back 0x[0-9a-f]+'; then
        echo "RESULT: FAIL — the AMD-Vi device table was not installed (or its DTBR did not read back)"
        exit 1
    fi
    if echo "$OUT" | grep -q 'DTBR write did NOT take'; then
        echo "RESULT: FAIL — the DTBR write did not stick"
        exit 1
    fi
    # The unit must actually come up. V|TV must be set in the DTE and IommuEn in CTRL —
    # checking the log line alone would pass for a DTE of all zeros, which is a device the
    # IOMMU ignores entirely rather than one it bounds.
    if ! echo "$OUT" | grep -qE 'DTE\[0x[0-9a-f]+\] = 0x[0-9a-f]*[37bf] \(V TV mode=3'; then
        echo "RESULT: FAIL — no DTE with V|TV was programmed"
        exit 1
    fi
    if ! echo "$OUT" | grep -qE 'unit ENABLED, CTRL=0x[0-9a-f]*[13579bdf]'; then
        echo "RESULT: FAIL — the AMD-Vi unit did not report IommuEn"
        exit 1
    fi
    # EventLogEn (CTRL bit 2) as well as IommuEn (bit 0): without the log a refused DMA is
    # silent, so an enabled unit with no log can never demonstrate that it refused anything.
    if ! echo "$OUT" | grep -qE 'event log 0x[0-9a-f]+ \(256 entries\) armed'; then
        echo "RESULT: FAIL — the AMD-Vi event log was not armed"
        exit 1
    fi
    if ! echo "$OUT" | grep -qE 'unit ENABLED, CTRL=0x[0-9a-f]*[4567cdef]'; then
        echo "RESULT: FAIL — CTRL does not report EventLogEn"
        exit 1
    fi
    # Containment, on the firmware rig where a device can actually be driven. The oracle is the
    # PAYLOAD: the same two transfers land when the unit is not translating and do not when it
    # is, so "CONTAINED" means nothing reached memory rather than nothing was attempted — the
    # transfers are separately required to have completed at the device.
    if [ "${FIRMWARE:-0}" = "1" ]; then
        # REQUIRED, not a condition. This used to read `&& grep -q 'edu ident='`, and that line
        # is printed LAST, after five places where the probe can bail out quietly: no DMA-capable
        # device, not edu, no BAR, the BAR would not map, a wrong ident. Any of them made the
        # condition false, skipped every gate below, and reported PASS — the containment result
        # would have stopped being tested with CI still green. Reaching the device is itself part
        # of what the firmware rig asserts.
        if ! echo "$OUT" | grep -q 'edu ident='; then
            echo "RESULT: FAIL — the probe never reached the device, so nothing below was checked"
            exit 1
        fi
        if ! echo "$OUT" | grep -q 'transfers: RAM->dev done dev->RAM done'; then
            echo "RESULT: FAIL — the device did not complete its transfers; containment untested"
            exit 1
        fi
        if ! echo "$OUT" | grep -q '\[iommu\] CONTAINED:'; then
            echo "RESULT: FAIL — a bounded device's DMA reached memory"
            exit 1
        fi
        # BOTH halves, and the second is what makes the first mean something: blocking
        # everything is what a broken unit also does. A granted IOVA must reach exactly its
        # frame, or the "containment" above is indistinguishable from a wall.
        # The model must GOVERN the hardware: a frame no capability granted must be refused a
        # page-table entry. Without this, "translated" only shows the table works, not that
        # anything decides what goes in it.
        if ! echo "$OUT" | grep -q 'ungranted-frame refused (no PTE written)'; then
            echo "RESULT: FAIL — the domain let an ungranted frame into the I/O page table"
            exit 1
        fi
        # That line is the MODEL's verdict printed back. This one is the device's: a DMA is
        # actually aimed at the ungranted IOVA and must not land. Without it, a defect that
        # wrote the PTE anyway would still print "refused (no PTE written)" and pass.
        if ! echo "$OUT" | grep -q 'UNREACHABLE: the ungranted IOVA'; then
            echo "RESULT: FAIL — an ungranted IOVA was never tested against the device, or it"
            echo "  was reachable"
            exit 1
        fi
        if ! echo "$OUT" | grep -q '\[iommu\] TRANSLATED:'; then
            echo "RESULT: FAIL — a GRANTED IOVA did not reach its frame; the unit is refusing"
            echo "  everything rather than enforcing a policy"
            exit 1
        fi
        # Rights, not just reachability. Every refusal above is of an absent mapping; this one
        # is of a PRESENT and readable page that the device may not WRITE, which is the only
        # way to show the granted rights reach the hardware entry instead of being validated
        # and then discarded. They were discarded, for eighteen commits: the leaf was written
        # with a constant IR|IW, and every grant happened to be RW so nothing noticed.
        if ! echo "$OUT" | grep -q '\[iommu\] RIGHTS ENFORCED:'; then
            echo "RESULT: FAIL — a READ-only grant did not stop a device WRITE"
            exit 1
        fi
        # And withdrawal has to reach the hardware too. Clearing the page-table entry is not
        # revocation while the unit still holds a cached translation — measured, not assumed:
        # without the invalidation this exact check read back the pattern.
        if ! echo "$OUT" | grep -q '\[iommu\] REVOKED:'; then
            echo "RESULT: FAIL — a withdrawn mapping was still reachable, or the check for it"
            echo "  did not run"
            exit 1
        fi
        if ! echo "$OUT" | grep -q 'invalidation issued and COMPLETED'; then
            echo "RESULT: FAIL — the unit never acknowledged an invalidation, so the revocation"
            echo "  above proves nothing about ordering"
            exit 1
        fi
    fi
fi

# Scoped off under PROVOKE_FAULT, which kills the guest long before the demo gets here —
# the same scoping the clock gate above needs, and for the same reason.
# DMA reach is a capability. All three refusals are asserted on EVERY x86 boot, IOMMU rig or
# not: the two halves of the gate (a domain cap without WRITE, and no domain cap at all) plus
# the refusal to hand out reach on a machine with no unit to bound it. The last is the one that
# would otherwise be tempting to soften into a no-op success.
for want in \
    'dma: an IommuDomain cap without WRITE cannot grant DMA reach' \
    'role: producer cannot grant DMA reach (no domain capability at all)'; do
    if [ "${PROVOKE_FAULT:-0}" != "1" ] && [ "${ARCH:-x86_64}" = "x86_64" ] \
        && ! echo "$OUT" | grep -qF "$want"; then
        echo "RESULT: FAIL — a required assertion never ran: $want"
        exit 1
    fi
done
# Freeing a region must withdraw the device's reach, whether or not the caller unmapped first.
# The probe that exercises it has to have RUN — the scan it feeds only reports a stale entry if
# something created one, so a missing probe is a silently weaker boot, not a passing one.
if [ "${PROVOKE_FAULT:-0}" != "1" ] && [ "${ARCH:-x86_64}" = "x86_64" ] \
    && ! echo "$OUT" | grep -qE 'dma: freed a region (while the device still had it mapped|that no unit could reach anyway)'; then
    echo "RESULT: FAIL — the free-while-DMA-mapped probe never ran"
    exit 1
fi

# A REAL bus-mastering BAR reaches an untrusted process only where that device's DMA is
# bounded. Both directions are required, keyed on whether the boot bound a domain — because
# QEMU's default machine has an e1000, so a boot with no IOMMU still HAS a real bus master for
# the scan to find, and the refusal is the whole point there.
if [ "${PROVOKE_FAULT:-0}" != "1" ] && [ "${ARCH:-x86_64}" = "x86_64" ]; then
    if echo "$OUT" | grep -q '\[iommu\] domain 1 bound to'; then
        if ! echo "$OUT" | grep -q 'dev: mapped the REAL device BAR'; then
            echo "RESULT: FAIL — the device's DMA is bounded, yet its registers never reached"
            echo "  the process, or the window mapped was not the device"
            exit 1
        fi
        # And the process DRIVING it: an untrusted program commands a real bus master, and the
        # transfer lands exactly in the memory one of its own capabilities asked MAP_DMA for.
        if ! echo "$OUT" | grep -q 'dev: WE drove the device'; then
            echo "RESULT: FAIL — the process could not move data through an IOVA it was granted"
            exit 1
        fi
        if ! echo "$OUT" | grep -q 'dev: a transfer aimed at an IOVA we were never granted'; then
            echo "RESULT: FAIL — an ungranted IOVA reached the process's memory, or the check"
            echo "  for it never ran"
            exit 1
        fi
    elif ! echo "$OUT" | grep -q 'dev: no bounded device on this machine'; then
        echo "RESULT: FAIL — a real bus-mastering BAR was handed to an untrusted process on a"
        echo "  machine where nothing bounds its DMA"
        exit 1
    fi
fi

# And the outcome that depends on whether a unit exists — keyed on what the BOOT reports about
# the machine, not on what the env flags imply about it. Keying it on IOMMU=1 && FIRMWARE=1 was
# wrong: `IOMMU=1` alone also finds IVRS and enables the unit, so the run that was supposed to
# demonstrate the refusal was demonstrating the mapping, and the gate failed a working boot.
if [ "${PROVOKE_FAULT:-0}" != "1" ] && [ "${ARCH:-x86_64}" = "x86_64" ]; then
    if echo "$OUT" | grep -q 'unit ENABLED'; then
        if ! echo "$OUT" | grep -q 'dma: the device can now reach our region by DMA'; then
            echo "RESULT: FAIL — MAP_DMA did not hand out reach on a rig that HAS an IOMMU"
            exit 1
        fi
        if ! echo "$OUT" | grep -q 'dma: UNMAP_DMA withdrew it and invalidated the unit'; then
            echo "RESULT: FAIL — a DMA mapping was made and never withdrawn"
            exit 1
        fi
        # Required only where a domain EXISTS: with no unit every domain capability is refused
        # for that reason alone, and the object would never be looked at.
        # The deny entry must actually DENY, shown with the device we can drive rather than
        # asserted from the word we wrote. Nothing else in the boot aims a device at one.
        if ! echo "$OUT" | grep -q '\[iommu\] DENY WORKS:'; then
            echo "RESULT: FAIL — the entry every unbound function gets was never shown to deny,"
            echo "  or a device holding it still reached memory"
            exit 1
        fi
        # With a bridge attached, the scan must actually go THROUGH it. The read-back below
        # cannot catch this: it only checks the functions the scan found, so a walk that stops
        # at bus 0 reports "0 passed through" over a set that excludes the device it missed.
        # The bus count is the number that moves.
        if [ "${BRIDGE:-0}" = "1" ] && ! echo "$OUT" | grep -qE 'PCI function\(s\) across [2-9][0-9]* bus\(es\)'; then
            echo "RESULT: FAIL — a bridge is present but the scan never left bus 0, so whatever"
            echo "  is behind it has no device-table entry and is passed through"
            exit 1
        fi
        # Default DENY: every PCI function has a valid device-table entry. An entry with V=0 is
        # PASSTHROUGH in this unit, so a function the nucleus never enumerated has unrestricted
        # DMA while the boot claims to bound it — and the rig really does have more DMA-capable
        # functions than there are domains for.
        if ! echo "$OUT" | grep -qE '\[iommu\] [0-9]+ other function\(s\) given an EMPTY table; [0-9]+ present, 0 still passed through'; then
            echo "RESULT: FAIL — a PCI function was left without a valid device-table entry,"
            echo "  which this unit treats as passthrough"
            exit 1
        fi
        # Invalidation must name the domain it is flushing. There is no PAYLOAD oracle for the
        # second domain — its device cannot be driven from here — so the witness is the
        # emulator's own report of what the unit was told. Checked only when tracing is on,
        # which is the honest scope: with QEMU_TRACE unset this property is unverified.
        if [ -n "${QEMU_TRACE:-}" ] && [ -f "${QEMU_TRACE_FILE:-/tmp/qemu-amdvi-trace.log}" ]; then
            TR="${QEMU_TRACE_FILE:-/tmp/qemu-amdvi-trace.log}"
            for dom in '0x0' '0x1'; do
                if ! grep -qa "amdvi_pages_inval AMD-Vi pages for domain $dom" "$TR"; then
                    echo "RESULT: FAIL — no invalidation ever named domain $dom, so a domain's"
                    echo "  withdrawals were flushed under another domain's id"
                    exit 1
                fi
            done
        fi
        # Teardown must withdraw a DMA mapping the process never unmapped, BY ATTRIBUTION.
        # The demo deliberately exits holding one. A count of zero means either the probe did
        # not run or the path does not — and both look identical from the outside, which is why
        # the count is asserted rather than the absence of a complaint.
        if ! echo "$OUT" | grep -qE '\[iommu\] teardown withdrew [1-9][0-9]* DMA mapping'; then
            echo "RESULT: FAIL — a process exited holding a DMA mapping and teardown withdrew"
            echo "  nothing by attribution"
            exit 1
        fi
        # Per-device containment, both halves. The second is what makes the first mean
        # anything: a device that reaches nothing is not contained, it is broken.
        if ! echo "$OUT" | grep -q '\[iommu\] PER-DEVICE:'; then
            echo "RESULT: FAIL — a frame mapped in another device's domain was reachable, or"
            echo "  the device could not reach its own domain's mapping"
            exit 1
        fi
        if ! echo "$OUT" | grep -q "dma: the second device's domain is a separate authority"; then
            echo "RESULT: FAIL — the second domain's capability was refused, or never used"
            exit 1
        fi
        if ! echo "$OUT" | grep -q 'dma: a cap naming a domain that does not exist is refused'; then
            echo "RESULT: FAIL — a capability naming a domain that does not exist was honoured,"
            echo "  or the check for it never ran"
            exit 1
        fi
    elif ! echo "$OUT" | grep -q 'dma: no IOMMU on this machine, so MAP_DMA refuses'; then
        echo "RESULT: FAIL — with no IOMMU programmed, MAP_DMA must refuse rather than hand"
        echo "  out DMA reach that nothing bounds"
        exit 1
    fi
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
    echo "RESULT: PASS ($MODE) — saw: $EXPECT"
    exit 0
fi
echo "RESULT: FAIL ($MODE) — expected: $EXPECT"
exit 1

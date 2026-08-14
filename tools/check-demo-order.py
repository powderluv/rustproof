#!/usr/bin/env python3
"""Refuse a demo that puts a live assertion behind a deliberately-fatal instruction.

The demos contain instructions that are EXPECTED to kill their process — that is how a
denial is asserted, since a process that is allowed to do the thing simply lives and there
is no failure line for it to print. `fault_trap` is `-> !`, so every statement after such an
instruction in the same process is unreachable.

That is a loaded gun pointed at every assertion below it, and it went off: a ring-3 clock
probe was added ABOVE the FREE_REGION owner-identity check, which silently made the only
on-hardware exercise of that gate dead code. Fifteen commits shipped with the boot reporting
PASS while checking strictly less, because the runners fail on a PRINTED "(bug)" line and an
assertion that never RUNS prints nothing at all.

The runners now require a couple of specific lines, but that only guards the lines someone
remembered to list. This checks the SHAPE instead: between a fatal instruction and the end of
its process, the only thing that may print is that probe's own "we survived, which is a bug"
line. Anything else is an assertion that cannot run.

Run by tools/host-tests.sh, so it gates every CI run without needing QEMU.
"""

import re
import sys
from pathlib import Path

# An instruction placed to be fatal. Keep this list in step with the demos; a probe that is
# not listed is simply not checked, which is why the count assertion at the bottom exists.
FATAL = [
    (re.compile(r'asm!\("rdtsc"'), "rdtsc under CR4.TSD"),
    (re.compile(r'asm!\("rdcycle'), "rdcycle under scounteren=0"),
    (re.compile(r"write_volatile\(0x1u64 as \*mut u8"), "wild write to a null-ish pointer"),
]

PRINT = re.compile(r'(?:dw!|debug_write)\(\s*b"((?:[^"\\]|\\.)*)"')
EXIT = re.compile(r"\bexit\(id\)\s*;")

DEMOS = ["crates/init/src/main.rs", "crates/riscv-init/src/main.rs"]


def check(path: Path) -> list[str]:
    """Return a list of complaints for one demo file."""
    lines = path.read_text().split("\n")
    problems: list[str] = []
    found = 0

    for i, line in enumerate(lines):
        why = next((w for rx, w in FATAL if rx.search(line)), None)
        if why is None:
            continue
        found += 1

        # Walk forward to the end of this process and collect everything it would print.
        for j in range(i + 1, len(lines)):
            if EXIT.search(lines[j]):
                break
            m = PRINT.search(lines[j])
            if m and "(bug)" not in m.group(1):
                problems.append(
                    f"{path}:{j + 1}: unreachable assertion behind a fatal instruction\n"
                    f"    the {why} at {path}:{i + 1} kills this process, so this never runs:\n"
                    f'      "{m.group(1)[:72]}"\n'
                    f"    move the fatal probe BELOW this assertion, or into its own process."
                )
        else:
            problems.append(f"{path}:{i + 1}: the {why} is not followed by exit(id)")

    if found == 0:
        problems.append(
            f"{path}: no fatal probe found at all — either the demo lost its denial "
            f"assertions, or FATAL in {__file__} has gone stale and is checking nothing."
        )
    return problems


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    problems: list[str] = []
    for rel in DEMOS:
        p = root / rel
        if not p.exists():
            problems.append(f"{rel}: missing — the demo pair is no longer symmetric")
            continue
        problems += check(p)

    if problems:
        print("demo-order: a demo puts a live assertion behind a deliberately-fatal instruction")
        for p in problems:
            print(f"  {p}")
        return 1
    print(f"demo-order: ok ({len(DEMOS)} demos, no assertion behind a fatal probe)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

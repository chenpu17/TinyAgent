#!/usr/bin/env python3
"""Generate the data of the `long-output` task into the run's workdir.

    python3 setup.py <workdir>

Deterministic — the same bytes on every machine and every run: 16 log files
(`data/part_01.txt` … `part_16.txt`, about 4 KB each). `COUNTS[i]` is the
number of lines in file i that hold the standalone word SIGNAL; the rest are
ordinary log lines and near-misses (signal, SIGNALLED, RESIGNAL,
SIGNAL_COUNT) that must not be counted.

The generator is importable: the task's check.py rebuilds the data in memory
to confirm that the agent left `data/` alone and to derive the expected counts.
"""
import sys
from pathlib import Path

COUNTS = (3, 5, 0, 2, 7, 1, 4, 6, 2, 5, 3, 0, 8, 1, 4, 3)
LINES_PER_FILE = 56
CUSTOMERS = ("Harbor Works", "Northline Co", "Cedar Yard", "Blue Pier Ltd", "East Quay")
DESKS = ("north", "east", "south", "west")
# Near-misses: templates with no standalone SIGNAL. {w} worker, {g} gate, {n} number.
DECOYS = (
    "INFO  RESIGNAL from worker-{w} ignored (duplicate of an earlier signal)",
    "INFO  worker-{w} was SIGNALLED twice, coalesced into one delivery",
    "INFO  signal strength on link-{g} below threshold, flagged for review",
    "DEBUG SIGNAL_COUNT cache not refreshed for worker-{w}, using last value",
)


class Rng:
    """A tiny deterministic generator (64-bit LCG), so the data does not
    depend on the Python version's `random` module."""

    def __init__(self, seed):
        self.state = seed & ((1 << 64) - 1)

    def below(self, n):
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
        return (self.state >> 33) % n


def build():
    """Return {file name: text} for the whole data set."""
    files = {}
    for index, count in enumerate(COUNTS, 1):
        rng = Rng(0x5EED_0000 + index * 7919)
        line_no = list(range(LINES_PER_FILE))
        signal_lines = set()
        while len(signal_lines) < count:
            signal_lines.add(line_no.pop(rng.below(len(line_no))))
        decoy_slots = [i for i in line_no if i % 14 == 3]  # L4, L18, L32, L46 of every file
        lines = []
        for i in range(LINES_PER_FILE):
            day, second = 3 + (index - 1) // 4, 3600 + i * 97 + index
            stamp = f"2026-11-{day:02d} {second // 3600:02d}:{second % 3600 // 60:02d}:{second % 60:02d}"
            if i in signal_lines:
                lines.append(f"{stamp} SIGNAL gate-{rng.below(9)} reopened after {11 + rng.below(80)}s "
                             f"on worker-{rng.below(16):02d}")
            elif i in decoy_slots:
                lines.append(f"{stamp} " + DECOYS[rng.below(len(DECOYS))].format(
                    w=f"{rng.below(16):02d}", g=rng.below(9)))
            elif i % 5 == 0:
                lines.append(f"{stamp} WARN  queue backlog={rng.below(40)} oldest={rng.below(600)}s "
                             f"worker-{rng.below(16):02d} stalled, retry scheduled")
            else:
                lines.append(f"{stamp} INFO  worker-{rng.below(16):02d} order {40000 + rng.below(9999)} "
                             f"shipped to {CUSTOMERS[rng.below(len(CUSTOMERS))]} via the "
                             f"{DESKS[rng.below(len(DESKS))]} desk")
        files[f"part_{index:02d}.txt"] = "\n".join(lines) + "\n"
    return files


def main(workdir):
    data = Path(workdir) / "data"
    data.mkdir(parents=True, exist_ok=True)
    for name, text in build().items():
        (data / name).write_text(text, encoding="utf-8")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: python3 setup.py <workdir>")
    main(sys.argv[1])

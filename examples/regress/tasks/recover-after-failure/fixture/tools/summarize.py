#!/usr/bin/env python3
"""Print the number of open action items in each notes file of a directory.

Usage: python3 tools/summarize.py <notes dir>
"""
import sys

from notelib import count_actions, load_notes


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: python3 tools/summarize.py <notes dir>")
    notes_dir = sys.argv[1]
    lines = []
    total = 0
    for name, text in load_notes(notes_dir):
        count = count_actions(text)
        lines.append(f"{name}: {count} action items")
        total += count
    lines.append(f"total: {total}")
    print("\n".join(lines))


if __name__ == "__main__":
    main()

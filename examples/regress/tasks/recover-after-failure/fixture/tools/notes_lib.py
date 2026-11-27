"""Loading helpers for the notes summarizer."""
import os


def load_notes(notes_dir):
    """Return (file name, text) for every .txt file in notes_dir, sorted by name."""
    names = sorted(n for n in os.listdir(notes_dir) if n.endswith(".txt"))
    return [
        (name, open(os.path.join(notes_dir, name), encoding="utf-8").read())
        for name in names
    ]


def count_actions(text):
    """Count open action check-boxes: lines that start with `- [ ] `."""
    return sum(1 for line in text.splitlines() if line.lstrip().startswith("- [ ] "))

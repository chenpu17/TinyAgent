# Notes summarizer

`tools/summarize.py` prints, for every `.txt` file in a notes directory, how
many open action items it holds:

```sh
python3 tools/summarize.py notes
```

Expected output shape:

```
<file name>: <n> action items
...
total: <n>
```

An open action item is a line that starts with `- [ ] ` (any indentation).
Lines with `- [x]` are done and do not count.

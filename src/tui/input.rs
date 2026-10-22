//! Text input: the multi-line `InputBuffer` editor, input-history recall,
//! and the input-box horizontal-scroll math.

use unicode_width::UnicodeWidthStr;

/// Horizontal scroll for the input box: returns `(scroll_offset, caret_x)`
/// where `caret_x` is the caret's on-screen column relative to the box's
/// inner left edge, guaranteed `< inner_cols`. The input Paragraph doesn't
/// wrap, so a line wider than the box must be horizontally scrolled to keep
/// the caret visible — an unclamped caret would be drawn past the border,
/// over the conversation area (or off-screen on a narrow terminal).
pub(super) fn input_hscroll(caret_col: usize, inner_cols: usize) -> (u16, u16) {
    let inner_cols = inner_cols.max(1);
    let scroll = caret_col
        .saturating_sub(inner_cols.saturating_sub(1))
        .min(u16::MAX as usize) as u16;
    let x = caret_col
        .saturating_sub(scroll as usize)
        .min(inner_cols.saturating_sub(1))
        .min(u16::MAX as usize) as u16;
    (scroll, x)
}

/// Multi-line input editor: a vec of lines with a (row, col) cursor. `col` is
/// a byte offset kept on UTF-8 char boundaries, so insert/remove never split a
/// multi-byte character. Owned entirely by the UI thread.
pub(super) struct InputBuffer {
    lines: Vec<String>,
    pub(super) row: usize,
    pub(super) col: usize,
}

impl InputBuffer {
    pub(super) fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub(super) fn put_char(&mut self, c: char) {
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }

    /// Delete the char before the cursor; at the start of a line, join it onto
    /// the previous one.
    pub(super) fn backspace(&mut self) {
        if self.col > 0 {
            let prev_ch = self.lines[self.row][..self.col].chars().last().unwrap();
            let prev = self.col - prev_ch.len_utf8();
            self.lines[self.row].replace_range(prev..self.col, "");
            self.col = prev;
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].push_str(&cur);
        }
    }

    /// Delete the char after the cursor (Ctrl-D); at the end of a line, join
    /// the next line onto this one — the forward mirror of `backspace`.
    pub(super) fn delete_forward(&mut self) {
        if self.col < self.lines[self.row].len() {
            let next_ch = self.lines[self.row][self.col..].chars().next().unwrap();
            let end = self.col + next_ch.len_utf8();
            self.lines[self.row].replace_range(self.col..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    /// Split the current line at the cursor; cursor lands at the start of the
    /// new line.
    pub(super) fn newline(&mut self) {
        let rest = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    /// Insert a (possibly multi-line) string at the cursor — used for bracketed
    /// paste. `\n` splits into a fresh line; the CR of a CRLF pair (Windows
    /// pastes) is dropped so it doesn't leave a stray CR in the input. A lone
    /// `\r` is kept — swallowing every CR would corrupt content that uses it
    /// deliberately (e.g. progress-redraw snippets).
    ///
    /// Runs are inserted in BULK: the old per-char loop hit `put_char`, whose
    /// `String::insert` shifts the whole tail once PER CHARACTER — O(n²) on a
    /// MB-scale single-line paste (long URLs, base64, minified JSON), freezing
    /// the UI thread for minutes. Splitting on the line breaks and splicing
    /// each run with one `insert_str` makes it linear; the lines/row/col
    /// semantics are byte-identical to the per-char version (pinned by
    /// `insert_str_bulk_matches_char_by_char_reference`).
    pub(super) fn insert_str(&mut self, s: &str) {
        let mut rest = s;
        while !rest.is_empty() {
            // Bulk-splice the run up to the next line-break-ish char ('\n'
            // splits; '\r' may be half of a CRLF pair). No hit = the tail run.
            let seg_end = rest.find(['\n', '\r']).unwrap_or(rest.len());
            if seg_end > 0 {
                let line = &mut self.lines[self.row];
                line.insert_str(self.col, &rest[..seg_end]);
                self.col += seg_end;
            }
            if seg_end == rest.len() {
                break;
            }
            let after = &rest[seg_end + 1..]; // '\n' and '\r' are one byte
            if rest.as_bytes()[seg_end] == b'\n' {
                self.newline();
            } else if !after.starts_with('\n') {
                // A lone '\r' is ordinary content; the '\r' of a CRLF pair
                // is dropped here and its '\n' handled on the next turn.
                self.put_char('\r');
            }
            rest = after;
        }
    }

    pub(super) fn move_left(&mut self) {
        if self.col > 0 {
            let prev_ch = self.lines[self.row][..self.col].chars().last().unwrap();
            self.col -= prev_ch.len_utf8();
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    pub(super) fn move_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            let ch = self.lines[self.row][self.col..].chars().next().unwrap();
            self.col += ch.len_utf8();
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub(super) fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = floor_char_boundary(&self.lines[self.row], self.col);
        }
    }

    pub(super) fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = floor_char_boundary(&self.lines[self.row], self.col);
        }
    }

    pub(super) fn home(&mut self) {
        self.col = 0;
    }

    pub(super) fn end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// Move one word left: at the start of a line, to the end of the previous
    /// line; otherwise to the start of the word left of the cursor.
    pub(super) fn word_left(&mut self) {
        if self.col == 0 {
            if self.row > 0 {
                self.row -= 1;
                self.col = self.lines[self.row].len();
            }
        } else {
            self.col = word_boundary_left(&self.lines[self.row], self.col);
        }
    }

    /// Move one word right: at the end of a line, to the start of the next
    /// line; otherwise to the next word's start (leaving the current word,
    /// then skipping the whitespace after it).
    pub(super) fn word_right(&mut self) {
        if self.col == self.lines[self.row].len() {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.col = 0;
            }
        } else {
            self.col = word_boundary_right(&self.lines[self.row], self.col);
        }
    }

    /// Ctrl-W: delete the word before the cursor. At the start of a line this
    /// degrades to backspace() (joining onto the previous line).
    pub(super) fn delete_word_back(&mut self) {
        if self.col == 0 {
            self.backspace();
        } else {
            let start = word_boundary_left(&self.lines[self.row], self.col);
            self.lines[self.row].replace_range(start..self.col, "");
            self.col = start;
        }
    }

    /// Ctrl-U: delete from the line start to the cursor.
    pub(super) fn kill_to_line_start(&mut self) {
        self.lines[self.row].replace_range(..self.col, "");
        self.col = 0;
    }

    /// Ctrl-K: delete from the cursor to the line end.
    pub(super) fn kill_to_line_end(&mut self) {
        self.lines[self.row].truncate(self.col);
    }

    /// Replace the whole buffer and put the cursor at the end (history recall).
    pub(super) fn set_text(&mut self, s: &str) {
        self.lines = s.split('\n').map(String::from).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }

    /// Take the full text, leaving an empty buffer.
    pub(super) fn drain_to_string(&mut self) -> String {
        let text = self.lines.join("\n");
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        text
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub(super) fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Snapshot for rendering.
    pub(super) fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Read-only view of the logical lines, for rendering each as a separate
    /// `Line` (a single `Line` carrying embedded `\n` collapses to one visual
    /// row — ratatui's grapheme iterator filters control chars including `\n`,
    /// so a multiline buffer must be split into multiple `Line`s to render).
    pub(super) fn lines_view(&self) -> &[String] {
        &self.lines
    }

    /// (row, DISPLAY width of the text before the caret on that line). Display
    /// width — not a char count — so the caret column is correct for wide (CJK)
    /// chars, which occupy two columns each (a char count drifts one column per
    /// wide char and lands the caret mid-text).
    pub(super) fn cursor_pos(&self) -> (usize, usize) {
        (self.row, self.lines[self.row][..self.col].width())
    }
}

/// Nearest UTF-8 char boundary at or below `idx`. `col` is a byte offset kept
/// on char boundaries of the *current* line; vertical motion targets a line
/// whose multibyte layout may differ, so an offset that's valid here can land
/// mid-codepoint there — and slicing/inserting at a non-boundary
/// (`cursor`/`put_char`/`backspace`/`move_*`) panics. Flooring to a boundary
/// avoids that.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Byte offset where the word left of `col` starts: drop trailing whitespace,
/// then the run of non-whitespace before it. Words are whitespace-delimited
/// (CJK text has no spaces — a whole non-space run is one word).
pub(super) fn word_boundary_left(line: &str, col: usize) -> usize {
    let trimmed = line[..col].trim_end();
    trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// Recall the previous (older) history entry into the buffer, saving the
/// current text as `draft` on the first recall so Down past the newest
/// restores it. Shared by plain Up (single-line buffer or already browsing)
/// and Alt+Up (always).
pub(super) fn history_up(
    history: &[String],
    hist_idx: &mut Option<usize>,
    draft: &mut String,
    input: &mut InputBuffer,
) {
    if history.is_empty() {
        return;
    }
    match *hist_idx {
        None => {
            *draft = input.text();
            *hist_idx = Some(history.len() - 1);
        }
        Some(0) => {}
        Some(i) => *hist_idx = Some(i - 1),
    }
    if let Some(i) = *hist_idx {
        input.set_text(&history[i]);
    }
}

/// Recall the next (newer) history entry; past the newest restores the draft
/// and stops browsing.
pub(super) fn history_down(
    history: &[String],
    hist_idx: &mut Option<usize>,
    draft: &str,
    input: &mut InputBuffer,
) {
    if let Some(i) = *hist_idx {
        if i + 1 < history.len() {
            *hist_idx = Some(i + 1);
            input.set_text(&history[i + 1]);
        } else {
            *hist_idx = None;
            input.set_text(draft);
        }
    }
}

/// Whether plain Up recalls history (vs moving the caret across lines): the
/// readline/zsh convention — a SINGLE-line buffer has no lines to cross, and
/// an active browse keeps walking history. A multi-line buffer keeps the
/// caret motion (recalling would clobber the multi-line draft).
pub(super) fn up_is_history(single_line: bool, browsing: bool) -> bool {
    single_line || browsing
}

/// Byte offset where the next word right of `col` starts: leave the current
/// word, then skip the whitespace after it.
pub(super) fn word_boundary_right(line: &str, col: usize) -> usize {
    let rest = &line[col..];
    let word_len = rest
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let after = &rest[word_len..];
    col + word_len + (after.len() - after.trim_start().len())
}

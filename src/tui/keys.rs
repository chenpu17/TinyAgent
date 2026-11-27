//! Keyboard → session state: the popup's first pick, the cancel / quit chords,
//! Enter (a message or a slash command — see `commands`), line editing and
//! history recall. Pure state transitions: the loop polls crossterm, hands the
//! key over and acts on the returned [`Effect`], so every key path is testable
//! without a terminal.

use super::input::{history_down, history_up, up_is_history};
use super::markdown::paste_plain;
use super::session::{RunPhase, Session};
use super::slash::{popup_key, PopupAction};
use super::transcript::{latest_detail, Row};
use crate::config::Config;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

/// What the loop does after a key was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// Keep looping.
    None,
    /// Leave the loop: the worker has already been told to stop. The loop
    /// returns how many queued messages it had to leave behind.
    Quit,
}

/// How long a first idle Ctrl-C arms the quit confirmation. A single press
/// in idle only shows the "press again" notice — the busy-Ctrl-C muscle
/// memory firing right after a run ends must not kill the session (the same
/// accident that got Esc-as-quit removed).
const CTRL_C_QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Whether a second idle Ctrl-C (at `now`) confirms the quit. None armed (or
/// past the window) means this press only arms.
pub(super) fn ctrl_c_should_quit(armed: Option<Instant>, now: Instant) -> bool {
    armed.is_some_and(|t| now.duration_since(t) <= CTRL_C_QUIT_WINDOW)
}

impl Session {
    /// Handle one key event. Keys are taken in a fixed order of precedence:
    /// releases are dropped; an open slash popup answers first; then the
    /// cancel chords, Enter, and the editing keys (`on_edit_key`).
    pub(super) fn on_key(&mut self, key: KeyEvent, cfg: &Config) -> Effect {
        // Accept key repeats too: on Windows, crossterm reports held
        // keys as Press once then Repeat — filtering to Press only
        // makes held Backspace/arrows delete/move a single char.
        // Release events are still ignored (they'd double-fire).
        if key.kind == KeyEventKind::Release {
            return Effect::None;
        }
        // The slash popup gets first crack while open (`on_popup_key`).
        if self.composer.popup.is_open() && self.on_popup_key(key) {
            return Effect::None;
        }
        match (key.code, key.modifiers) {
            // Esc / Ctrl-C: cancel the in-flight turn and stay in the
            // TUI. When idle the two now DIVERGE: Esc clears the input
            // box (it quit before, which was easy to hit by accident
            // right after a run — quit via /quit or /exit now), while
            // Ctrl-C quits on a SECOND press within 2s (same accident
            // profile as Esc had: the busy-cancel muscle memory firing
            // the instant a run ends). raw mode turns Ctrl-C into a
            // normal key. The transcript lives in native scrollback,
            // so quitting just releases the live region — the
            // conversation stays selectable above it.
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                return self.on_cancel(key.code)
            }
            // Alt/Shift+Enter inserts a newline (portable in raw mode;
            // every terminal reports KeyModifiers::SHIFT truthfully).
            (KeyCode::Enter, KeyModifiers::ALT) | (KeyCode::Enter, KeyModifiers::SHIFT) => {
                self.composer.input.newline();
            }
            // Enter sends (queues while busy), runs a slash command, or
            // continues the line after a trailing `\` — see `on_enter`.
            // The input stays editable during a run.
            (KeyCode::Enter, _) => return self.on_enter(cfg),
            (code, mods) => return self.on_edit_key(code, mods),
        }
        Effect::None
    }

    /// The popup's pick while it is open: Up/Down move its selection instead
    /// of the caret, Tab/Enter complete WITHOUT sending, Esc closes it WITHOUT
    /// quitting. Modifier chords and every other key fall through to the
    /// normal handlers untouched. Returns whether the key was fully handled.
    ///
    /// One refinement to "Enter completes": when the typed text already IS
    /// the completion there is nothing to complete, so Enter keeps its
    /// ordinary meaning and the fully typed command runs on this press. The
    /// safety the two-step rule buys is for an ABBREVIATION (`/cl` + Enter
    /// must not run `/clear` sight unseen) — a command typed out in full was
    /// meant. Argument-taking commands are never "already complete": their
    /// completion adds the trailing space, so Enter still only completes.
    fn on_popup_key(&mut self, key: KeyEvent) -> bool {
        match popup_key(&mut self.composer.popup, key.code, key.modifiers) {
            Some(PopupAction::Consumed) => true,
            Some(PopupAction::Dismiss) => {
                self.composer.popup.dismiss(&self.composer.input.text());
                true
            }
            Some(PopupAction::Complete(c)) => {
                let already_complete =
                    key.code == KeyCode::Enter && c == self.composer.input.text();
                if !already_complete {
                    self.composer.input.set_text(&c);
                }
                // Closed on this text either way: a refused command keeps
                // its text, and the popup must not pop back open on it.
                self.composer.popup.dismiss(&c);
                !already_complete
            }
            None => false,
        }
    }

    /// Detail and line-editing shortcuts (the slice carries the comments).
    fn on_edit_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Effect {
        let composer = &mut self.composer;
        match (code, mods) {
            // Detail and line-editing shortcuts. Word motions must
            // precede plain arrows; Ctrl chords precede plain chars.
            (KeyCode::Char('o'), KeyModifiers::CONTROL) => match latest_detail(&self.rows) {
                Some(row) => self.rows.push(row),
                None => self.rows.push(Row::sys_info(
                    "[no tool call, result or thinking detail yet]",
                )),
            },
            (KeyCode::Char('j'), KeyModifiers::CONTROL) => composer.input.newline(),
            (KeyCode::Left, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
            {
                composer.input.word_left()
            }
            (KeyCode::Right, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
            {
                composer.input.word_right()
            }
            (KeyCode::Backspace, KeyModifiers::CONTROL) => composer.input.delete_word_back(),
            (KeyCode::Delete, _) => composer.input.delete_forward(),
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => composer.input.delete_word_back(),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => composer.input.kill_to_line_start(),
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => composer.input.kill_to_line_end(),
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => composer.input.home(),
            (KeyCode::Char('e'), KeyModifiers::CONTROL) => composer.input.end(),
            // Ctrl-D: delete forward at the caret (the backspace
            // mirror); on an EMPTY idle input it quits instead — the
            // terminal-tool convention. Empty + busy is a no-op:
            // nothing to delete, and quitting mid-run stays on /quit
            // or the double-Ctrl-C confirmation.
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                if !composer.input.is_empty() {
                    composer.input.delete_forward();
                } else if !self.run.phase.busy() {
                    let _ = self.in_tx.send(None);
                    return Effect::Quit;
                }
            }
            (KeyCode::Backspace, _) => composer.input.backspace(),
            (KeyCode::Left, _) => composer.input.move_left(),
            (KeyCode::Right, _) => composer.input.move_right(),
            // History recall. Alt+Up/Down ALWAYS recall; plain Up/Down
            // recall when the buffer is single-line (or a browse is
            // already active) and move the caret across lines otherwise
            // — the readline/zsh convention, so the empty-input Up that
            // every terminal user reaches for just works.
            (KeyCode::Up, KeyModifiers::ALT) => {
                history_up(
                    &composer.history,
                    &mut composer.hist_idx,
                    &mut composer.draft,
                    &mut composer.input,
                );
            }
            (KeyCode::Down, KeyModifiers::ALT) => {
                history_down(
                    &composer.history,
                    &mut composer.hist_idx,
                    &composer.draft,
                    &mut composer.input,
                );
            }
            (KeyCode::Up, _) => {
                if up_is_history(
                    composer.input.line_count() == 1,
                    composer.hist_idx.is_some(),
                ) {
                    history_up(
                        &composer.history,
                        &mut composer.hist_idx,
                        &mut composer.draft,
                        &mut composer.input,
                    );
                } else {
                    composer.input.move_up();
                }
            }
            (KeyCode::Down, _) => {
                if composer.hist_idx.is_some() {
                    history_down(
                        &composer.history,
                        &mut composer.hist_idx,
                        &composer.draft,
                        &mut composer.input,
                    );
                } else {
                    composer.input.move_down();
                }
            }
            (KeyCode::Home, _) => composer.input.home(),
            (KeyCode::End, _) => composer.input.end(),
            // PageUp/PageDown are intentionally not handled: with mouse
            // capture off, the terminal owns scrollback paging (and the
            // wheel). There is no app-managed transcript to page.
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                composer.input.put_char(c);
            }
            _ => {}
        }
        Effect::None
    }

    /// Esc / Ctrl-C (see `on_key` for the reasoning): `code` tells the two
    /// apart once idle.
    fn on_cancel(&mut self, code: KeyCode) -> Effect {
        if self.run.phase.busy() {
            self.cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if self.run.phase == RunPhase::Running {
                self.run.begin_cancelling();
                self.rows.push(Row::sys_warn("[cancelling…]"));
                if !self.queued.is_empty() {
                    self.rows.push(Row::sys_warn(format!(
                        "[dropped {} queued message(s)]",
                        self.queued.len()
                    )));
                    self.queued.clear();
                    // The pause is a property of held
                    // messages: nothing held, nothing paused.
                    self.queue_paused = false;
                }
            }
            // Already Cancelling: keep the signal latched but
            // do not append another row on key repeat.
        } else if code == KeyCode::Char('c') {
            // Ctrl-C in idle: first press arms (notice row),
            // second within the window quits. /quit and /exit
            // stay single-shot.
            if ctrl_c_should_quit(self.composer.ctrl_c_armed, Instant::now()) {
                let _ = self.in_tx.send(None);
                return Effect::Quit;
            }
            self.composer.ctrl_c_armed = Some(Instant::now());
            self.rows
                .push(Row::sys_info("[press Ctrl-C again to quit]"));
        } else {
            // Esc in idle: clear the input box instead of
            // quitting. Quit via /quit or /exit.
            self.composer.input.drain_to_string();
        }
        Effect::None
    }

    /// Bracketed paste: the whole blob (newlines included) arrives
    /// as one event and is inserted as input — not split into many
    /// Enter-triggered sends the way a raw paste would be. Escape
    /// sequences go whole and \n / \t survive (`paste_plain`).
    pub(super) fn on_paste(&mut self, s: &str) {
        self.composer.input.insert_str(&paste_plain(s));
    }
}

//! Enter: a message (sent now, or queued behind the run in flight) or a slash
//! command. Slash commands are handled in the UI and never sent to the model;
//! the few that need the agent (`/clear`, `/context`, `/compact`) become
//! `Input` messages for the worker. Every path ends in rows and state only.
//! Commands are NOT recorded in input history, so Alt+Up recalls messages.

use super::keys::Effect;
use super::render::{copy_to_clipboard, strip_controls};
use super::session::{pause_for_held, Session};
use super::slash::{classify, slash_reply, SlashCmd};
use super::transcript::{
    nth_last_thinking, nth_last_tool_call, nth_last_tool_result, render_transcript, Row, SysLevel,
};
use super::worker::Input;
use crate::config::Config;

/// Expand a leading `~/` (and a bare `~`) to `$HOME`, for `/export ~/notes.md`.
/// Only the current user's home — `~other` is left as-is (rare; not needed
/// here). `_with` takes home explicitly so the logic is unit-testable without
/// mutating the process-wide `$HOME`; `expand_tilde` binds it to the env.
pub(super) fn expand_tilde_with(path: &str, home: Option<&std::ffi::OsStr>) -> String {
    let home = match home {
        Some(h) => h,
        None => return path.to_string(),
    };
    if path == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let mut s = home.to_string_lossy().into_owned();
        s.push('/');
        s.push_str(rest);
        return s;
    }
    path.to_string()
}

fn expand_tilde(path: &str) -> String {
    expand_tilde_with(path, std::env::var_os("HOME").as_deref())
}

impl Session {
    /// Enter in a non-empty composer.
    pub(super) fn on_enter(&mut self, cfg: &Config) -> Effect {
        // A trailing `\` on the last line turns Enter into a newline (bash line
        // continuation) — the one multiline entry path terminals that don't
        // report Shift/Alt+Enter modifiers (macOS Terminal.app) can rely on.
        if self
            .composer
            .input
            .lines_view()
            .last()
            .is_some_and(|l| l.ends_with('\\'))
        {
            let input = &mut self.composer.input;
            input.end();
            input.backspace(); // drop the backslash
            input.newline();
            return Effect::None;
        }
        // Strip control chars once at submission, so what enters rows / input
        // history / the wire is clean.
        if self.composer.input.is_empty() {
            return Effect::None;
        }
        let trimmed = strip_controls(self.composer.input.text().trim());
        if trimmed.is_empty() {
            return Effect::None;
        }
        let cmd = classify(&trimmed);
        if self.run.phase.busy() {
            if let Some(why) = refused_while_busy(&cmd) {
                self.refuse_command(why);
                return Effect::None;
            }
        }
        // Every executing arm starts with `take_input`: a command leaves the
        // input box and is NOT recorded in input history. Two refusals keep
        // the text for correction — the "disabled while running" ones above,
        // and an unknown command below.
        match cmd {
            SlashCmd::Message => self.send_message(trimmed),
            SlashCmd::Clear => {
                // Already-committed scrollback is left in place (it's history
                // the user can still scroll to); only the live buffer resets.
                self.take_input();
                self.slash_clear()
            }
            SlashCmd::Context => {
                self.take_input();
                if self.in_tx.send(Some(Input::Context)).is_err() {
                    self.rows.push(Row::sys_error(
                        "[worker stopped — context report unavailable]",
                    ));
                }
            }
            SlashCmd::Compact(focus) => {
                self.take_input();
                if self.in_tx.send(Some(Input::Compact(focus))).is_err() {
                    self.rows
                        .push(Row::sys_error("[worker stopped — compact unavailable]"));
                } else {
                    // The summary is a real, billed model request — busy like
                    // a turn, so Esc cancels it and typed messages queue behind.
                    self.run.start("compacting", false);
                }
            }
            // /quit (or /exit): leave the TUI. Works mid-run too — the quit
            // path cancels the in-flight turn and tears down. Esc in idle no
            // longer quits, so this (or Ctrl-C) is the intentional way out.
            SlashCmd::Quit => {
                self.take_input();
                let _ = self.in_tx.send(None);
                return Effect::Quit;
            }
            SlashCmd::Export(arg) => {
                self.take_input();
                self.slash_export(arg, &trimmed)
            }
            SlashCmd::Queue => {
                self.take_input();
                if self.queued.is_empty() {
                    self.rows.push(Row::sys_info("[queue empty]"));
                } else {
                    for (i, text) in self.queued.iter().enumerate() {
                        self.rows.push(Row::sys_info(format!(
                            "[queued #{} · not sent]\n{text}",
                            i + 1
                        )));
                    }
                }
            }
            SlashCmd::Unqueue => {
                self.take_input();
                match self.queued.pop_back() {
                    Some(text) => {
                        self.composer.input.set_text(&text);
                        self.composer.hist_idx = None;
                        self.composer.draft.clear();
                        // Withdrawing the last held message lifts the pause.
                        self.queue_paused = pause_for_held(self.queue_paused, self.queued.len());
                    }
                    None => self
                        .rows
                        .push(Row::sys_warn("[queue empty — nothing to withdraw]")),
                }
            }
            // While a run is in flight the command explains itself instead.
            SlashCmd::Resume if self.run.phase.busy() => {
                self.take_input();
                self.rows.push(Row::sys_warn(
                    "[busy — /resume continues a paused queue after the run]",
                ));
            }
            SlashCmd::Resume => {
                self.take_input();
                if !self.queue_paused && self.queued.is_empty() {
                    self.rows.push(Row::sys_info("[no paused queue]"));
                } else {
                    self.queue_paused = false;
                    let flush = self.flush_next_queued();
                    self.apply_flush(flush);
                }
            }
            SlashCmd::Call(arg) => {
                self.take_input();
                self.recall_nth(
                    arg,
                    &trimmed,
                    "tool call",
                    "call(s)",
                    nth_last_tool_call,
                    |r| matches!(r, Row::ToolCall { .. }),
                );
            }
            SlashCmd::Thinking(arg) => {
                self.take_input();
                self.recall_nth(
                    arg,
                    &trimmed,
                    "thinking block",
                    "block(s)",
                    nth_last_thinking,
                    |r| matches!(r, Row::Thinking { .. }),
                );
            }
            SlashCmd::Out(arg) => {
                self.take_input();
                self.recall_nth(
                    arg,
                    &trimmed,
                    "tool result",
                    "result(s)",
                    nth_last_tool_result,
                    |r| matches!(r, Row::ToolResult { .. }),
                );
            }
            cmd @ SlashCmd::Unknown(_) => {
                // Correctable input stays in the composer.
                self.rows.push(Row::sys_warn(slash_reply(&cmd, cfg)))
            }
            cmd => {
                self.take_input();
                self.rows.push(Row::sys_info(slash_reply(&cmd, cfg)))
            }
        }
        Effect::None
    }

    /// Empty the composer (a command is being run).
    fn take_input(&mut self) {
        self.composer.input.drain_to_string();
    }

    /// A command refused because a run is in flight: a warning row, and the
    /// command stays in the composer for an idle retry.
    fn refuse_command(&mut self, text: &str) {
        self.rows.push(Row::sys_warn(text));
    }

    /// A plain message: queued behind a run in flight, or sent now.
    fn send_message(&mut self, trimmed: String) {
        if self.composer.history.last().map(String::as_str) != Some(&trimmed) {
            self.composer.history.push(trimmed.clone());
        }
        self.composer.hist_idx = None;
        self.composer.draft.clear();
        self.take_input();
        if self.run.phase.busy() {
            // Queue behind the in-flight run. Pending text stays in the FIFO;
            // only its preview is drawn live.
            self.queued.push_back(trimmed);
        } else {
            self.rows.push(Row::User(trimmed.clone()));
            // Deliver before flipping busy. If the worker is gone (panicked
            // mid-run in a debug build, or tearing down) the send fails — don't
            // leave the UI stuck in "running" waiting for a RunFinished that
            // will never arrive.
            if self.in_tx.send(Some(Input::Text(trimmed))).is_err() {
                self.run.fail();
                // The message never reached the agent — drop it from the
                // transcript so the on-screen history matches what the agent
                // actually saw.
                if matches!(self.rows.last(), Some(Row::User(_))) {
                    self.rows.pop();
                }
                self.rows.push(Row::sys_error(
                    "[worker stopped — message not delivered; type /quit to exit]",
                ));
            } else {
                self.run.start("thinking", true);
            }
        }
    }

    /// `/clear`: reset the live buffer (the composer is already empty).
    fn slash_clear(&mut self) {
        self.rows.clear();
        self.md_cache.clear();
        self.committed = 0;
        self.usage.in_tokens = 0;
        self.usage.out_tokens = 0;
        self.usage.cache_create = 0;
        self.usage.cache_read = 0;
        // Status-bar state follows the cleared history: a stale ctx% or
        // plan done/total segment would otherwise linger until the next
        // Usage / TodoUpdate event happens to overwrite it (external
        // re-review).
        self.usage.ctx_tokens = 0;
        self.plan.clear();
        // Send before claiming success. If the worker is gone (panicked
        // mid-run in a debug build, or tearing down) the send fails and the
        // agent-side history is NOT cleared — report that honestly, mirroring
        // the text path. The local UI reset above still applies.
        let cleared = self.in_tx.send(Some(Input::Clear)).is_ok();
        self.rows.push(if cleared {
            Row::sys_info("[history cleared]")
        } else {
            Row::sys_error("[worker stopped — UI cleared but session history not cleared]")
        });
    }

    /// `/export [path]`: the conversation as text — to that file, or (no
    /// path) the clipboard if a tool is available, else a default file. Native
    /// selection works too; /export stays for the machine-readable
    /// whole-transcript dump.
    fn slash_export(&mut self, arg: Option<String>, trimmed: &str) {
        let n = self.rows.len();
        let text = render_transcript(&self.rows);
        let row = match arg {
            Some(path) => {
                use std::fs::OpenOptions;
                use std::io::Write;
                // Expand `~/` so /export ~/notes.md lands in the
                // home dir. create_new refuses to clobber an
                // existing file: the TUI has no undo, so a typo'd
                // path (e.g. ~/.zshrc) would otherwise truncate
                // real content silently. (The clipboard-less
                // fallback below keeps overwriting its own scratch
                // file on purpose.)
                let dest = expand_tilde(&path);
                let res = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)
                    .and_then(|mut f| f.write_all(text.as_bytes()));
                match res {
                    Ok(_) => Row::sys_info(format!("[exported {n} rows to {dest}]")),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        Row::sys_error(format!(
                            "[export to {dest} failed: file already exists — /export won't \
                             overwrite; remove it or choose another path]"
                        ))
                    }
                    Err(e) => Row::sys_error(format!("[export to {dest} failed: {e}]")),
                }
            }
            None => {
                if copy_to_clipboard(&text) {
                    Row::sys_info(format!("[copied {n} rows to clipboard]"))
                } else {
                    let path = "tinyagent-export.md";
                    match std::fs::write(path, &text) {
                        Ok(_) => Row::sys_info(format!(
                            "[no clipboard tool found; exported {n} rows to {path}]"
                        )),
                        Err(e) => Row::sys_error(format!("[export failed: {e}]")),
                    }
                }
            }
        };
        // A failed export is correctable (another path): the command goes
        // back into the composer.
        if matches!(
            row,
            Row::System {
                level: SysLevel::Error,
                ..
            }
        ) {
            self.composer.input.set_text(trimmed);
        }
        self.rows.push(row);
    }

    /// `/call N`, `/thinking N`, `/out N`: re-print the Nth-last row of the
    /// command's kind, or say how many exist and keep the command in the
    /// composer. `nth_last` and `is_kind` pick the kind.
    fn recall_nth(
        &mut self,
        arg: Option<usize>,
        trimmed: &str,
        what: &str,
        unit: &str,
        nth_last: fn(&[Row], usize) -> Option<Row>,
        is_kind: fn(&Row) -> bool,
    ) {
        let found = nth_last(&self.rows, arg.unwrap_or(1));
        let total = self.rows.iter().filter(|r| is_kind(r)).count();
        match found {
            Some(r) => self.rows.push(r),
            None => {
                self.rows.push(Row::sys_warn(format!(
                    "[no {what} #{} — {total} {unit} so far]",
                    arg.unwrap_or(1)
                )));
                self.composer.input.set_text(trimmed);
            }
        }
    }
}

/// The "disabled while running" answer for the commands a run in flight
/// forbids — each needs the worker between runs (it pumps its inbox only
/// then), and a clear or a fold would rewrite the very history the active
/// run is working on.
fn refused_while_busy(cmd: &SlashCmd) -> Option<&'static str> {
    match cmd {
        SlashCmd::Clear => Some("[busy — /clear is disabled while running]"),
        SlashCmd::Context => Some("[busy — /context is disabled while running]"),
        SlashCmd::Compact(_) => Some("[busy — /compact is disabled while running]"),
        _ => None,
    }
}

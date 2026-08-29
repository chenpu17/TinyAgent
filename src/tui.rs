use crate::agent::{Agent, AgentEvent};
use crate::config::Config;
use crate::provider::Block;
use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::ExecutableCommand;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as WBlock, BorderType, Borders, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use std::io::stdout;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// Braille spinner frames for the activity shown in the input border title.
/// The frame is derived from the run's elapsed time (100ms per frame), not
/// stored state.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

// Theme: the one place history-row / chrome colors are defined.
const USER_FG: Color = Color::Cyan;
const AGENT_FG: Color = Color::Green;
const TOOL_CALL_FG: Color = Color::Yellow;
const TOOL_OK_FG: Color = Color::Green;
const TOOL_ERR_FG: Color = Color::Red;
const DIM_FG: Color = Color::Gray;

// Markdown header emphasis by level: h1 pops, h2/h3 steady, deeper recedes.
const MD_H1_FG: Color = Color::Yellow;
const MD_H23_FG: Color = Color::Cyan;
const MD_DEEP_FG: Color = Color::DarkGray;

// Status-bar model badge: black on cyan (echoes USER_FG as a solid block).
const BADGE_FG: Color = Color::Black;
const BADGE_BG: Color = Color::Cyan;

/// Keep prose readable on wide terminals while leaving the reading column
/// left-anchored. Input and status chrome use the available terminal width.
const CONTENT_MAX_WIDTH: u16 = 120;

/// Height of the inline viewport (the live band at the bottom of the primary
/// screen). Splits into a transcript/streaming zone (top), the input box, and
/// a one-row status bar. Tall enough that the slash popup and a few streaming
/// lines fit; the host terminal scrolls once committed content outgrows it.
const INLINE_HEIGHT: u16 = 8;

/// Body indent beneath the standalone conversation role headers.
const ROLE_BODY_INDENT: usize = 2;
const TOOL_INDENT: usize = 4;

/// Severity of a `Row::System` notice — drives its color (info dim, warn
/// yellow, error red). The text keeps its `[warn]`/`[error]`-style prefix, so
/// the plain-text /export transcript stays self-describing without the level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SysLevel {
    Info,
    Warn,
    Error,
}

/// One rendered row in the message history.
enum Row {
    User(String),
    /// A user message queued behind an in-flight run: rendered as a dim
    /// `❯ … · queued` ghost. Removed (and replaced by the real Row::User)
    /// when the queue flush actually sends it; dropped if the queue is.
    Queued(String),
    Assistant(String),
    /// Tool call: collapsed shows a compact one-line arg summary; expanded
    /// pretty-prints the full input JSON. In the inline model rows commit to
    /// scrollback at their default state, so `expanded` stays at its initial
    /// `false` (a one-line summary) — there's no in-app cursor to toggle it
    /// post-commit. `collapsed` caches the compact arg summary at creation so
    ToolCall {
        name: String,
        input: serde_json::Value,
        collapsed: String,
        expanded: bool,
        started: Instant,
    },
    /// Tool result: collapsed shows a truncated preview; expanded shows the
    /// full output (which can be large, but participates in normal scrolling).
    /// `collapsed` caches the preview at creation (like ToolCall does) so the
    /// renderer doesn't re-truncate the content every frame. `elapsed` is the
    /// pre-formatted duration of the matching call ("1.2s"), None when no
    /// same-name ToolCall row was found (e.g. a resumed session).
    ToolResult {
        name: String,
        content: String,
        collapsed: String,
        is_error: bool,
        expanded: bool,
        elapsed: Option<String>,
    },
    System {
        text: String,
        level: SysLevel,
    },
}

impl Row {
    /// Convenience constructors so the many System-row sites name a level
    /// without repeating the struct literal.
    fn sys(text: impl Into<String>, level: SysLevel) -> Row {
        Row::System {
            text: text.into(),
            level,
        }
    }
    fn sys_info(text: impl Into<String>) -> Row {
        Row::sys(text, SysLevel::Info)
    }
    fn sys_warn(text: impl Into<String>) -> Row {
        Row::sys(text, SysLevel::Warn)
    }
    fn sys_error(text: impl Into<String>) -> Row {
        Row::sys(text, SysLevel::Error)
    }
}

/// What a `/`-prefixed input line asks for. `Message` = not a command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlashCmd {
    Message,
    Clear,
    /// `/export [path]` — dump the conversation to a file (or clipboard when no
    /// path is given and a clipboard tool is available). The optional arg is the
    /// destination path, whitespace-trimmed.
    Export(Option<String>),
    Model,
    Limits,
    Tools,
    System,
    Help,
    /// `/quit` (or `/exit`): leave the TUI. Always quits, even mid-run — the
    /// quit path cancels the in-flight turn and tears down. Esc in idle no
    /// longer quits, so this is the intentional way out (plus Ctrl-C).
    Quit,
    Unknown(String),
}

/// Classify a trimmed input line into a slash command (or Message). Pure — no
/// side effects — so the command grammar is unit-tested independently of the
/// terminal/key-handling. Case-sensitive: `/Model` is unknown, not `/model`.
/// The command word is split from its argument on the first whitespace, so
/// `/export out.md` carries `Some("out.md")`.
fn classify(trimmed: &str) -> SlashCmd {
    let Some(rest) = trimmed.strip_prefix('/') else {
        return SlashCmd::Message;
    };
    let (word, raw_arg) = match rest.split_once(char::is_whitespace) {
        Some((w, a)) => (w, a.trim()),
        None => (rest.trim(), ""),
    };
    let arg = if raw_arg.is_empty() {
        None
    } else {
        Some(raw_arg.to_string())
    };
    // Only /export takes an argument. After the classify refactor the other
    // commands silently dropped a trailing arg (`/model foo` ran /model with
    // "foo" ignored); reject them as unknown so the typo is surfaced. Unknown
    // carries the full `rest` so the message names what was actually typed.
    let stray_arg = arg.is_some();
    match word {
        "clear" if !stray_arg => SlashCmd::Clear,
        "export" => SlashCmd::Export(arg),
        "model" if !stray_arg => SlashCmd::Model,
        "limits" if !stray_arg => SlashCmd::Limits,
        "tools" if !stray_arg => SlashCmd::Tools,
        "system" if !stray_arg => SlashCmd::System,
        "help" if !stray_arg => SlashCmd::Help,
        "quit" | "exit" if !stray_arg => SlashCmd::Quit,
        _ => SlashCmd::Unknown(rest.to_string()),
    }
}

/// Read-only reply text for an info slash command (everything but Clear, which
/// has its own "[history cleared]" side effect). Pure given the config.
fn slash_reply(cmd: &SlashCmd, cfg: &Config) -> String {
    match cmd {
        SlashCmd::Model => format!(
            "model {} · provider {:?} · max_tokens {} · base {}",
            cfg.model,
            cfg.provider,
            cfg.max_tokens,
            cfg.base_url.as_deref().unwrap_or("(default)"),
        ),
        SlashCmd::Limits => format!(
            "limits: max_turns {} · timeout_secs {}",
            cfg.limits.max_turns, cfg.limits.timeout_secs,
        ),
        SlashCmd::Tools => format!(
            "tools: {}",
            if cfg.tools.is_empty() {
                "(none)".to_string()
            } else {
                cfg.tools.join(", ")
            },
        ),
        SlashCmd::System => format!(
            "system: {}",
            cfg.system_prompt.as_deref().unwrap_or("(none)"),
        ),
        // Generated from SLASH_COMMANDS — the single table the completion
        // popup also reads — so a new command updates /help and the popup
        // together. Column width follows the longest "/name [arg]".
        SlashCmd::Help => {
            let lhs = |c: &SlashSpec| {
                if c.arg_hint.is_empty() {
                    format!("/{}", c.name)
                } else {
                    format!("/{} {}", c.name, c.arg_hint)
                }
            };
            let kw = SLASH_COMMANDS
                .iter()
                .map(|c| lhs(c).len())
                .max()
                .unwrap_or(0);
            let mut s = "commands:".to_string();
            for c in &SLASH_COMMANDS {
                s.push_str(&format!("\n  {:<kw$}  {}", lhs(c), c.desc));
            }
            s
        }
        SlashCmd::Unknown(name) => format!("[unknown command: /{name} — try /help]"),
        // Message/Clear/Export never reach here; empty keeps the match
        // exhaustive. Export and Clear have their own side-effecting arms in
        // the key handler.
        SlashCmd::Message | SlashCmd::Clear | SlashCmd::Export(_) | SlashCmd::Quit => String::new(),
    }
}

// ── Slash-command completion popup ────────────────────────────────────

/// One slash command as the completion popup shows it. KEEP IN SYNC with
/// `classify` above — that match is the actual command set; this table only
/// describes it (name, argument hint, one-line description).
struct SlashSpec {
    name: &'static str,
    arg_hint: &'static str,
    desc: &'static str,
}

const SLASH_COMMANDS: [SlashSpec; 8] = [
    SlashSpec {
        name: "clear",
        arg_hint: "",
        desc: "clear the conversation history",
    },
    SlashSpec {
        name: "export",
        arg_hint: "[path]",
        desc: "dump the transcript to a file (or clipboard)",
    },
    SlashSpec {
        name: "model",
        arg_hint: "",
        desc: "model, provider, max_tokens, base URL",
    },
    SlashSpec {
        name: "limits",
        arg_hint: "",
        desc: "turn and timeout limits",
    },
    SlashSpec {
        name: "tools",
        arg_hint: "",
        desc: "enabled tool list",
    },
    SlashSpec {
        name: "system",
        arg_hint: "",
        desc: "the active system prompt",
    },
    SlashSpec {
        name: "help",
        arg_hint: "",
        desc: "list commands",
    },
    SlashSpec {
        name: "quit",
        arg_hint: "",
        desc: "exit the TUI (alias: /exit)",
    },
];

/// Popup height cap; the match window scrolls to keep the selection visible.
const POPUP_MAX_ROWS: usize = 8;

/// Floating slash-command completion state. Pure state machine, no terminal:
/// `update` recomputes the visible matches from the input text (empty list =
/// closed), the key handling lives in `popup_key`. No mouse support —
/// click-to-complete buys little over Tab/Enter.
struct SlashPopup {
    /// The `/`-stripped prefix the current matches were filtered with;
    /// selection resets whenever it changes.
    prefix: String,
    matches: Vec<&'static SlashSpec>,
    sel: usize,
    /// Text a dismiss (Esc, or a completion just applied) was taken on. The
    /// popup stays closed while the buffer still equals it — otherwise
    /// `update` would reopen on the very next frame, making Esc/completion
    /// unable to close the popup at all. Any further edit lifts it.
    dismissed: Option<String>,
}

impl SlashPopup {
    fn new() -> Self {
        Self {
            prefix: String::new(),
            matches: Vec::new(),
            sel: 0,
            dismissed: None,
        }
    }

    /// Recompute from the input text. Open only while the user is typing the
    /// command NAME: the buffer starts with `/` and contains no space yet
    /// (a space means the argument stage, e.g. the /export path). Bare `/`
    /// lists everything; a non-empty prefix filters case-sensitively, like
    /// classify (`/Model` matches nothing). Zero matches = closed.
    fn update(&mut self, text: &str) {
        if self.dismissed.as_deref() == Some(text) {
            return; // dismissed on exactly this text: stay closed
        }
        self.dismissed = None;
        let matches: Vec<&'static SlashSpec> = text
            .strip_prefix('/')
            .filter(|rest| !rest.contains(' '))
            .map(|p| {
                SLASH_COMMANDS
                    .iter()
                    .filter(|c| c.name.starts_with(p))
                    .collect()
            })
            .unwrap_or_default();
        if matches.is_empty() {
            self.matches.clear();
            self.prefix.clear();
            self.sel = 0;
            return;
        }
        let p = text.strip_prefix('/').unwrap_or("");
        if p != self.prefix {
            self.sel = 0; // the filter changed; restart at the top
        }
        self.prefix = p.to_string();
        self.sel = self.sel.min(matches.len() - 1);
        self.matches = matches;
    }

    fn is_open(&self) -> bool {
        !self.matches.is_empty()
    }

    /// Wrap-around selection moves (Up/Down cycle through the matches).
    fn next(&mut self) {
        if self.is_open() {
            self.sel = (self.sel + 1) % self.matches.len();
        }
    }
    fn prev(&mut self) {
        if self.is_open() {
            self.sel = (self.sel + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// The completion text for the selected command: bare "/name", or
    /// "/name " WITH a trailing space for argument-taking commands (/export),
    /// so the path can be typed straight on.
    fn completion(&self) -> Option<String> {
        let c = self.matches.get(self.sel)?;
        Some(if c.arg_hint.is_empty() {
            format!("/{}", c.name)
        } else {
            format!("/{} ", c.name)
        })
    }

    /// Close until the text changes (see `dismissed`).
    fn dismiss(&mut self, text: &str) {
        self.dismissed = Some(text.to_string());
        self.matches.clear();
        self.prefix.clear();
        self.sel = 0;
    }
}

/// What an Input-mode key does while the popup is open. None = not
/// intercepted, the caller's normal handling applies unchanged.
enum PopupAction {
    /// Up/Down moved the selection; the key is fully consumed (it must NOT
    /// also move the caret or recall history).
    Consumed,
    /// Esc: close the popup only — quitting stays one more Esc away.
    Dismiss,
    /// Tab/Enter: complete the command into the input but do NOT send — a
    /// bare Enter must never fire a destructive /clear by accident; the
    /// user presses Enter a second time to actually run it.
    Complete(String),
}

/// Key interception while the popup is open. Modifier chords (Alt+Up history
/// recall, Ctrl-…) are never intercepted — they fall through to the normal
/// handlers, and the text change they cause re-filters the popup anyway.
fn popup_key(
    popup: &mut SlashPopup,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<PopupAction> {
    if !modifiers.is_empty() {
        return None;
    }
    match code {
        KeyCode::Up => {
            popup.prev();
            Some(PopupAction::Consumed)
        }
        KeyCode::Down => {
            popup.next();
            Some(PopupAction::Consumed)
        }
        KeyCode::Esc => Some(PopupAction::Dismiss),
        KeyCode::Tab | KeyCode::Enter => popup.completion().map(PopupAction::Complete),
        _ => None,
    }
}

/// The (skip, take) window of matches shown under the height cap: the
/// selection always stays inside it.
fn visible_window(len: usize, sel: usize, max: usize) -> (usize, usize) {
    let take = len.min(max);
    let skip = if sel >= take { sel + 1 - take } else { 0 };
    (skip, take)
}

/// Render the popup rows: "/name [arg]  one-line description", the selected
/// row in reverse video padded out to the popup's content width so the bar
/// fills the whole row, not just the text.
///
/// `max_rows` is the live zone height: windowing to the ACTUAL band (not just
/// the `POPUP_MAX_ROWS` ceiling) keeps the selection on-screen when the command
/// list is taller than the zone (7 commands in a ~3-row band would otherwise
/// scroll the highlighted row out the top).
fn slash_popup_lines(popup: &SlashPopup, max_rows: usize) -> Vec<Line<'static>> {
    let max = max_rows.clamp(1, POPUP_MAX_ROWS);
    let (skip, take) = visible_window(popup.matches.len(), popup.sel, max);
    let mut lines: Vec<Line<'static>> = popup.matches[skip..skip + take]
        .iter()
        .map(|c| {
            Line::from(vec![
                Span::styled(format!("/{}", c.name), Style::default().fg(USER_FG)),
                if c.arg_hint.is_empty() {
                    Span::raw(String::new())
                } else {
                    Span::styled(format!(" {}", c.arg_hint), Style::default().fg(DIM_FG))
                },
                Span::styled(format!("  {}", c.desc), Style::default().fg(DIM_FG)),
            ])
        })
        .collect();
    let content_w = lines.iter().map(|l| l.width()).max().unwrap_or(0);
    if popup.sel >= skip && popup.sel < skip + take {
        let line = &mut lines[popup.sel - skip];
        line.style = line.style.add_modifier(Modifier::REVERSED);
        let pad = content_w.saturating_sub(line.width());
        if pad > 0 {
            line.spans.push(Span::styled(
                " ".repeat(pad),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        }
    }
    lines
}

/// Render the conversation rows as plain text for `/export` — a readable
/// transcript mirroring the on-screen layout, minus styling/wrapping. Empty
/// assistant rows (left while a turn is mid-stream) are skipped so the dump
/// has no bare "Tiny" headers with no body.
fn render_transcript(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        match row {
            Row::User(t) => {
                out.push_str("You\n");
                out.push_str(t);
                out.push_str("\n\n");
            }
            Row::Queued(t) => {
                out.push_str("You (queued)\n");
                out.push_str(t);
                out.push_str("\n\n");
            }
            Row::Assistant(t) if t.is_empty() => {}
            Row::Assistant(t) => {
                out.push_str("Tiny\n");
                out.push_str(t);
                out.push_str("\n\n");
            }
            Row::ToolCall { name, input, .. } => {
                out.push_str(&format!("→ {name}: {input}\n\n"));
            }
            Row::ToolResult {
                name,
                content,
                is_error,
                ..
            } => {
                let tag = if *is_error { "✗" } else { "✓" };
                // Content may carry ANSI codes (kept for the TUI's colored
                // render); the transcript wants the plain text.
                out.push_str(&format!("{tag} {name}: {}\n\n", ansi_plain(content)));
            }
            Row::System { text, .. } => {
                out.push_str(&format!("· {text}\n\n"));
            }
        }
    }
    out
}

/// Best-effort copy to the system clipboard via the platform's CLI tool. Tries
/// each known tool in turn; returns true if one accepted the text. No clipboard
/// crate dep (keeps the binary lean) — this shelling-out covers macOS (pbcopy),
/// Wayland (wl-copy), X11 (xclip/xsel), and Windows (clip). A tool that isn't
/// installed fails to spawn and we move on; if none is present, `/export` falls
/// back to writing a file.
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let attempts: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("clip", &[]),
    ];
    for (prog, args) in attempts {
        let mut child = match Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue, // not installed / not on PATH → try the next
        };
        let fed = child
            .stdin
            .as_mut()
            .map(|s| s.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        drop(child.stdin.take()); // EOF so the tool flushes and exits
                                  // Always reap. An earlier `fed && wait()` short-circuited the wait
                                  // when the write failed (broken pipe after the tool exited early),
                                  // and Rust's `Child::drop` does NOT reap on Unix — leaking a zombie
                                  // until process exit. wait() runs unconditionally; its result still
                                  // combines with `fed` so a failed write reports failure.
                                  //
                                  // Bound the wait at 3s: a clipboard tool that holds stdin open without
                                  // exiting (e.g. xclip daemonizing in some modes) would otherwise hang
                                  // the whole TUI. The thread owns the child and reaps it whenever it
                                  // finally exits; on timeout we just stop blocking the UI.
        let exited_ok = {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(child.wait().map(|s| s.success()).unwrap_or(false));
            });
            rx.recv_timeout(std::time::Duration::from_secs(3))
                .unwrap_or(false)
        };
        let ok = fed && exited_ok;
        if ok {
            return true;
        }
    }
    false
}

/// Expand a leading `~/` (and a bare `~`) to `$HOME`, for `/export ~/notes.md`.
/// Only the current user's home — `~other` is left as-is (rare; not needed
/// here). `_with` takes home explicitly so the logic is unit-testable without
/// mutating the process-wide `$HOME`; `expand_tilde` binds it to the env.
fn expand_tilde_with(path: &str, home: Option<&std::ffi::OsStr>) -> String {
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

enum UiMsg {
    Event(AgentEvent),
    Error(String),
    RunFinished,
}

/// Control or text input from the UI to the worker thread. `None` on the
/// channel means "quit" (Esc / idle Ctrl-C); `Some(Text)` is a user message;
/// `Some(Clear)` is the `/clear` command, which the worker applies to the
/// agent's history without running a turn.
enum Input {
    Text(String),
    Clear,
}

/// On drop, signals worker completion so the quit path can wait a bounded
/// time for the worker to exit (and let a cancel-aware tool kill its child)
/// instead of detaching it mid-tool and orphaning a spawned shell process.
struct DoneSignal(mpsc::Sender<()>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Restores the terminal on drop. Installed right after raw mode is entered,
/// so a panic (or a propagated `?` error) inside the UI loop unwinds through
/// this and still leaves the user's terminal usable — without it, a panic
/// mid-draw strands them in raw mode with no echo. Inline mode lives on the
/// PRIMARY screen (no alternate screen) and captures no mouse, so teardown is
/// just bracketed-paste + raw-mode — native selection/scroll keep working.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = stdout().execute(DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

pub fn run(
    agent: Agent,
    cfg: &Config,
    initial_task: Option<String>,
    auto_start: bool,
) -> Result<()> {
    // UI → worker: user inputs (None = quit).
    let (in_tx, in_rx) = mpsc::channel::<Option<Input>>();
    // Worker → quit-path: the worker signals it has exited, so quitting can
    // join within a bounded wait instead of detaching mid-tool.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    // Worker → UI: agent events.
    let (out_tx, out_rx) = mpsc::channel::<UiMsg>();
    // Cancellation: Esc sets this; the agent checks it at each turn boundary.
    // This is the RECOVERABLE per-turn interrupt — the next turn reuses the same
    // MCP server child, so it must never tear it down.
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Teardown: set ONLY on session end (the quit path below), never on a
    // per-turn Esc. The MCP shutdown-reaper watches this — firing it kills the
    // server child so a stuck call unblocks on quit. Split from `cancel` so a
    // recoverable turn-interrupt doesn't irreversibly kill the MCP server the
    // next turn couldn't restart (P1 audit fix).
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Set up the terminal BEFORE spawning the worker. The worker may fire an
    // initial task immediately (agent.ends_with_user() → run_once), and every
    // step below can fail with `?` (stdout not a tty, broken pipe, Terminal
    // creation). When setup preceded the spawn, such a failure returned early
    // and skipped the cancel/join cleanup below — leaving the agent running API
    // calls and tools in the background with no UI and no cancel signal. Doing
    // this first means a setup failure simply returns before any worker exists.
    enable_raw_mode()?;
    // Create the restore guard IMMEDIATELY after the first state-changing
    // call, so a `?` failure in ANY of the steps below (EnableBracketedPaste /
    // Terminal::with_options — e.g. stdout not a tty, broken pipe) still drops
    // the guard and restores the terminal. Drop is fully idempotent (every
    // disable is `let _ =`), so creating the guard before some enables have
    // run is harmless.
    let _guard = TerminalGuard;
    // Bracketed paste: terminals that support it deliver a pasted blob as one
    // Event::Paste (newlines included) instead of replaying it as keystrokes —
    // which would hit the Enter handler and send each line as a message.
    stdout().execute(EnableBracketedPaste)?;
    // Inline viewport: the TUI lives in a fixed INLINE_HEIGHT-row band at the
    // bottom of the PRIMARY screen — no alternate screen, no mouse capture.
    // Finalized conversation turns are committed into the terminal's own
    // scrollback via Terminal::insert_before, so the host terminal's NATIVE
    // text selection, scrolling, and search all work on the conversation. The
    // float-on-redraw selection bug of the full-screen model cannot occur
    // because the conversation lives in real scrollback, not a redrawn frame.
    let mut term = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(INLINE_HEIGHT),
        },
    )?;

    // The worker thread owns the agent and drives run() per user input.
    let worker_cancel = cancel.clone();
    let worker_shutdown = shutdown.clone();
    let worker = std::thread::spawn(move || {
        // Signal completion on ANY exit (normal loop end or early return) via
        // Drop, so the quit path can join within a bounded wait.
        let _done = DoneSignal(done_tx);
        let mut agent = agent;
        agent.set_cancel(worker_cancel);
        // Arm the MCP shutdown-reaper on the session-end flag (NOT cancel): a
        // per-turn Esc leaves the server alive; only a real quit kills it.
        agent.set_shutdown(worker_shutdown);

        // Run one agent turn. Returns false once the UI has gone away
        // (out_rx dropped) so the loop can stop instead of pushing into
        // a dead channel.
        let run_once = |agent: &mut Agent| -> bool {
            let res = agent.run(&mut |ev| {
                let _ = out_tx.send(UiMsg::Event(ev));
            });
            let msg = match res {
                Ok(_) => UiMsg::RunFinished,
                Err(e) => UiMsg::Error(format!("{e:#}")),
            };
            out_tx.send(msg).is_ok()
        };

        // If the session already ends with a user message — an initial
        // task baked in by main, or a resumed session stopped mid-turn at
        // a tool_result — kick off a run now. We must NOT re-send the
        // task over in_tx: main already put it in the agent's history,
        // and sending again would create two adjacent user messages
        // (Anthropic rejects that with a 400).
        if agent.ends_with_user() && !run_once(&mut agent) {
            return;
        }
        while let Ok(Some(cmd)) = in_rx.recv() {
            match cmd {
                Input::Text(text) => {
                    if let Some(w) = agent.push_user(vec![Block::Text(text)]) {
                        let _ = out_tx.send(UiMsg::Event(AgentEvent::Warning(w)));
                    }
                    if !run_once(&mut agent) {
                        return;
                    }
                }
                Input::Clear => {
                    if let Some(w) = agent.clear_messages() {
                        let _ = out_tx.send(UiMsg::Event(AgentEvent::Warning(w)));
                    }
                }
            }
        }
    });

    let result = ui_loop(
        &mut term,
        cfg,
        &in_tx,
        &out_rx,
        &cancel,
        initial_task.as_deref(),
        auto_start,
    );

    // Stop the worker before restoring the terminal. If terminal restoration
    // errors out below, the worker has already been told to stop (and is
    // detached on scope exit) instead of running unchecked to its next limit.
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    // Session end: fire the SEPARATE teardown flag so the MCP shutdown-reaper
    // kills a wedged server child and unblocks the worker's bounded join below.
    // (cancel alone won't: the worker may be stuck inside a 120s MCP call, not
    // at a turn boundary where cancel is checked.) This is the ONLY place
    // shutdown is raised — a per-turn Esc never reaches here.
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(in_tx); // signal worker to exit
                 // Wait a bounded time for the worker to actually exit. A cancel-aware
                 // shell kills its process group within ~50ms, so joining here prevents the
                 // orphan-on-quit case where the process exits mid-tool and reparents a
                 // running shell to init. A wedged MCP call is now killed by the shutdown
                 // reaper armed above (child killed → stdout EOF → blocked request returns)
                 // within the reaper's 100ms poll, so it too unblocks here. A genuinely
                 // non-cancel-aware tool with no teardown just lets this time out — we then
                 // detach as before, never hanging exit.
    let worker_stuck = done_rx.recv_timeout(Duration::from_secs(3)).is_err();
    drop(worker); // detach if the worker is still stuck past the deadline
                  // Restore the terminal BEFORE reporting discarded queued messages — a note
                  // printed while raw mode is still active would be garbled. The guard's Drop
                  // is idempotent, so a panic path below still ends clean. Inline mode leaves
                  // the committed scrollback intact: the conversation stays in the terminal's
                  // history like ordinary command output.
    drop(_guard);
    // Step past the inline viewport so the dropped-queue note (and the shell
    // prompt on return) doesn't overwrite the viewport's last frame.
    println!();
    // A detached worker is a lifecycle fact the user must see, not silence:
    // a third-party tool / host callback with no cancel awareness can still
    // be running (writing files, holding connections) after the terminal is
    // back. The MCP reaper + cancel-aware tools normally finish within the
    // 3s join above; this fires only for the remainder.
    if worker_stuck {
        eprintln!(
            "[tiny-agent] worker did not exit within 3s — detached; a non-cancel-aware \
             tool may still be running in the background"
        );
    }
    if let Ok(dropped) = result {
        if dropped > 0 {
            eprintln!("[tiny-agent] {dropped} queued message(s) discarded on quit");
        }
    }
    result.map(|_| ())
}

fn ui_loop(
    term: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    cfg: &Config,
    in_tx: &mpsc::Sender<Option<Input>>,
    out_rx: &mpsc::Receiver<UiMsg>,
    cancel: &std::sync::atomic::AtomicBool,
    initial_task: Option<&str>,
    auto_start: bool,
) -> Result<usize> {
    // The full conversation, in append order. Finalized prefixes are committed
    // into the terminal scrollback via insert_before and never re-rendered; the
    // live viewport only ever shows the tail end (the in-flight streaming row
    // and queued ghosts), so memory grows with the session but per-frame work
    // does not.
    let mut rows: Vec<Row> = Vec::new();
    // Watermark: rows[0..committed] have already been flushed to scrollback and
    // are permanent (only Queued ghosts are ever removed, and they live in the
    // uncommitted tail). See `pending_end` for the finalize rule.
    let mut committed: usize = 0;
    let mut input = InputBuffer::new();
    let mut busy = false;
    // Messages typed while a run is in flight, oldest first. Enter queues
    // here instead of sending; the RunFinished/Error flush pops one and sends
    // it (only then committing its Row::User, so the transcript stays in true
    // send order).
    let mut queued: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    // Start of the current run (one user message → possibly many turns), for
    // the status-bar elapsed time and the spinner frame. None when idle.
    let mut run_start: Option<Instant> = None;
    // What the agent is doing right now ("thinking" / "writing" / "→ tool"),
    // shown in the busy status segment; fed by the event stream.
    let mut activity = String::new();
    let mut streaming = false;
    // Transient "agent is thinking" marker: set at TurnStart (and when a
    // queued message is flushed into a new run), cleared by the first visible
    // output (text delta or tool call) and at run end. Render-only — the live
    // viewport renders a `● …` line from it; nothing is committed for it.
    let mut thinking = false;
    // Transient in-flight tool tracker (render-only, like `thinking`): the
    // ToolCall row commits immediately, so this placeholder is what keeps the
    // live zone showing WHICH tool is running and for how long.
    let mut tool_running = ToolRunning::new();
    // Name of the sub-agent whose text is currently accumulating into the
    // trailing Assistant row, so consecutive deltas from the same sub-agent
    // merge — and a different sub-agent (or the parent) starts a fresh row.
    let mut sub_streaming_name: Option<String> = None;
    // Input history (Alt+Up/Down to recall). `draft` holds the in-progress
    // text when the user starts browsing, so Down past the newest restores it.
    let mut history: Vec<String> = Vec::new();
    let mut hist_idx: Option<usize> = None;
    let mut draft: String = String::new();
    // Slash-command completion popup: refreshed from the input text before
    // every draw, consulted by the key handler while open.
    let mut popup = SlashPopup::new();
    // Full cwd path for the status bar's right-aligned segment — computed
    // once, not per frame.
    let cwd_path: String = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Running token totals for the session, summed from each turn's usage.
    let mut in_tokens: u64 = 0;
    let mut out_tokens: u64 = 0;
    // Prompt-cache write/read totals — shown in the status bar so the user can
    // see caching paying off (Anthropic reports both; OpenAI reports reads).
    let mut cache_create: u64 = 0;
    let mut cache_read: u64 = 0;
    // Estimated context-window occupancy after the latest API call, SET (not
    // summed) from each Usage event: prompt tokens (incl. cached parts) plus
    // the response just appended to the history. Drives the status bar's
    // `ctx` segment.
    let mut ctx_tokens: u64 = 0;
    // The agent's current task plan, tracked via the `todo` tool. Shown live in
    // the status bar (a `plan done/total` segment) so progress is visible
    // without scrolling — the full checklist also lives in the most recent
    // `todo` tool-result row in the transcript. Updated on each TodoUpdate
    // (full-replacement semantics: each call sends the entire plan).
    let mut plan: Vec<crate::agent::TodoItem> = Vec::new();
    // Per-row markdown render cache (M6). Only the in-flight streaming row is
    // ever re-rendered in the live viewport, so this stays small; committed
    // rows are rendered once at commit time.
    let mut md_cache = MdCache::new();
    // Dirty flag for the render pass: set by any worker message or terminal
    // event; while clear (and not busy) the loop skips term.draw entirely.
    let mut dirty = true; // first frame must paint
    let mut last_draw = Instant::now();

    rows.push(Row::sys_info(welcome_text(&cfg.model, cfg.tools.len())));
    // Mirror the initial task (already in the agent's history) so the user
    // sees what's running. Do NOT re-send it over in_tx: main has already
    // placed it in the agent's messages, and a second send would duplicate the
    // task and create two adjacent user messages (Anthropic rejects that 400).
    // The worker starts the run itself.
    if let Some(task) = initial_task {
        rows.push(Row::User(task.to_string()));
    }
    // The worker auto-runs when the session ends with a user message (an
    // initial task, or a resumed session stopped mid-turn). Show "running" so
    // the status bar is honest; further submissions queue behind this run.
    if auto_start {
        busy = true;
        run_start = Some(Instant::now());
    }

    loop {
        // Drain worker events.
        loop {
            let msg = match out_rx.try_recv() {
                Ok(m) => m,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker thread is gone (e.g. panicked mid-run, or the
                    // process is tearing down). Don't leave the UI stuck in
                    // "running" waiting for a RunFinished that will never come
                    // — surface it so the user can Esc out cleanly.
                    if busy {
                        busy = false;
                        run_start = None;
                        streaming = false;
                        sub_streaming_name = None;
                        // TurnStart sets this true; without resetting it here a
                        // worker that dies during the thinking phase (before any
                        // TextDelta clears it) would leave a stale flag.
                        thinking = false;
                        tool_running.clear();
                        dirty = true;
                        rows.push(Row::sys_error(
                            "[worker stopped unexpectedly — type /quit to exit]",
                        ));
                    }
                    // Nothing queued can ever be delivered now: drop the
                    // pending texts and their ghost rows. Row removal shifts
                    // indices, so the index-keyed MdCache is reset when
                    // anything was removed (the watermark stays valid — only
                    // uncommitted ghosts are ever removed).
                    queued.clear();
                    if drop_queued_ghosts(&mut rows) {
                        md_cache.clear();
                    }
                    break;
                }
            };
            dirty = true;
            match msg {
                UiMsg::Event(ev) => match ev {
                    AgentEvent::TurnStart(_) => {
                        streaming = false;
                        activity = "thinking".into();
                        thinking = true;
                        tool_running.clear();
                    }
                    AgentEvent::TextDelta(t) | AgentEvent::ThinkingDelta(t) => {
                        // First visible output: the thinking placeholder is
                        // superseded by real content (same below at ToolCall).
                        // NOTE: a ThinkingDelta (reasoning-model trace) is folded
                        // into the same assistant row as reply text — the TUI is
                        // frozen (README Non-goals: bugfix-only), so it only
                        // guarantees the trace is VISIBLE, not styled as a
                        // separate collapsible block. TinyWorker's GUI renders
                        // that block off the same event.
                        thinking = false;
                        // The PARENT is streaming: no parent tool is pending
                        // anymore. (Sub-agent deltas below don't clear — a
                        // sibling parallel tool may legitimately still run.)
                        tool_running.clear();
                        // Parent speaks: close any open sub-agent bubble AND
                        // keep this delta out of it — streaming=false forces
                        // append_delta to open a fresh row even though the sub
                        // bubble is an Assistant row (the old code merged the
                        // parent's text into the sub-agent's bubble).
                        if sub_streaming_name.take().is_some() {
                            streaming = false;
                        }
                        // Guard the assignment: this fires per streamed delta,
                        // so only touch the string on the thinking→writing
                        // transition instead of allocating every chunk.
                        if activity != "writing" {
                            activity = "writing".into();
                        }
                        append_delta(&mut rows, &mut streaming, &t);
                    }
                    AgentEvent::ToolCall { name, input } => {
                        streaming = false;
                        thinking = false;
                        activity = format!("→ {name}");
                        tool_running.on_call(&name);
                        // Strip control chars once at entry so both the
                        // collapsed summary and the expanded pretty-print
                        // render clean.
                        let input = strip_controls_json(&input);
                        let collapsed = compact(&input);
                        rows.push(Row::ToolCall {
                            name,
                            input,
                            collapsed,
                            expanded: false,
                            started: Instant::now(),
                        });
                    }
                    AgentEvent::ToolResult {
                        name,
                        is_error,
                        content,
                    } => {
                        streaming = false;
                        tool_running.on_result(&name);
                        // Content stays RAW: ANSI colors survive into
                        // ansi_guided_lines at render (which strips non-SGR
                        // sequences itself), so strip_controls is gone from
                        // this path.
                        let elapsed = tool_elapsed_for(&rows, &name);
                        let collapsed = crate::agent::preview(&ansi_plain(&content));
                        let expanded = auto_expand_result(is_error, &content);
                        rows.push(Row::ToolResult {
                            collapsed,
                            name,
                            content,
                            is_error,
                            expanded,
                            elapsed,
                        });
                    }
                    AgentEvent::Done { stop_reason } => {
                        streaming = false;
                        // A normal turn end is the common case — no row. Only
                        // abnormal stops (max_tokens/length/error/unknown) are
                        // worth a notice.
                        if !is_normal_stop(&stop_reason) {
                            rows.push(Row::sys_warn(format!("[done: {stop_reason}]")));
                        }
                    }
                    AgentEvent::Compressed {
                        dropped,
                        kept,
                        trigger,
                    } => {
                        streaming = false;
                        let how = match trigger {
                            crate::agent::CompressTrigger::Preflight => "preflight",
                            crate::agent::CompressTrigger::Overflow => "overflow",
                        };
                        rows.push(Row::sys_info(format!(
                            "[context compressed ({how}): dropped {dropped} messages, {kept} kept]"
                        )));
                    }
                    AgentEvent::Warning(w) => {
                        rows.push(Row::sys_warn(format!("[warn] {w}")));
                    }
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
                    } => {
                        in_tokens += input_tokens;
                        out_tokens += output_tokens;
                        cache_create += cache_creation_input_tokens;
                        cache_read += cache_read_input_tokens;
                        // Per-request values, not cumulative: overwrite. The
                        // prompt side is input + both cache buckets (Anthropic
                        // splits cached tokens out of input_tokens; OpenAI
                        // reports the whole prompt in input_tokens and leaves
                        // the buckets at 0). + output covers the response the
                        // next request will carry.
                        ctx_tokens = input_tokens
                            + cache_creation_input_tokens
                            + cache_read_input_tokens
                            + output_tokens;
                    }
                    AgentEvent::TodoUpdate { todos } => {
                        // Update the live plan state — the status bar renders a
                        // `plan done/total` segment from it each frame (see
                        // build_status_line). No transcript row: the full
                        // checklist is already visible in the `todo` tool-result
                        // row, and a per-update sys_info line would just clutter
                        // the scrollback. Full-replacement semantics: each call
                        // carries the entire plan, so a plain assign is correct.
                        plan = todos.clone();
                    }
                    AgentEvent::SubAgent { name, event } => {
                        // Render the child's event tagged with its name so the
                        // user can see each sub-agent's progress (and tell
                        // parallel ones apart). Usage is already folded into the
                        // run totals at the agent level, so skip it here.
                        //
                        // Known limitation: consecutive TextDelta chunks merge
                        // into the last Assistant row whose tag matches `name`.
                        // If a turn runs two parallel sub-agents with the SAME
                        // name (the model gave identical `name`s), their text
                        // interleaves into one row. Distinguishing them would
                        // need a stable id on AgentEvent::SubAgent (not just the
                        // model-supplied name); the contract is that `name`
                        // uniquely identifies a sub-agent within a turn.
                        // Strip control chars from the model-supplied name
                        // once, so every use (bubble prefix, row names,
                        // activity segment) is terminal-safe.
                        let name = strip_controls(&name);
                        match &*event {
                            AgentEvent::TextDelta(t) | AgentEvent::ThinkingDelta(t) => {
                                // ThinkingDelta folded in alongside TextDelta
                                // (TUI shows the trace inline; see main match).
                                thinking = false;
                                append_sub_delta(&mut rows, &mut sub_streaming_name, &name, t);
                            }
                            AgentEvent::ToolCall { name: tn, input } => {
                                sub_streaming_name = None;
                                thinking = false;
                                activity = format!("[{name}] → {tn}");
                                let input = strip_controls_json(input);
                                let collapsed = compact(&input);
                                // Same display name as the row below, so the
                                // placeholder and the committed head agree.
                                let row_name = format!("[{name}] {tn}");
                                tool_running.on_call(&row_name);
                                rows.push(Row::ToolCall {
                                    name: row_name,
                                    input,
                                    collapsed,
                                    expanded: false,
                                    started: Instant::now(),
                                });
                            }
                            AgentEvent::ToolResult {
                                name: tn,
                                is_error,
                                content,
                            } => {
                                sub_streaming_name = None;
                                // The row name mirrors the ToolCall arm's
                                // format, so nearest-back matching finds it.
                                // Content stays raw for the ANSI-aware render.
                                let row_name = format!("[{name}] {tn}");
                                tool_running.on_result(&row_name);
                                let elapsed = tool_elapsed_for(&rows, &row_name);
                                let collapsed = crate::agent::preview(&ansi_plain(content));
                                // Same auto-expand contract as the parent arm.
                                let expanded = auto_expand_result(*is_error, content);
                                rows.push(Row::ToolResult {
                                    collapsed,
                                    name: row_name,
                                    content: content.clone(),
                                    is_error: *is_error,
                                    expanded,
                                    elapsed,
                                });
                            }
                            AgentEvent::Done { .. } => sub_streaming_name = None,
                            AgentEvent::Warning(w) => {
                                sub_streaming_name = None;
                                rows.push(Row::sys_warn(format!("[{name}] [warn] {w}")));
                            }
                            _ => {} // TurnStart/Compressed/Usage/SubAgent: noisy or folded
                        }
                    }
                    // non_exhaustive forward-compat: AgentEvent may grow new
                    // variants (0.x SemVer window); they render nothing here
                    // until given a dedicated arm. Compile-adaptation only —
                    // no behavior change for the variants above.
                    _ => {}
                },
                UiMsg::Error(e) => {
                    rows.push(Row::sys_error(format!("[error] {e}")));
                    activity.clear();
                    tool_running.clear();
                    // End every open stream: a leftover `streaming` /
                    // `sub_streaming_name` would let the NEXT run's first
                    // TextDelta merge into a stale row (or, when the last row
                    // is this System row, be silently dropped).
                    streaming = false;
                    sub_streaming_name = None;
                    // Flush the queue: send the next queued message right
                    // away, keeping busy=true on success so no idle gap opens
                    // (a Ctrl-C in that gap would quit instead of cancelling).
                    let flush = flush_next_queued(&mut queued, &mut rows, &mut md_cache, in_tx);
                    apply_flush(
                        flush,
                        &mut busy,
                        &mut run_start,
                        &mut activity,
                        &mut thinking,
                    );
                }
                UiMsg::RunFinished => {
                    streaming = false;
                    sub_streaming_name = None;
                    activity.clear();
                    tool_running.clear();
                    // Flush the queue (same contract as the Error branch).
                    let flush = flush_next_queued(&mut queued, &mut rows, &mut md_cache, in_tx);
                    apply_flush(
                        flush,
                        &mut busy,
                        &mut run_start,
                        &mut activity,
                        &mut thinking,
                    );
                }
            }
        }

        // Commit finalized turns into the terminal scrollback. A row is
        // "finalized" once it will never mutate again: everything except the
        // in-flight streaming Assistant row (skipped while a stream is open)
        // and queued ghosts (removed on flush, never committed). This is the
        // one place the inline model writes the conversation to scrollback, so
        // the host terminal's native selection/scroll/search cover it.
        let stream_open = streaming || sub_streaming_name.is_some();
        let pending_end = commit_pending_end(&rows, committed, stream_open);
        if committed < pending_end {
            let width = term.size().map(|s| s.width).unwrap_or(80) as usize;
            commit_rows(term, &rows[committed..pending_end], &mut md_cache, width)?;
            committed = pending_end;
            dirty = true;
        }
        // Index of the in-flight row shown live in the viewport's transcript
        // zone (None when nothing is streaming). Computed independently of the
        // commit watermark above — see inflight_row_index — so a streaming
        // Assistant that opened AFTER a queued ghost still renders.
        let inflight_idx = inflight_row_index(&rows, stream_open);

        // Render the live viewport — only when something changed (worker event
        // drained, key handled, first frame). While a run is in flight, also
        // redraw on a fast tick so the spinner animates and the status-bar
        // elapsed time keeps moving. Committed turns are already in scrollback,
        // so this redraws only the small bottom band, not the whole history.
        let clock_tick = busy && last_draw.elapsed() >= Duration::from_millis(120);
        if dirty || clock_tick {
            dirty = false;
            last_draw = Instant::now();
            // The slash popup tracks the input text: any edit sets dirty, so
            // this refresh rides the same redraw.
            popup.update(&input.text());
            term.draw(|f| {
                let area = f.area(); // the INLINE_HEIGHT-row band at the bottom
                let term_w = area.width;
                // Input box grows with the number of lines typed, up to half
                // the viewport so the zone + status stay visible.
                let max_input = ((area.height as usize) / 2).max(3);
                let input_h = (input.line_count() + 2).min(max_input) as u16;
                let [zone_rect, input_rect, status_rect] = Layout::vertical([
                    Constraint::Min(0),
                    Constraint::Length(input_h),
                    Constraint::Length(1),
                ])
                .areas(area);
                let content_w = content_width(term_w);

                // Transcript zone (the rows above the input box inside the
                // viewport): the slash popup while typing a command, else the
                // thinking placeholder before any output, else the in-flight
                // streaming row, else the running-tool placeholder (a tool
                // execution streams nothing of its own). Each is bottom-aligned
                // so its last line sits flush above the input box.
                // Already-committed turns live in the host terminal's
                // scrollback above this viewport.
                if popup.is_open() {
                    let lines = slash_popup_lines(&popup, zone_rect.height as usize);
                    render_zone_top(f, zone_rect, &lines);
                } else if thinking {
                    let line = Line::from(vec![
                        Span::styled(
                            "● ".to_string(),
                            Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("…", Style::default().fg(DIM_FG)),
                    ]);
                    render_zone_tail(f, zone_rect, &[line]);
                } else if let Some(idx) = inflight_idx {
                    if let Some(row) = rows.get(idx) {
                        let l = build_history_lines(
                            std::slice::from_ref(row),
                            &mut md_cache,
                            content_w,
                            true,
                            false,
                        );
                        let wrapped = wrap_history(&l, content_w);
                        render_zone_tail(f, zone_rect, &wrapped);
                    }
                } else if let Some(line) = tool_running_line(&tool_running, Instant::now()) {
                    render_zone_tail(f, zone_rect, &[line]);
                }

                // Input box. The editing prompt is stable; Enter queues while
                // busy. The border frame tracks the run: busy flips the whole
                // frame to AGENT_FG so the bottom chrome reads as one busy
                // unit, idle falls back to USER_FG.
                let spinner_char = SPINNER[(run_start
                    .map(|t| t.elapsed().as_millis() / 100)
                    .unwrap_or(0) as usize)
                    % SPINNER.len()];
                let (prompt, prompt_style) = (
                    "❯ ".to_string(),
                    Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
                );
                let body = input.text();
                let lines_view = input.lines_view();
                let (crow, ccol) = input.cursor_pos();
                // The input box height is capped, so a long paste can exceed
                // it. Scroll the box to follow the caret and place the caret
                // within the visible window. The same applies HORIZONTALLY: the
                // Paragraph doesn't wrap, so a line wider than the box is
                // clipped and an unclamped caret would be drawn past the border.
                let inner_rows = (input_h as usize).saturating_sub(2);
                let input_scroll = crow
                    .saturating_sub(inner_rows.saturating_sub(1))
                    .min(u16::MAX as usize) as u16;
                let inner_cols = (input_rect.width as usize).saturating_sub(2).max(1);
                let lead = if crow == 0 { prompt.width() } else { 0 };
                let caret_col = ccol + lead; // display column within the caret's line
                let (hscroll, caret_x) = input_hscroll(caret_col, inner_cols);
                let secs = run_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                let act = if activity.is_empty() {
                    "working"
                } else {
                    activity.as_str()
                };
                let input_title = build_input_title(
                    busy,
                    spinner_char,
                    act,
                    secs,
                    queued.len(),
                    (input_rect.width as usize).saturating_sub(2),
                );
                let title_fg = chrome_fg(busy);
                // Ghost placeholder on an empty input box. The caret math below
                // is unaffected: an empty buffer's caret sits at the prompt end,
                // in front of the placeholder text.
                // Build one Line per logical line so a multiline buffer renders
                // across rows (a single Line carrying embedded `\n` collapses:
                // ratatui's grapheme iterator filters control chars including
                // `\n`, so the caret row math below would land on an empty row).
                // The prompt spans only the first line; subsequent lines start
                // at the left border so caret `lead` (computed below as
                // prompt width only when crow==0) stays correct.
                let input_lines: Vec<Line> = if body.is_empty() {
                    vec![Line::from(vec![
                        Span::styled(prompt.clone(), prompt_style),
                        Span::styled(INPUT_PLACEHOLDER, Style::default().fg(DIM_FG)),
                    ])]
                } else {
                    lines_view
                        .iter()
                        .enumerate()
                        .map(|(i, ln)| {
                            if i == 0 {
                                Line::from(vec![
                                    Span::styled(prompt.clone(), prompt_style),
                                    Span::raw(ln.clone()),
                                ])
                            } else {
                                Line::from(vec![Span::raw(ln.clone())])
                            }
                        })
                        .collect()
                };
                let input_widget = Paragraph::new(input_lines)
                    .block(
                        WBlock::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(title_fg))
                            .title(Span::styled(input_title, Style::default().fg(title_fg))),
                    )
                    .scroll((input_scroll, hscroll));
                f.render_widget(input_widget, input_rect);
                // Place the caret. Column = border(1) + display width of the text
                // before the caret (+ the prompt, on the first line only), minus
                // the horizontal scroll. Display width keeps it correct for wide
                // chars; hscroll keeps it inside the box on over-wide lines.
                let x = input_rect.x + 1 + caret_x;
                let y = input_rect.y + 1 + (crow as u16).saturating_sub(input_scroll);
                f.set_cursor_position((x, y));

                // Status bar. Detailed run activity lives in the input title;
                // the bar only names the broad state so the two regions do not
                // compete. Idle shows no clock — the run elapsed is the title's
                // job.
                let state = if busy { "running" } else { "idle" }.to_string();
                let cache_seg = if cache_read > 0 || cache_create > 0 {
                    format!(
                        " · cache {}w {}r",
                        fmt_tokens(cache_create),
                        fmt_tokens(cache_read)
                    )
                } else {
                    String::new()
                };
                let stats = format!(
                    " · {} in · {} out{cache_seg}",
                    fmt_tokens(in_tokens),
                    fmt_tokens(out_tokens),
                );
                let ctx = fmt_ctx(ctx_tokens, cfg.limits.max_context_tokens);
                // Plan segment: " · plan done/total", green when every step is
                // completed, yellow while work remains. None when the agent
                // hasn't used `todo` (segment absent, not a null).
                let plan_seg = if plan.is_empty() {
                    None
                } else {
                    let total = plan.len();
                    let done = plan.iter().filter(|t| t.status == "completed").count();
                    let style = if done == total {
                        Style::default().fg(AGENT_FG)
                    } else {
                        Style::default().fg(TOOL_CALL_FG)
                    };
                    Some((format!(" · plan {done}/{total}"), style))
                };
                let status = build_status_line(
                    &cfg.model,
                    &state,
                    busy,
                    ctx.as_ref().map(|(t, s)| (t.as_str(), *s)),
                    plan_seg.as_ref().map(|(t, s)| (t.as_str(), *s)),
                    &stats,
                    &cwd_path,
                    status_rect.width as usize,
                );
                f.render_widget(Paragraph::new(status), status_rect);
            })?;
        }

        // Keyboard. Poll longer when idle: a keypress wakes the poll
        // immediately, so the longer timeout costs no input latency but
        // avoids spinning the loop while nothing is happening.
        let poll_ms = if busy { 60 } else { 250 };
        if event::poll(Duration::from_millis(poll_ms))? {
            let ev = event::read()?;
            // Any event may change the view (caret, popup, …).
            dirty = true;
            if let Event::Key(key) = ev {
                // Accept key repeats too: on Windows, crossterm reports held
                // keys as Press once then Repeat — filtering to Press only
                // makes held Backspace/arrows delete/move a single char.
                // Release events are still ignored (they'd double-fire).
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                // The slash popup gets first crack while open: Up/Down move
                // its selection instead of the caret, Tab/Enter complete
                // WITHOUT sending, Esc closes it WITHOUT quitting. Modifier
                // chords and all other keys fall through to the normal
                // handlers untouched.
                if popup.is_open() {
                    match popup_key(&mut popup, key.code, key.modifiers) {
                        Some(PopupAction::Consumed) => continue,
                        Some(PopupAction::Dismiss) => {
                            popup.dismiss(&input.text());
                            continue;
                        }
                        Some(PopupAction::Complete(c)) => {
                            input.set_text(&c);
                            popup.dismiss(&c);
                            continue;
                        }
                        None => {}
                    }
                }
                match (key.code, key.modifiers) {
                    // Esc / Ctrl-C: cancel the in-flight turn and stay in the
                    // TUI. When idle the two now DIVERGE: Esc clears the input
                    // box (it quit before, which was easy to hit by accident
                    // right after a run — quit via /quit or /exit now), while
                    // Ctrl-C still quits. raw mode turns Ctrl-C into a normal
                    // key. The transcript lives in native scrollback, so quitting
                    // just releases the live region — the conversation stays
                    // selectable above it.
                    (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        if busy {
                            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                            // End the current assistant bubble: a System row is
                            // now last, so trailing TextDelta events still in
                            // flight start a fresh Assistant row.
                            streaming = false;
                            sub_streaming_name = None;
                            tool_running.clear();
                            rows.push(Row::sys_warn("[cancelling…]"));
                            // Cancelling stops the whole line: queued messages
                            // are dropped too, and their ghost rows go with them.
                            if !queued.is_empty() {
                                rows.push(Row::sys_warn(format!(
                                    "[dropped {} queued message(s)]",
                                    queued.len()
                                )));
                                queued.clear();
                                if drop_queued_ghosts(&mut rows) {
                                    md_cache.clear();
                                }
                            }
                        } else if matches!(key.code, KeyCode::Char('c')) {
                            // Ctrl-C in idle quits (Esc no longer does — use
                            // /quit, /exit, or Ctrl-C).
                            let _ = in_tx.send(None);
                            return Ok(queued.len());
                        } else {
                            // Esc in idle: clear the input box instead of
                            // quitting. Quit via /quit or /exit.
                            input.drain_to_string();
                        }
                    }
                    // Alt/Shift+Enter inserts a newline (portable in raw mode;
                    // every terminal reports KeyModifiers::SHIFT truthfully).
                    (KeyCode::Enter, KeyModifiers::ALT) | (KeyCode::Enter, KeyModifiers::SHIFT) => {
                        input.newline();
                    }
                    // Enter sends (queues while busy). The input stays editable
                    // during a run.
                    (KeyCode::Enter, _) => {
                        if !input.is_empty() {
                            // Strip control chars once at submission, so what
                            // enters rows / input history / the wire is clean.
                            let trimmed = strip_controls(input.text().trim());
                            if !trimmed.is_empty() {
                                match classify(&trimmed) {
                                    SlashCmd::Message => {
                                        if history.last().map(String::as_str) != Some(&trimmed) {
                                            history.push(trimmed.clone());
                                        }
                                        hist_idx = None;
                                        draft.clear();
                                        input.drain_to_string();
                                        if busy {
                                            // Queue behind the in-flight run. A
                                            // dim Row::Queued ghost marks the
                                            // spot; the flush swaps it for the
                                            // real Row::User only when it sends,
                                            // keeping the transcript in true send
                                            // order. The queue depth shows in the
                                            // input box title.
                                            queued.push_back(trimmed.clone());
                                            rows.push(Row::Queued(trimmed));
                                        } else {
                                            rows.push(Row::User(trimmed.clone()));
                                            // Deliver before flipping busy. If the
                                            // worker is gone (panicked mid-run in a
                                            // debug build, or tearing down) the send
                                            // fails — don't leave the UI stuck in
                                            // "running" waiting for a RunFinished that
                                            // will never arrive.
                                            if in_tx.send(Some(Input::Text(trimmed))).is_err() {
                                                busy = false;
                                                // The message never reached the agent —
                                                // drop it from the transcript so the
                                                // on-screen history matches what the agent
                                                // actually saw.
                                                if matches!(rows.last(), Some(Row::User(_))) {
                                                    rows.pop();
                                                }
                                                rows.push(Row::sys_error(
                                                    "[worker stopped — message not delivered; \
                                                     type /quit to exit]",
                                                ));
                                            } else {
                                                busy = true;
                                                run_start = Some(Instant::now());
                                            }
                                        }
                                    }
                                    SlashCmd::Clear if busy => {
                                        // Clearing mid-run would yank history
                                        // out from under the in-flight agent;
                                        // refuse instead of queueing it.
                                        input.drain_to_string();
                                        rows.push(Row::sys_warn(
                                            "[busy — /clear is disabled while running]",
                                        ));
                                    }
                                    SlashCmd::Clear => {
                                        // Slash command — handled in the UI,
                                        // never sent to the model. Cleared from
                                        // the input box but NOT recorded in input
                                        // history (so Alt+Up recalls messages,
                                        // not commands). Already-committed
                                        // scrollback is left in place (it's
                                        // history the user can still scroll to);
                                        // only the live buffer resets.
                                        input.drain_to_string();
                                        rows.clear();
                                        md_cache.clear();
                                        committed = 0;
                                        in_tokens = 0;
                                        out_tokens = 0;
                                        cache_create = 0;
                                        cache_read = 0;
                                        // Send before claiming success. If the worker is gone
                                        // (panicked mid-run in a debug build, or tearing down)
                                        // the send fails and the agent-side history is NOT
                                        // cleared — report that honestly, mirroring the text
                                        // path. The local UI reset above still applies.
                                        let cleared = in_tx.send(Some(Input::Clear)).is_ok();
                                        rows.push(if cleared {
                                            Row::sys_info("[history cleared]")
                                        } else {
                                            Row::sys_error(
                                                "[worker stopped — UI cleared but session \
                                                 history not cleared]",
                                            )
                                        });
                                    }
                                    SlashCmd::Quit => {
                                        // /quit (or /exit): leave the TUI. Works
                                        // mid-run too — the quit path cancels the
                                        // in-flight turn and tears down. Esc in
                                        // idle no longer quits, so this (or
                                        // Ctrl-C) is the intentional way out.
                                        input.drain_to_string();
                                        let _ = in_tx.send(None);
                                        return Ok(queued.len());
                                    }
                                    SlashCmd::Export(arg) => {
                                        // /export dumps the conversation as text:
                                        // with a path → that file; without → the
                                        // clipboard if a tool is available, else
                                        // a default file. (Native selection now
                                        // works too, but /export stays for the
                                        // machine-readable whole-transcript dump.)
                                        input.drain_to_string();
                                        let n = rows.len();
                                        let text = render_transcript(&rows);
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
                                                    Ok(_) => Row::sys_info(format!(
                                                        "[exported {n} rows to {dest}]"
                                                    )),
                                                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                                        Row::sys_error(format!(
                                                            "[export to {dest} failed: file already \
                                                             exists — /export won't overwrite; remove \
                                                             it or choose another path]"
                                                        ))
                                                    }
                                                    Err(e) => Row::sys_error(format!(
                                                        "[export to {dest} failed: {e}]"
                                                    )),
                                                }
                                            }
                                            None => {
                                                if copy_to_clipboard(&text) {
                                                    Row::sys_info(format!(
                                                        "[copied {n} rows to clipboard]"
                                                    ))
                                                } else {
                                                    let path = "tinyagent-export.md";
                                                    match std::fs::write(path, &text) {
                                                        Ok(_) => Row::sys_info(format!(
                                                            "[no clipboard tool found; \
                                                             exported {n} rows to {path}]"
                                                        )),
                                                        Err(e) => Row::sys_error(format!(
                                                            "[export failed: {e}]"
                                                        )),
                                                    }
                                                }
                                            }
                                        };
                                        rows.push(row);
                                    }
                                    cmd => {
                                        input.drain_to_string();
                                        rows.push(Row::sys_info(slash_reply(&cmd, cfg)));
                                    }
                                }
                            }
                        }
                    }
                    // Line-editing shortcuts. The Alt+Left/Right word motions
                    // must precede the plain Left/Right arms below (match arms
                    // are tried in order); the Ctrl chords likewise precede
                    // the plain-char arm at the bottom.
                    (KeyCode::Left, KeyModifiers::ALT) => input.word_left(),
                    (KeyCode::Right, KeyModifiers::ALT) => input.word_right(),
                    (KeyCode::Char('w'), KeyModifiers::CONTROL) => input.delete_word_back(),
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => input.kill_to_line_start(),
                    (KeyCode::Char('k'), KeyModifiers::CONTROL) => input.kill_to_line_end(),
                    (KeyCode::Char('a'), KeyModifiers::CONTROL) => input.home(),
                    (KeyCode::Char('e'), KeyModifiers::CONTROL) => input.end(),
                    (KeyCode::Backspace, _) => input.backspace(),
                    (KeyCode::Left, _) => input.move_left(),
                    (KeyCode::Right, _) => input.move_right(),
                    // Alt+Up/Down recall input history; Up/Down move the caret
                    // across lines inside the buffer.
                    (KeyCode::Up, KeyModifiers::ALT) => {
                        if !history.is_empty() {
                            match hist_idx {
                                None => {
                                    draft = input.text();
                                    hist_idx = Some(history.len() - 1);
                                }
                                Some(0) => {}
                                Some(i) => hist_idx = Some(i - 1),
                            }
                            if let Some(i) = hist_idx {
                                input.set_text(&history[i]);
                            }
                        }
                    }
                    (KeyCode::Down, KeyModifiers::ALT) => {
                        if let Some(i) = hist_idx {
                            if i + 1 < history.len() {
                                hist_idx = Some(i + 1);
                                input.set_text(&history[i + 1]);
                            } else {
                                hist_idx = None;
                                input.set_text(&draft);
                            }
                        }
                    }
                    (KeyCode::Up, _) => input.move_up(),
                    (KeyCode::Down, _) => input.move_down(),
                    (KeyCode::Home, _) => input.home(),
                    (KeyCode::End, _) => input.end(),
                    // PageUp/PageDown are intentionally not handled: with mouse
                    // capture off, the terminal owns scrollback paging (and the
                    // wheel). There is no app-managed transcript to page.
                    (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                        input.put_char(c);
                    }
                    _ => {}
                }
            } else if let Event::Paste(s) = ev {
                // Bracketed paste: the whole blob (newlines included) arrives
                // as one event and is inserted as input — not split into many
                // Enter-triggered sends the way a raw paste would be. Control
                // chars (ESC/CSI) are stripped; \n and \t survive.
                input.insert_str(&strip_controls(&s));
            }
            // No mouse handler: with capture off, the terminal owns wheel
            // scroll and click-drag-select over the committed scrollback — both
            // "just work" on the real transcript, which is the whole point of
            // the inline model.
        }
    }
}

/// Render a role body with the icon inline on the first line: `❯ hello`
/// instead of a standalone `❯ You` header row. Continuation lines indent to
/// the same text column, so the layout keeps its alignment but every message
/// saves one screen row. `suffix` (e.g. the queued ghost's ` · queued`) rides
/// on the first line. An empty body still shows the bare icon.
fn push_role(
    lines: &mut Vec<Line<'static>>,
    icon: &str,
    icon_style: Style,
    text: &str,
    body_style: Style,
    suffix: Option<(&str, Style)>,
) {
    let mut body = text.lines();
    let first = Line::from(vec![
        Span::styled(icon.to_string(), icon_style),
        Span::styled(body.next().unwrap_or_default().to_string(), body_style),
    ]);
    let mut first = first;
    if let Some((s, style)) = suffix {
        first.spans.push(Span::styled(s.to_string(), style));
    }
    lines.push(first);
    for l in body {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(ROLE_BODY_INDENT)),
            Span::styled(l.to_string(), body_style),
        ]));
    }
}

/// Indent rendered Markdown or ANSI lines without flattening their spans.
fn indent_lines(lines: &mut [Line<'static>], width: usize) {
    for line in lines {
        line.spans.insert(0, Span::raw(" ".repeat(width)));
    }
}

/// Push a tool head line. An over-long head (a wide collapsed summary on a
/// narrow terminal) wraps with a HANGING indent: continuation rows align
/// under the summary column (`hang`) instead of falling back to column 0,
/// which would break the tool gutter. Selection styling is the caller's
/// post-pass over the row's lines, not this function's concern.
fn push_tool_head(lines: &mut Vec<Line<'static>>, head: Line<'static>, hang: usize, width: usize) {
    // wrap_line rebuilds Lines with a default line-level style; carry the
    // head's.
    let line_style = head.style;
    let mut rows = wrap_line(&head, width);
    // wrap_line always yields at least one row.
    let mut wrapped: Vec<Line<'static>> = vec![rows.remove(0)];
    for row in rows {
        // Continuation rows carry the indent, so they re-wrap narrower.
        for mut cont in wrap_line(&row, width.saturating_sub(hang).max(1)) {
            cont.spans.insert(0, Span::raw(" ".repeat(hang)));
            wrapped.push(cont);
        }
    }
    for row in wrapped.iter_mut() {
        row.style = line_style;
    }
    lines.extend(wrapped);
}

/// Build styled history lines for the conversation. Used two ways: to render
/// finalized rows into the terminal scrollback (via `commit_rows`), and to
/// render the in-flight streaming row live in the viewport zone. Split out of
/// the draw/commit paths so role hierarchy, colors, `│ ` guides, the streaming
/// cursor, and the empty placeholder are unit-testable without a terminal.
///
/// `inner_w` is the content width.
/// `streaming` appends a `▌` cursor to the in-flight assistant row's last
/// line — the lines are clones out of MdCache, so the cached render stays
/// clean.
/// `thinking` synthesizes one transient `● …` tail line while the agent has
/// started a turn but produced no visible output yet. It is render-only: not
/// a row, so MdCache and the transcript are untouched.
#[allow(clippy::too_many_arguments)]
fn build_history_lines(
    rows: &[Row],
    md_cache: &mut MdCache,
    inner_w: usize,
    streaming: bool,
    thinking: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() && !thinking {
        // Empty conversation (post-/clear): the guide line plus a two-line
        // key cheat-sheet — cheapest onboarding real estate there is.
        let style = Style::default().fg(DIM_FG).add_modifier(Modifier::ITALIC);
        for l in [
            "  no messages yet — type below; /help lists commands",
            "  / completes commands · Enter sends",
            "  scroll / select / copy are native to your terminal",
        ] {
            lines.push(Line::from(Span::styled(l.to_string(), style)));
        }
        return lines;
    }
    let marker = "    ";
    for (idx, row) in rows.iter().enumerate() {
        match row {
            Row::User(t) => {
                push_role(
                    &mut lines,
                    "❯ ",
                    Style::default().fg(USER_FG).add_modifier(Modifier::BOLD),
                    t,
                    Style::default(),
                    None,
                );
            }
            Row::Queued(t) => {
                // Ghost for a message waiting behind the in-flight run: same
                // user shape, fully dimmed, with a `· queued` tag on the
                // first line so it can't be mistaken for a sent message.
                push_role(
                    &mut lines,
                    "❯ ",
                    Style::default().fg(DIM_FG).add_modifier(Modifier::BOLD),
                    t,
                    Style::default().fg(DIM_FG),
                    Some((" · queued", Style::default().fg(DIM_FG))),
                );
            }
            Row::Assistant(t) => {
                let markdown_width = inner_w.saturating_sub(ROLE_BODY_INDENT).max(1);
                // M6: reuse the cached render unless this row's text
                // grew since last frame (streaming). Prior rows and
                // scroll redraws hit the cache; only the in-flight row
                // (or a first render) re-parses markdown. The cache is keyed by
                // (text, width), not row index — see MdCache docs for why.
                let mut rendered = md_cache
                    .get_or_render(t, markdown_width, |txt| {
                        render_markdown(txt, markdown_width)
                            .iter()
                            .map(line_to_owned)
                            .collect()
                    })
                    .to_vec();
                if rendered.is_empty() {
                    rendered.push(Line::default());
                }
                indent_lines(&mut rendered, ROLE_BODY_INDENT);
                // Inline role icon: the first body line's indent becomes the
                // `● ` marker — same column, one row saved, no header line.
                rendered[0].spans[0] = Span::styled(
                    "● ".to_string(),
                    Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
                );
                lines.extend(rendered);
                // Streaming cursor on the live assistant row.
                if streaming && idx == rows.len() - 1 {
                    if let Some(last) = lines.last_mut() {
                        last.spans
                            .push(Span::styled("▌".to_string(), Style::default().fg(AGENT_FG)));
                    }
                }
            }
            Row::ToolCall {
                name,
                input,
                collapsed,
                expanded,
                ..
            } => {
                let name_span = format!("{marker}→ {name}");
                let has_summary = !*expanded && !collapsed.is_empty();
                let head = Line::from(vec![
                    Span::styled(
                        name_span.clone(),
                        Style::default()
                            .fg(TOOL_CALL_FG)
                            .add_modifier(Modifier::BOLD),
                    ),
                    if has_summary {
                        Span::styled(
                            format!("  {}", collapsed.lines().next().unwrap_or_default()),
                            Style::default().fg(DIM_FG),
                        )
                    } else {
                        Span::raw(String::new())
                    },
                ]);
                // Continuation rows align under the summary (or the name when
                // bare); selection styling is the post-pass below.
                let hang = name_span.width() + if has_summary { 2 } else { 0 };
                push_tool_head(&mut lines, head, hang, inner_w);
                // Tool-call input is JSON sanitized at entry (ANSI inside it
                // is meaningless); ansi_guided_lines is a pass-through here
                // but keeps one uniform body path with ToolResult.
                if *expanded {
                    let body =
                        serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
                    let mut body = ansi_guided_lines(&body, Style::default().fg(DIM_FG));
                    indent_lines(&mut body, TOOL_INDENT);
                    lines.extend(body);
                }
            }
            Row::ToolResult {
                name,
                content,
                collapsed,
                is_error,
                expanded,
                elapsed,
            } => {
                let (tag, head_fg) = if *is_error {
                    ("✗", TOOL_ERR_FG)
                } else {
                    ("✓", TOOL_OK_FG)
                };
                // The ok head stays UNBOLD: a green bold "✓ shell" reads as a
                // second "● " agent icon in a long execution flow; only the
                // failure glyph earns the emphasis.
                let head_style = if *is_error {
                    Style::default().fg(head_fg).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(head_fg)
                };
                let dur = elapsed
                    .as_deref()
                    .map(|e| format!(" · {e}"))
                    .unwrap_or_default();
                let name_span = format!("{marker}{tag} {name}{dur}");
                let has_summary = !*expanded && !collapsed.is_empty();
                let head = Line::from(vec![
                    Span::styled(name_span.clone(), head_style),
                    if has_summary {
                        let preview = ansi_plain(collapsed);
                        Span::styled(
                            format!("  {}", preview.lines().next().unwrap_or_default()),
                            Style::default().fg(if *is_error { TOOL_ERR_FG } else { DIM_FG }),
                        )
                    } else {
                        Span::raw(String::new())
                    },
                ]);
                let hang = name_span.width() + if has_summary { 2 } else { 0 };
                push_tool_head(&mut lines, head, hang, inner_w);
                // Tool output keeps its ANSI colors: content arrives raw (the
                // parser strips non-SGR sequences and bare control chars, so
                // this path no longer needs strip_controls at entry).
                if *expanded {
                    let body_style = if *is_error {
                        Style::default().fg(TOOL_ERR_FG)
                    } else {
                        Style::default().fg(DIM_FG)
                    };
                    let mut body = ansi_guided_lines(content, body_style);
                    indent_lines(&mut body, TOOL_INDENT);
                    lines.extend(body);
                }
            }
            Row::System { text, level } => {
                let fg = match level {
                    SysLevel::Info => DIM_FG,
                    SysLevel::Warn => TOOL_CALL_FG,
                    SysLevel::Error => TOOL_ERR_FG,
                };
                // One-line layout: the "· " marker rides the first text line
                // (a bare "·" dangling on its own line read as a bug);
                // continuation lines align under it. Info notices nest in the
                // tool gutter; warn/error rise to column 0 so they stand out
                // from the execution detail they interrupt.
                let (marker_pad, cont_pad) = match level {
                    SysLevel::Info => ("    · ", "      "),
                    _ => ("· ", "  "),
                };
                let marker_style = Style::default().fg(fg).add_modifier(Modifier::BOLD);
                let body_style = Style::default().fg(fg);
                let mut it = text.lines();
                lines.push(Line::from(vec![
                    Span::styled(marker_pad.to_string(), marker_style),
                    Span::styled(it.next().unwrap_or("").to_string(), body_style),
                ]));
                for l in it {
                    lines.push(Line::from(Span::styled(
                        format!("{cont_pad}{l}"),
                        body_style,
                    )));
                }
            }
        }
        // Turn boundary: a blank spacer separates this execution flow from the
        // next user message / queued ghost / system line.
        let next = rows.get(idx + 1);
        if next.is_none()
            || matches!(
                next,
                Some(Row::User(_) | Row::Queued(_) | Row::System { .. })
            )
        {
            lines.push(Line::default());
        }
    }
    // Transient thinking placeholder: the agent's icon plus an ellipsis while
    // a turn has started but nothing visible has streamed yet.
    if thinking {
        lines.push(Line::from(vec![
            Span::styled(
                "● ",
                Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled("…", Style::default().fg(DIM_FG)),
        ]));
    }
    lines
}

/// Remove the oldest ghost Row::Queued (FIFO, matching `queued.pop_front()`).
/// Returns true when a row was removed — row indices then shifted, so the
/// caller must clear the index-keyed MdCache.
fn remove_oldest_queued(rows: &mut Vec<Row>) -> bool {
    let pos = rows.iter().position(|r| matches!(r, Row::Queued(_)));
    if let Some(i) = pos {
        rows.remove(i);
    }
    pos.is_some()
}

/// Remove every ghost Row::Queued (the whole queue was dropped). Returns true
/// when any were removed, for the same MdCache-staleness reason as above.
fn drop_queued_ghosts(rows: &mut Vec<Row>) -> bool {
    let had = rows.iter().any(|r| matches!(r, Row::Queued(_)));
    rows.retain(|r| !matches!(r, Row::Queued(_)));
    had
}

/// Stop reasons that mean "the turn ended normally", across both provider
/// families (Anthropic: end_turn / tool_use / stop_sequence; OpenAI: stop /
/// tool_calls). Only abnormal stops (max_tokens / length / error / unknown)
/// get a System row — a routine end needs no noise.
fn is_normal_stop(reason: &str) -> bool {
    matches!(
        reason,
        "end_turn" | "tool_use" | "stop" | "tool_calls" | "stop_sequence"
    )
}

/// The startup welcome line: model and tool count, and the /help pointer.
fn welcome_text(model: &str, n_tools: usize) -> String {
    format!("tiny-agent · model {model} · {n_tools} tools · /help lists commands")
}

/// Wall-clock duration of a tool call, pre-formatted for the result header/// ("1.2s"). Matches the NEAREST same-name ToolCall row scanning backwards:
/// parallel sub-agents interleave their calls, so a name can repeat and
/// nearest-back is the best available approximation. None when no same-name
/// call row exists (e.g. a resumed session whose calls predate the row list).
fn tool_elapsed_for(rows: &[Row], name: &str) -> Option<String> {
    rows.iter().rev().find_map(|r| match r {
        Row::ToolCall {
            name: n, started, ..
        } if n == name => Some(fmt_tool_elapsed(started.elapsed())),
        _ => None,
    })
}

/// Auto-expand contract for a tool result row: a FAILED call with SHORT
/// output (a command's stderr) opens so the user reads the error right away;
/// long dumps stay collapsed so they don't flood the scrollback, and
/// successful results never auto-open.
fn auto_expand_result(is_error: bool, content: &str) -> bool {
    is_error && content.lines().count() <= 20
}

/// Compact tool duration: one decimal under 10s ("1.2s"), whole seconds at
/// 10s and above ("12s") — the last digit stops jittering on long calls.
fn fmt_tool_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{secs:.0}s")
    }
}

/// Render-only state for tool calls currently executing, mirroring the
/// `thinking` placeholder's contract: synthesized at render time, never
/// entering rows / MdCache / the transcript. The ToolCall row itself commits
/// to scrollback the moment it appears, so without this the live zone goes
/// blank for the whole execution — a 60s shell showed nothing but the
/// input-box spinner and the whole-run clock, with no per-tool timing.
struct ToolRunning {
    /// (display name, start) per in-flight call; parallel `task` fan-out can
    /// legitimately hold several at once.
    calls: Vec<(String, Instant)>,
}

impl ToolRunning {
    fn new() -> Self {
        Self { calls: Vec::new() }
    }

    /// A tool call started (parent or sub-agent arm).
    fn on_call(&mut self, name: &str) {
        self.calls.push((name.to_string(), Instant::now()));
    }

    /// A tool result arrived: remove the FIRST same-name entry. With no
    /// same-name entry (interleaved names make the matching approximate,
    /// like `tool_elapsed_for`) the oldest drops; an empty list is a no-op.
    fn on_result(&mut self, name: &str) {
        if self.calls.is_empty() {
            return;
        }
        let pos = self.calls.iter().position(|(n, _)| n == name).unwrap_or(0);
        self.calls.remove(pos);
    }

    fn clear(&mut self) {
        self.calls.clear();
    }

    fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

/// The live-zone placeholder line while tools execute: `→ name · 1.2s…`, dim
/// elapsed (read at render time so the busy clock tick keeps it moving), with
/// `(+N)` when more than one call is in flight. None when nothing runs.
fn tool_running_line(state: &ToolRunning, now: Instant) -> Option<Line<'static>> {
    let (name, started) = state.calls.first()?;
    let elapsed = fmt_tool_elapsed(now.checked_duration_since(*started).unwrap_or_default());
    let more = state.calls.len().saturating_sub(1);
    let tail = if more > 0 {
        format!(" · {elapsed}… (+{more})")
    } else {
        format!(" · {elapsed}…")
    };
    Some(Line::from(vec![
        Span::styled(
            format!("→ {name}"),
            Style::default()
                .fg(TOOL_CALL_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(tail, Style::default().fg(DIM_FG)),
    ]))
}

/// Session/run elapsed for the status bar: "Ns" under a minute, else "MmSSs"
/// ("4m05s") so long runs stay readable without growing the segment.
fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Ghost placeholder shown in the empty input box. Keeps the /help pointer:
/// the welcome line scrolls away, this is the standing discoverability hint.
const INPUT_PLACEHOLDER: &str = "Message TinyAgent… (/help)";

/// The one color for the bottom chrome (input border + its title): AGENT_FG
/// while a run is in flight so the frame reads as a single busy unit, the
/// idle color otherwise. One busy color across chrome AND status bar.
fn chrome_fg(busy: bool) -> Color {
    if busy {
        AGENT_FG
    } else {
        USER_FG
    }
}

/// Input border title for idle state or the current in-flight run. `width` is
/// the room the border row offers (input box width minus the two corners).
/// ratatui clips an over-long title from the RIGHT — silently dropping the
/// elapsed time, the cancel hint, and the queue count, exactly the actionable
/// segments — so a busy title that would overflow degrades here instead: the
/// activity name goes first (the least actionable part), keeping spinner +
/// elapsed + cancel + queue.
fn build_input_title(
    busy: bool,
    spinner: char,
    activity: &str,
    secs: u64,
    queued: usize,
    width: usize,
) -> String {
    if busy {
        let queue = if queued == 0 {
            String::new()
        } else {
            format!(" · {queued} queued")
        };
        let tail = format!("{} · Ctrl-C cancel{queue} ", fmt_elapsed(secs));
        let full = format!(" {spinner} {activity}… · {tail}");
        if full.width() <= width {
            full
        } else {
            format!(" {spinner} {tail}")
        }
    } else {
        " Ask TinyAgent ".to_string()
    }
}

/// Assemble the status bar in two density levels. Wide terminals get token
/// detail; medium ones keep the context-window segment (`ctx`) after the stats
/// drop out (it's the most decision-relevant number, so it outlives the stats);
/// narrow ones preserve only the model badge and broad run state. The cwd path
/// pins to the right edge in whatever room the left side leaves (it is the
/// first thing narrow terminals lose). Keyboard help lives behind `/help`.
// The bar is assembled from independent segments; grouping them into a
// struct would only shuffle the same fields to every call site.
#[allow(clippy::too_many_arguments)]
fn build_status_line(
    model: &str,
    state: &str,
    busy: bool,
    ctx: Option<(&str, Style)>,
    plan: Option<(&str, Style)>,
    stats: &str,
    path: &str,
    width: usize,
) -> Line<'static> {
    let badge = format!(" {model} ");
    let state_seg = format!("  {state}");
    let state_style = if busy {
        // One busy color across the chrome: the input border title already
        // pops AGENT_FG, the status state segment matches it.
        Style::default().fg(AGENT_FG).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM_FG)
    };
    let mut spans = vec![
        Span::styled(badge, Style::default().fg(BADGE_FG).bg(BADGE_BG)),
        Span::styled(state_seg, state_style),
    ];
    let used = spans.iter().map(|s| s.content.width()).sum::<usize>();
    // Optional segments in SURVIVAL order (highest priority first): ctx, plan,
    // stats. Each is added only if it AND every higher-priority segment fit, so
    // narrowing drops right-to-left — stats first, then plan, then ctx (ctx
    // survives longest, matching the documented "ctx 保留到最后"). `plan` is
    // None unless the agent used `todo`, so a no-plan run is byte-identical to
    // before the segment existed.
    let mut opt: Vec<(String, Style)> = Vec::new();
    if let Some((t, s)) = ctx {
        opt.push((format!(" · {t}"), s));
    }
    if let Some((t, s)) = plan {
        opt.push((t.to_string(), s));
    }
    opt.push((stats.to_string(), Style::default().fg(DIM_FG)));
    let mut prefix_w = 0usize;
    for (text, style) in &opt {
        prefix_w += text.width();
        if used + prefix_w <= width {
            spans.push(Span::styled(text.clone(), *style));
        } else {
            // Lower-priority segments won't fit either — stop.
            break;
        }
    }
    // Right-aligned cwd path in the room the left side left over.
    let left_w = spans.iter().map(|s| s.content.width()).sum::<usize>();
    if let Some(seg) = right_path_seg(path, width.saturating_sub(left_w)) {
        spans.push(seg);
    }
    Line::from(spans)
}

/// Right-aligned cwd path segment for the status bar. Returns None when the
/// leftover after the left segments is under 12 columns (anything less is
/// useless truncation), when the path is empty, or when even "…" + the
/// basename cannot fit with a 2-column gap. Long paths are LEFT-truncated
/// with a leading "…" on a char boundary, so the most identifying tail —
/// the project directory — stays visible.
fn right_path_seg(path: &str, avail: usize) -> Option<Span<'static>> {
    if path.is_empty() || avail < 12 {
        return None;
    }
    let budget = avail - 2; // minimum gap to the left segments
    let pw = path.width();
    let (text, tw) = if pw <= budget {
        (path.to_string(), pw)
    } else {
        let base = path.rsplit('/').next().unwrap_or(path);
        if base.width() + 1 > budget {
            return None; // even "…" + basename is too wide
        }
        // Longest char-boundary suffix that fits next to the "…" marker.
        let mut w = 0;
        let mut start = 0; // 0 = whole path fits (unreachable here, defensive)
        for (i, c) in path.char_indices().rev() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw + 1 > budget {
                break;
            }
            w += cw;
            start = i;
        }
        if start == 0 {
            (path.to_string(), pw)
        } else {
            (format!("…{}", &path[start..]), w + 1)
        }
    };
    let pad = avail - tw;
    Some(Span::styled(
        format!("{}{text}", " ".repeat(pad)),
        Style::default().fg(DIM_FG),
    ))
}

/// Wrap logical history lines to the render width. The inline model commits
/// finalized lines to native scrollback, so there is no per-visual-line row
/// map to maintain — wrapping is purely about fitting text to the width.
fn wrap_history(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    lines.iter().flat_map(|l| wrap_line(l, width)).collect()
}

/// Render width for transcript text: the live viewport spans the terminal, so
/// there is no horizontal inset, but prose is still capped at a readable max.
fn content_width(term_w: u16) -> usize {
    (term_w as usize).min(CONTENT_MAX_WIDTH as usize).max(1)
}

/// Render finalized rows to text and commit them into the terminal scrollback
/// above the inline viewport via `insert_before`. The live region slides down
/// by exactly the rendered height; the host terminal's native
/// selection/scroll/search then own those lines.
/// How far the commit watermark may advance from `committed`. Two things are
/// held back, never written into the terminal's permanent scrollback:
///
/// 1. The **first `Row::Queued` ghost**, and therefore everything after it. A
///    queued ghost is a placeholder for a not-yet-sent user message; while it
///    waits, the in-flight run keeps appending real rows after it (tool calls,
///    tool results, warnings, compress notices, sub-agent rows). The old logic
///    only stripped *trailing* `Queued` rows, so the moment any real row landed
///    after a ghost the ghost was no longer trailing and got committed —
///    permanently leaving a dim placeholder in scrollback, AND breaking the
///    watermark's own invariant ("only uncommitted ghosts are ever removed"):
///    `flush_next_queued` would later `remove` that already-committed row,
///    shifting indices under `committed` and silently dropping the next real
///    row. Capping at the *first* ghost fixes both: the ghost and every later
///    row stay out of scrollback until the queue flushes (flush removes only
///    this still-uncommitted ghost, so the index invariant stays sound).
/// 2. While a stream is open, a trailing **`Row::Assistant`** — the in-flight
///    reply is still mutating and is shown live in the viewport, not committed
///    until the stream closes.
fn commit_pending_end(rows: &[Row], committed: usize, stream_open: bool) -> usize {
    // Cap at the first queued ghost (holds the ghost + everything after back).
    let mut pending_end = match rows[committed..]
        .iter()
        .position(|r| matches!(r, Row::Queued(_)))
    {
        Some(i) => committed + i,
        None => rows.len(),
    };
    // Hold back an in-flight trailing Assistant while its stream is open.
    if stream_open && pending_end > committed && matches!(rows[pending_end - 1], Row::Assistant(_))
    {
        pending_end -= 1;
    }
    pending_end
}

/// Index of the row to render live in the viewport's transcript zone while a
/// stream is open, else None. This is the row `append_delta` is currently
/// mutating — the LAST `Row::Assistant` in `rows` — which a queued ghost may
/// sit behind. It is deliberately INDEPENDENT of `commit_pending_end`: that
/// watermark caps at the first queued ghost, so when a new turn streams AFTER
/// a ghost, the watermark points at the stale pre-ghost Assistant, not the
/// live one. Deriving the in-flight index from the watermark (the old code)
/// rendered that stale row and hid the real streaming output.
fn inflight_row_index(rows: &[Row], stream_open: bool) -> Option<usize> {
    if stream_open {
        rows.iter().rposition(|r| matches!(r, Row::Assistant(_)))
    } else {
        None
    }
}

fn commit_rows<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    rows: &[Row],
    md_cache: &mut MdCache,
    width: usize,
) -> Result<(), B::Error> {
    let lines = build_history_lines(rows, md_cache, width, false, false);
    let wrapped = wrap_history(&lines, width);
    if wrapped.is_empty() {
        return Ok(());
    }
    // Cap at u16::MAX: insert_before's height is a u16, and `y as u16` in the
    // loop would wrap past 65,535 rendered lines — silently reserving too few
    // rows while the loop overwrites past them. A single commit reaching that
    // size is not realistic (a ~65k-line turn), but capping drops the
    // unreachable tail instead of corrupting the layout.
    let n = wrapped.len().min(u16::MAX as usize);
    let h = n as u16;
    term.insert_before(h, |buf: &mut Buffer| {
        for (y, line) in wrapped[..n].iter().enumerate() {
            // set_line writes the line's spans from x=0, truncating at the
            // buffer's width — which equals `width` (or wider), so the
            // pre-wrapped lines never overflow.
            buf.set_line(0, y as u16, line, width as u16);
        }
    })?;
    Ok(())
}

/// Top-anchored render for a menu (the slash popup): line 0 sits at the top of
/// `rect`. The caller windows `lines` down to the zone height first (see
/// `slash_popup_lines`), keeping the selection on-screen — so this just paints
/// top-down with no scroll, and the highlighted row can never be the one scrolled
/// out of view.
fn render_zone_top(f: &mut Frame, rect: Rect, lines: &[Line<'static>]) {
    if rect.height == 0 || lines.is_empty() {
        return;
    }
    f.render_widget(Paragraph::new(lines.to_vec()), rect);
}

/// Bottom-align `lines` inside `rect` (the transcript zone above the input box):
/// the last line sits flush against the input box. Used for the thinking
/// placeholder and the in-flight streaming row. When the content FITS it is
/// padded down to the bottom of the zone; when it OVERFLOWS, a dim `↑ +N`
/// indicator caps the top and the tail fills the rest — so it's clear the live
/// answer is taller than the band (the full text commits to scrollback once the
/// turn ends, and is scrollable there).
fn render_zone_tail(f: &mut Frame, rect: Rect, lines: &[Line<'static>]) {
    if rect.height == 0 || lines.is_empty() {
        return;
    }
    let n = lines.len();
    let h = rect.height as usize;
    if n <= h {
        // Fits: pad the top so the last line lands flush above the input box.
        let pad = (h - n) as u16;
        let body = if pad > 0 {
            let [_, b] =
                Layout::vertical([Constraint::Length(pad), Constraint::Min(0)]).areas(rect);
            b
        } else {
            rect
        };
        f.render_widget(Paragraph::new(lines.to_vec()), body);
        return;
    }
    // Overflow: a one-row truncation hint at the top, the tail below it. The
    // hint occupies one of the h rows, so the content rows left are h-1 — the
    // hidden count is what the tail CAN'T show.
    let hidden = n - (h - 1);
    let hint = Line::from(Span::styled(
        format!(
            "↑ +{hidden} line{} above",
            if hidden == 1 { "" } else { "s" }
        ),
        Style::default().fg(DIM_FG).add_modifier(Modifier::ITALIC),
    ));
    let [hint_rect, tail_rect] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(rect);
    f.render_widget(Paragraph::new(hint), hint_rect);
    let take = (tail_rect.height as usize).min(n);
    f.render_widget(Paragraph::new(lines[n - take..].to_vec()), tail_rect);
}

/// Strip control characters (escape sequences, NUL, …) from external text
/// before it enters the row list — tool output or model text containing ESC /
/// CSI sequences would otherwise corrupt the TUI frame. Newlines and tabs are
/// kept (both are legitimate content here). Applied once at the row-entry
/// points, never on the render path.
fn strip_controls(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// strip_controls for every string inside a JSON value (tool-call input), so
/// both the collapsed summary and the expanded pretty-print come out clean
/// without re-filtering on the render path.
fn strip_controls_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(strip_controls(s)),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(strip_controls_json).collect())
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), strip_controls_json(v)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

/// Append a streamed parent-agent text delta to the trailing Assistant row —
/// opening a NEW Assistant row when the tail is anything else (a ghost
/// Row::Queued, a System notice, a tool row pushed mid-stream). The old code
/// dropped the delta in that case, silently losing output. Control characters
/// are stripped here, once, at the row entry point.
fn append_delta(rows: &mut Vec<Row>, streaming: &mut bool, text: &str) {
    if !*streaming || !matches!(rows.last(), Some(Row::Assistant(_))) {
        rows.push(Row::Assistant(String::new()));
        *streaming = true;
    }
    if let Some(Row::Assistant(buf)) = rows.last_mut() {
        buf.push_str(&strip_controls(text));
    }
}

/// Sub-agent counterpart of append_delta: consecutive deltas from the same
/// named sub-agent merge into one `[{name}] `-prefixed bubble; a different
/// sub-agent — or a tail row that isn't an Assistant bubble (a notice or
/// ghost pushed mid-stream) — starts a fresh bubble instead of dropping the
/// delta (the old silent-drop bug).
fn append_sub_delta(
    rows: &mut Vec<Row>,
    sub_streaming_name: &mut Option<String>,
    name: &str,
    text: &str,
) {
    let merge = sub_streaming_name.as_deref() == Some(name)
        && matches!(rows.last(), Some(Row::Assistant(_)));
    if !merge {
        rows.push(Row::Assistant(format!("[{name}] ")));
        *sub_streaming_name = Some(name.to_string());
    }
    if let Some(Row::Assistant(buf)) = rows.last_mut() {
        buf.push_str(&strip_controls(text));
    }
}

/// What happened when the queue was flushed after a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flush {
    /// Nothing was queued — the session goes idle.
    Empty,
    /// The oldest queued message was delivered; the caller marks the new run
    /// busy.
    Sent,
    /// Popped but the worker is gone — the message could not be delivered
    /// (a sys_error row records it); the session goes idle.
    Undeliverable,
}

/// Pop the oldest queued message after a run ended and start the next run
/// with it. Shared by the Error and RunFinished arms, whose flush blocks were
/// verbatim duplicates. Contract: pop_front ↔ remove the oldest ghost row ↔
/// append the real Row::User at the END (the finished run's trailing output
/// may already sit after the ghost, so only an append keeps true send order).
/// Row removal shifts indices, so the index-keyed MdCache is reset.
fn flush_next_queued(
    queued: &mut std::collections::VecDeque<String>,
    rows: &mut Vec<Row>,
    md_cache: &mut MdCache,
    in_tx: &mpsc::Sender<Option<Input>>,
) -> Flush {
    let Some(text) = queued.pop_front() else {
        return Flush::Empty;
    };
    if remove_oldest_queued(rows) {
        md_cache.clear();
    }
    rows.push(Row::User(text.clone()));
    if in_tx.send(Some(Input::Text(text))).is_ok() {
        Flush::Sent
    } else {
        rows.push(Row::sys_error(
            "[worker stopped — queued message not delivered; type /quit to exit]",
        ));
        Flush::Undeliverable
    }
}

/// Apply a `flush_next_queued` outcome to the run state. Shared by the Error
/// and RunFinished arms so the flush contract lives in exactly one place:
/// Sent starts the next run with NO idle gap (a Ctrl-C in that gap would
/// quit instead of cancelling), anything else drops the session to idle.
fn apply_flush(
    flush: Flush,
    busy: &mut bool,
    run_start: &mut Option<Instant>,
    activity: &mut String,
    thinking: &mut bool,
) {
    match flush {
        Flush::Sent => {
            *busy = true;
            *run_start = Some(Instant::now());
            *activity = "thinking".into();
            *thinking = true;
        }
        Flush::Undeliverable => {
            *busy = false;
            *run_start = None;
            *thinking = false;
        }
        Flush::Empty => {
            *busy = false;
            *run_start = None;
            *thinking = false;
        }
    }
}

/// Horizontal scroll for the input box: returns `(scroll_offset, caret_x)`
/// where `caret_x` is the caret's on-screen column relative to the box's
/// inner left edge, guaranteed `< inner_cols`. The input Paragraph doesn't
/// wrap, so a line wider than the box must be horizontally scrolled to keep
/// the caret visible — an unclamped caret would be drawn past the border,
/// over the conversation area (or off-screen on a narrow terminal).
fn input_hscroll(caret_col: usize, inner_cols: usize) -> (u16, u16) {
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

/// Wrap one styled line to `width` display columns, returning one `Line` per
/// visual row. Greedy word wrap: fill a row until the next word won't fit, then
/// break at the last space; a word longer than `width` is hard-broken. A space
/// landing right after a break is dropped (cleaner left edge). Span styles are
/// preserved across breaks — a span split by a wrap becomes two spans with the
/// same style. With this, `lines.len()` equals the rendered height, so the
/// scroll range is exact.
fn wrap_line(src: &Line, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    // Flatten to (char, style) so a wrap can split a span at any char.
    let cells: Vec<(char, Style)> = src
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
        .collect();
    let char_w = |c: char| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row_start = 0usize; // cells index where the current row began
    let mut col = 0usize; // display width of [row_start, i)
                          // Index just past the most recent space — a valid break point as long as
                          // there's non-space content before it on this row.
    let mut last_space_after: Option<usize> = None;

    let mut i = 0usize;
    while i < cells.len() {
        let (c, _style) = cells[i];
        let w = char_w(c);
        if c == ' ' {
            if col + w > width && col > 0 {
                // The space itself would overflow: break before it and drop it.
                rows.push(line_from_cells(&cells[row_start..i]));
                row_start = i + 1;
                col = 0;
                last_space_after = None;
                i += 1;
                continue;
            }
            col += w;
            // Only treat it as a break point if real content precedes it.
            if row_start < i {
                last_space_after = Some(i + 1);
            }
            i += 1;
            continue;
        }
        if col + w > width {
            // Overflow on a non-space char. Prefer breaking at the last space.
            if let Some(after) = last_space_after {
                if after - 1 > row_start {
                    // Row = everything before the space (space dropped).
                    rows.push(line_from_cells(&cells[row_start..after - 1]));
                    row_start = after;
                    col = cells[row_start..i].iter().map(|(ch, _)| char_w(*ch)).sum();
                    last_space_after = None;
                    continue; // re-process cell i on the new row
                }
            }
            // No usable break point: hard-break before this char.
            if col > 0 {
                rows.push(line_from_cells(&cells[row_start..i]));
                row_start = i;
                col = 0;
                last_space_after = None;
                continue; // re-process cell i
            }
            // Row empty and char wider than width (or width 1): emit it anyway
            // so we make progress instead of looping forever.
        }
        col += w;
        i += 1;
    }
    if row_start < cells.len() {
        rows.push(line_from_cells(&cells[row_start..]));
    }
    if rows.is_empty() {
        rows.push(Line::default());
    }
    rows
}

/// Build a `Line` from (char, style) cells, coalescing adjacent same-style chars
/// into one span so a wrapped row doesn't explode into one span per char.
fn line_from_cells(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    for (c, st) in cells {
        if text.is_empty() {
            text.push(*c);
            style = *st;
        } else if *st == style {
            text.push(*c);
        } else {
            spans.push(Span::styled(std::mem::take(&mut text), style));
            text.push(*c);
            style = *st;
        }
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Compact token count for the status line: 850, 1.2k, 12k, 1.0M.
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Format the status bar's context-window segment: `ctx 54.7k` when the
/// model's window size is unknown (limits.max_context_tokens unset), or
/// `ctx 21% (54.7k/256.0k)` when it is configured. The color grades with
/// fill level — dim while comfortable, yellow past 60%, red past 80% — so a
/// glance tells you how much headroom is left. None before the first Usage
/// event (nothing meaningful to show).
fn fmt_ctx(ctx_tokens: u64, max_context_tokens: u64) -> Option<(String, Style)> {
    if ctx_tokens == 0 {
        return None;
    }
    if max_context_tokens == 0 {
        return Some((
            format!("ctx {}", fmt_tokens(ctx_tokens)),
            Style::default().fg(DIM_FG),
        ));
    }
    let pct = ctx_tokens.saturating_mul(100) / max_context_tokens.max(1);
    let fg = if pct >= 80 {
        TOOL_ERR_FG
    } else if pct >= 60 {
        TOOL_CALL_FG
    } else {
        DIM_FG
    };
    // The window size is a round configured number — "256k" reads cleaner
    // than "256.0k", so drop a trailing ".0" on the denominator only (the
    // live numerator keeps its decimal, e.g. "54.7k").
    let max_s = fmt_tokens(max_context_tokens)
        .replace(".0k", "k")
        .replace(".0M", "M");
    Some((
        format!("ctx {}% ({}/{})", pct, fmt_tokens(ctx_tokens), max_s),
        Style::default().fg(fg),
    ))
}

fn compact(v: &serde_json::Value) -> String {
    crate::agent::preview(&v.to_string())
}

/// The standard ANSI palette (SGR 30-37 / 40-47) and the bright variant
/// (90-97 / 100-107), in SGR order.
const ANSI_8: [Color; 8] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
];
const ANSI_BRIGHT: [Color; 8] = [
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

/// Apply one SGR parameter list ("1;31") to `style`. Supports reset, bold,
/// dim, italic, underline, default fg/bg (39/49) and the two 8-color ranges;
/// 256-color and RGB forms (38;5;n / 38;2;r;g;b and the 48* bg twins) have
/// their extra parameters consumed and reset that channel — carrying a full
/// palette here isn't worth it. Unknown parameters are ignored. An empty
/// parameter list is reset (ESC[m).
fn apply_sgr(style: &mut Style, params: &str) {
    let mut it = params.split(';');
    while let Some(p) = it.next() {
        let n: u16 = p.parse().unwrap_or(0); // "" (ESC[m) behaves as 0
        match n {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            30..=37 => style.fg = Some(ANSI_8[(n - 30) as usize]),
            40..=47 => style.bg = Some(ANSI_8[(n - 40) as usize]),
            90..=97 => style.fg = Some(ANSI_BRIGHT[(n - 90) as usize]),
            100..=107 => style.bg = Some(ANSI_BRIGHT[(n - 100) as usize]),
            39 => style.fg = None,
            49 => style.bg = None,
            38 | 48 => {
                match it.next().and_then(|m| m.parse::<u16>().ok()) {
                    Some(5) => {
                        it.next(); // palette index
                    }
                    Some(2) => {
                        it.next(); // r
                        it.next(); // g
                        it.next(); // b
                    }
                    _ => {}
                }
                if n == 38 {
                    style.fg = None;
                } else {
                    style.bg = None;
                }
            }
            _ => {}
        }
    }
}

/// Parse tool-output text with ANSI escape sequences into styled lines.
/// SGR color/style sequences become span styles; every other escape sequence
/// (non-`m` CSI, OSC hyperlinks/titles, two-char ESC sequences) and every
/// remaining control char except \n and \t is stripped — so this REPLACES
/// strip_controls on the tool-output path, with color support added. A
/// sequence truncated at end-of-input (e.g. a collapsed preview cut mid-CSI)
/// is dropped without panicking.
fn ansi_to_lines(text: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut style = Style::default();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: parameter/intermediate bytes until a final byte
                    // (@..=~). Only SGR ('m') is interpreted; the rest is
                    // stripped. End-of-input before a final byte → dropped.
                    let mut params = String::new();
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            if c2 == 'm' {
                                // Style boundary: flush the text the PREVIOUS
                                // style covered before applying the new one,
                                // or a mid-line SGR change (red diff line
                                // reset to default) would restyle its head.
                                if !cur.is_empty() {
                                    spans.push(Span::styled(std::mem::take(&mut cur), style));
                                }
                                apply_sgr(&mut style, &params);
                            }
                            break;
                        }
                        params.push(c2);
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: runs until BEL or ESC\. Stripped wholesale.
                    let mut prev_esc = false;
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' || (prev_esc && c2 == '\\') {
                            break;
                        }
                        prev_esc = c2 == '\x1b';
                    }
                }
                // Two-character ESC sequences (ESC c, ESC ( B, …): strip the
                // ESC and the introducer, letting any printable tail through
                // the normal path.
                _ => {
                    chars.next();
                }
            },
            '\n' => {
                spans.push(Span::styled(std::mem::take(&mut cur), style));
                lines.push(Line::from(std::mem::take(&mut spans)));
            }
            // Expand tabs to 2 spaces: unicode-width counts \t as ZERO
            // columns while terminals render it 1–8, so a tabbed line wraps
            // late and hanging indents drift. ansi_plain shares this path, so
            // /export shows the same spacing the user saw on screen.
            '\t' => cur.push_str("  "),
            c if c.is_control() => {} // strip (same contract as strip_controls)
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || !spans.is_empty() {
        spans.push(Span::styled(cur, style));
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Tool-output body lines with the dim `│ ` guide: ANSI-styled spans keep
/// their colors; spans the source left unstyled (or reset to default) take
/// `base_style` (dim for ok results, red for errors).
fn ansi_guided_lines(text: &str, base_style: Style) -> Vec<Line<'static>> {
    ansi_to_lines(text)
        .into_iter()
        .map(|l| {
            let mut spans = vec![Span::styled("│ ".to_string(), Style::default().fg(DIM_FG))];
            spans.extend(l.spans.into_iter().map(|s| {
                if s.style == Style::default() {
                    Span::styled(s.content.into_owned(), base_style)
                } else {
                    s
                }
            }));
            Line::from(spans)
        })
        .collect()
}

/// Plain-text view of ANSI-bearing tool output, for /export: styling and
/// escape sequences dropped, the visible text kept.
fn ansi_plain(text: &str) -> String {
    ansi_to_lines(text)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when a markdown line is a horizontal rule: at least 3 chars, all of
/// them `-` / `*` / `_` (whitespace ignored). tui-markdown emits a thematic
/// break as a literal "---" line, so this catches it post-parse.
fn is_hrule(text: &str) -> bool {
    let n = text.chars().filter(|c| !c.is_whitespace()).count();
    n >= 3
        && text
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, '-' | '*' | '_'))
}

/// Render a markdown string into styled lines. `tui-markdown` 0.3 styles inline
/// elements (bold / code) correctly but leaves block elements literal: an ATX
/// header renders as plain "## Title" (markers visible, no emphasis), a list
/// item as "- text", a fenced code block keeps its ``` fence lines with the
/// code unstyled, and a thematic break comes through as a literal "---" line.
/// We keep its inline work and fix only the block lines — headers become a
/// bold, level-colored title with the `#` markers stripped, `-`/`*`/`+`
/// bullets become `•`, the ``` fence lines are dropped and the enclosed code
/// gets a dim `▎ ` left border bar with the body left at the default color
/// (tui-markdown emits no language highlighting), and an hr becomes a
/// full-width dim `─` rule.
///
/// `width` is the conversation block's inner width (hr length). NOTE: the
/// rendered lines are memoized in MdCache keyed by row text, so a terminal
/// RESIZE keeps hr rules at the old width until the row re-renders — a minor
/// cosmetic staleness, accepted over keying the cache on width too.
fn render_markdown(md: &str, width: usize) -> Vec<Line<'_>> {
    let raw = tui_markdown::from_str(md).lines;
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(raw.len());
    let mut in_fence = false;
    for line in raw {
        let first: &str = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
        if first.trim_start().starts_with("```") {
            // Fence marker line (``` or ```lang): drop it and toggle.
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            // Code content: a dim `▎ ` border bar is the only signal — the
            // body keeps the default fg so code no longer fights the yellow
            // tool-call heads for attention (tui-markdown emits no inline
            // styling inside a fence anyway).
            let mut line = line;
            line.spans.insert(
                0,
                Span::styled("▎ ".to_string(), Style::default().fg(DIM_FG)),
            );
            lines.push(line);
            continue;
        }
        let line_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if is_hrule(&line_text) {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.max(1)),
                Style::default().fg(DIM_FG),
            )));
            continue;
        }
        let mut line = line;
        let first: String = line
            .spans
            .first()
            .map(|s| s.content.as_ref())
            .unwrap_or("")
            .to_string();
        if let Some(hashes) = header_level(&first) {
            // Collapse to one bold span; inline styling inside a header title
            // is rare, so dropping it is fine.
            let title: String = line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()[hashes..]
                .trim_start()
                .to_string();
            let color = match hashes {
                1 => MD_H1_FG,
                2 | 3 => MD_H23_FG,
                _ => MD_DEEP_FG,
            };
            line.spans = vec![Span::styled(
                title,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )];
        } else if first.starts_with("- ") || first.starts_with("* ") || first.starts_with("+ ") {
            // tui-markdown isolates the marker in its own span, so swap just it
            // and keep the rest (inline code/bold on the item text).
            if let Some(s0) = line.spans.first_mut() {
                s0.content = "• ".into();
            }
        }
        lines.push(line);
    }
    lines
}

/// Snapshot a `Line` with every span's borrowed content materialized into an
/// owned `String` → `Line<'static>`. Style and alignment are preserved (both
/// `Copy`). M6's render cache stores owned lines so it outlives the row text it
/// was rendered from (which is borrowed during the draw closure).
fn line_to_owned(line: &Line) -> Line<'static> {
    Line {
        spans: line
            .spans
            .iter()
            .map(|s| Span {
                content: std::borrow::Cow::Owned(s.content.to_string()),
                style: s.style,
            })
            .collect(),
        style: line.style,
        alignment: line.alignment,
    }
}

/// Per-row cache of rendered-markdown lines (M6). The draw closure re-renders
/// every assistant message on EVERY frame — including each streamed TextDelta
/// and each scroll tick — which re-parses the whole conversation's markdown via
/// `tui_markdown::from_str` each time. That's O(history) per event and gets
/// slow for long chats. This memoizes the parse keyed by (text, width) — content
/// addressing, NOT row index. Keying by row index was unsafe across frames:
/// `build_history_lines` is called with different slices (the commit window vs a
/// single in-flight row via `from_ref`), so the same relative index pointed at
/// different rows across calls, and the stale check only compared (text length,
/// width) — two different messages of equal byte length would alias and the
/// wrong cached render would be committed into the terminal scrollback.
struct MdCache {
    entries: std::collections::HashMap<(String, usize), Vec<Line<'static>>>,
}

impl MdCache {
    fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
        }
    }
    fn clear(&mut self) {
        self.entries.clear();
    }
    /// Cached rendered lines for `text` at `width`, recomputing via `render` on
    /// a miss. Keyed by (text, width) so equal-length different messages never
    /// alias. The returned slice's borrow ends at the next `&mut self` call.
    fn get_or_render(
        &mut self,
        text: &str,
        width: usize,
        render: impl FnOnce(&str) -> Vec<Line<'static>>,
    ) -> &[Line<'static>] {
        // Width participates because Markdown rules and code blocks bake the
        // available measure into their rendered lines. A terminal resize must
        // not reuse the old geometry.
        self.entries
            .entry((text.to_string(), width))
            .or_insert_with(|| render(text))
    }
}

/// ATX header level if `s` starts with 1..=6 '#' immediately followed by a
/// space (CommonMark). None otherwise.
fn header_level(s: &str) -> Option<usize> {
    let n = s.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&n) && s[n..].starts_with(' ') {
        Some(n)
    } else {
        None
    }
}

/// Multi-line input editor: a vec of lines with a (row, col) cursor. `col` is
/// a byte offset kept on UTF-8 char boundaries, so insert/remove never split a
/// multi-byte character. Owned entirely by the UI thread.
struct InputBuffer {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl InputBuffer {
    fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    fn put_char(&mut self, c: char) {
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }

    /// Delete the char before the cursor; at the start of a line, join it onto
    /// the previous one.
    fn backspace(&mut self) {
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

    /// Split the current line at the cursor; cursor lands at the start of the
    /// new line.
    fn newline(&mut self) {
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
    fn insert_str(&mut self, s: &str) {
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\n' => self.newline(),
                '\r' if chars.peek() == Some(&'\n') => {}
                _ => self.put_char(c),
            }
        }
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            let prev_ch = self.lines[self.row][..self.col].chars().last().unwrap();
            self.col -= prev_ch.len_utf8();
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    fn move_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            let ch = self.lines[self.row][self.col..].chars().next().unwrap();
            self.col += ch.len_utf8();
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = floor_char_boundary(&self.lines[self.row], self.col);
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = floor_char_boundary(&self.lines[self.row], self.col);
        }
    }

    fn home(&mut self) {
        self.col = 0;
    }

    fn end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// Move one word left: at the start of a line, to the end of the previous
    /// line; otherwise to the start of the word left of the cursor.
    fn word_left(&mut self) {
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
    fn word_right(&mut self) {
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
    fn delete_word_back(&mut self) {
        if self.col == 0 {
            self.backspace();
        } else {
            let start = word_boundary_left(&self.lines[self.row], self.col);
            self.lines[self.row].replace_range(start..self.col, "");
            self.col = start;
        }
    }

    /// Ctrl-U: delete from the line start to the cursor.
    fn kill_to_line_start(&mut self) {
        self.lines[self.row].replace_range(..self.col, "");
        self.col = 0;
    }

    /// Ctrl-K: delete from the cursor to the line end.
    fn kill_to_line_end(&mut self) {
        self.lines[self.row].truncate(self.col);
    }

    /// Replace the whole buffer and put the cursor at the end (history recall).
    fn set_text(&mut self, s: &str) {
        self.lines = s.split('\n').map(String::from).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }

    /// Take the full text, leaving an empty buffer.
    fn drain_to_string(&mut self) -> String {
        let text = self.lines.join("\n");
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        text
    }

    fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Snapshot for rendering.
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Read-only view of the logical lines, for rendering each as a separate
    /// `Line` (a single `Line` carrying embedded `\n` collapses to one visual
    /// row — ratatui's grapheme iterator filters control chars including `\n`,
    /// so a multiline buffer must be split into multiple `Line`s to render).
    fn lines_view(&self) -> &[String] {
        &self.lines
    }

    /// (row, DISPLAY width of the text before the caret on that line). Display
    /// width — not a char count — so the caret column is correct for wide (CJK)
    /// chars, which occupy two columns each (a char count drifts one column per
    /// wide char and lands the caret mid-text).
    fn cursor_pos(&self) -> (usize, usize) {
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
fn word_boundary_left(line: &str, col: usize) -> usize {
    let trimmed = line[..col].trim_end();
    trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// Byte offset where the next word right of `col` starts: leave the current
/// word, then skip the whitespace after it.
fn word_boundary_right(line: &str, col: usize) -> usize {
    let rest = &line[col..];
    let word_len = rest
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let after = &rest[word_len..];
    col + word_len + (after.len() - after.trim_start().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_fenced_code_block_styled_and_fences_dropped() {
        // Probe conclusion (tui-markdown 0.3): the ``` fence lines survive as
        // literal lines and the code between them is unstyled. render_markdown
        // must drop the fence lines and give the enclosed code a dim `▎ `
        // border bar; the body keeps the DEFAULT fg so code doesn't fight
        // the yellow tool-call heads for attention.
        let lines = render_markdown(
            "before\n\n```rust\nlet x = 1;\nlet y = 2;\n```\n\nafter `code`\n",
            40,
        );
        let texts: Vec<String> = lines.iter().map(row_text).collect();
        assert!(
            !texts.iter().any(|t| t.contains("```")),
            "fence lines leaked: {texts:?}"
        );
        let code: Vec<&Line> = lines
            .iter()
            .filter(|l| row_text(l).starts_with("▎ let "))
            .collect();
        assert_eq!(code.len(), 2, "code lines: {texts:?}");
        for l in code {
            // Span 0 is the dim border bar; the code spans stay default.
            assert_eq!(l.spans[0].content.as_ref(), "▎ ");
            assert_eq!(l.spans[0].style.fg, Some(DIM_FG));
            assert!(
                l.spans[1..].iter().all(|s| s.style.fg.is_none()),
                "code body must keep the default fg: {:?}",
                l.spans
            );
        }
        // Inline code outside a fence keeps tui-markdown's own styling
        // (fg White), untouched by the fence pass.
        let after = lines
            .iter()
            .find(|l| row_text(l).starts_with("after"))
            .expect("the line after the block");
        assert!(after.spans.iter().any(|s| s.style.fg == Some(Color::White)));
    }

    #[test]
    fn markdown_hrule_becomes_a_full_width_dim_rule() {
        // tui-markdown emits a thematic break as a literal "---" line;
        // render_markdown swaps it for a dim ─ rule of the given width.
        let lines = render_markdown("above\n\n---\n\nbelow\n", 12);
        let rule = lines
            .iter()
            .find(|l| row_text(l).contains('─'))
            .expect("an hr line");
        assert_eq!(row_text(rule), "─".repeat(12));
        assert_eq!(rule.spans[0].style.fg, Some(DIM_FG));
        // is_hrule itself: ≥3 chars of -,*,_ only; "--" and "- item" are not.
        assert!(is_hrule("---") && is_hrule("***") && is_hrule("___"));
        assert!(is_hrule(" - - - "));
        assert!(!is_hrule("--") && !is_hrule("- item") && !is_hrule("abc---"));
    }

    // ── build_history_lines: history row styling ───────────────────────

    /// Fixed inner width for history-building tests.
    const TEST_W: usize = 40;

    /// Shorthand for a collapsed tool call row.
    fn tool_call_row(name: &str) -> Row {
        Row::ToolCall {
            name: name.into(),
            input: serde_json::Value::Null,
            collapsed: "arg summary".into(),
            expanded: false,
            started: Instant::now(),
        }
    }

    /// Shorthand for a collapsed tool result row (elapsed None, as when no
    /// matching call row exists).
    fn tool_result_row(name: &str, is_error: bool) -> Row {
        Row::ToolResult {
            name: name.into(),
            content: "body line".into(),
            collapsed: "body line".into(),
            is_error,
            expanded: false,
            elapsed: None,
        }
    }

    /// Build history lines with the test width, not streaming.
    fn history(rows: &[Row]) -> Vec<Line<'static>> {
        let mut cache = MdCache::new();
        build_history_lines(rows, &mut cache, TEST_W, false, false)
    }

    #[test]
    fn history_empty_shows_dim_italic_placeholder() {
        let lines = history(&[]);
        assert_eq!(
            lines.iter().map(row_text).collect::<Vec<_>>(),
            vec![
                "  no messages yet — type below; /help lists commands".to_string(),
                "  / completes commands · Enter sends".to_string(),
                "  scroll / select / copy are native to your terminal".to_string(),
            ]
        );
        for l in &lines {
            let st = l.spans[0].style;
            assert_eq!(st.fg, Some(DIM_FG));
            assert!(st.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn history_user_and_agent_use_inline_icons() {
        let rows = vec![Row::User("hello".into()), Row::Assistant("world".into())];
        let lines = history(&rows);

        // User: cyan-bold `❯ ` icon inline on the first body line — no
        // standalone header row. Body text keeps the default color.
        assert_eq!(row_text(&lines[0]), "❯ hello");
        let icon = lines[0].spans[0].style;
        assert_eq!(icon.fg, Some(USER_FG));
        assert!(icon.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            lines[0].spans[1].style.fg, None,
            "user body must stay default fg"
        );

        // Assistant: green-bold `● ` icon the same way.
        assert_eq!(row_text(&lines[1]), "● world");
        let icon = lines[1].spans[0].style;
        assert_eq!(icon.fg, Some(AGENT_FG));
        assert!(icon.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn history_multiline_role_body_aligns_under_the_icon_text() {
        let rows = vec![Row::User("first\nsecond".into())];
        let lines = history(&rows);
        // Continuation lines indent to the icon's text column (2 cells).
        assert_eq!(row_text(&lines[0]), "❯ first");
        assert_eq!(row_text(&lines[1]), "  second");
    }

    #[test]
    fn history_tool_rows_have_colored_heads_and_guided_bodies() {
        let rows = vec![
            tool_call_row("shell"),
            tool_result_row("shell", false),
            tool_result_row("shell", true),
        ];
        let lines = history(&rows);

        // Collapsed tools are one-line lifecycle records under the AI gutter.
        assert_eq!(row_text(&lines[0]), "    → shell  arg summary");
        let st = lines[0].spans[0].style;
        assert_eq!(st.fg, Some(TOOL_CALL_FG));
        assert!(st.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[0].spans[1].style.fg, Some(DIM_FG));

        // Success stays compact and uses a dim preview. The ok head is NOT
        // bold: a green bold "✓" would read as a second "● " agent icon.
        assert_eq!(row_text(&lines[1]), "    ✓ shell  body line");
        let ok = lines[1].spans[0].style;
        assert_eq!(ok.fg, Some(TOOL_OK_FG));
        assert!(!ok.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[1].spans[1].style.fg, Some(DIM_FG));

        // Error preview is red as well as carrying an explicit failure glyph;
        // the failure head alone earns the bold.
        assert_eq!(row_text(&lines[2]), "    ✗ shell  body line");
        let err = lines[2].spans[0].style;
        assert_eq!(err.fg, Some(TOOL_ERR_FG));
        assert!(err.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[2].spans[1].style.fg, Some(TOOL_ERR_FG));
    }

    #[test]
    fn history_tool_result_head_shows_elapsed_when_present() {
        let mut row = tool_result_row("shell", false);
        if let Row::ToolResult { elapsed, .. } = &mut row {
            *elapsed = Some("1.2s".into());
        }
        let lines = history(&[row]);
        assert_eq!(row_text(&lines[0]), "    ✓ shell · 1.2s  body line");
    }

    #[test]
    fn history_collapsed_tool_preview_strips_ansi_without_losing_text() {
        let row = Row::ToolResult {
            name: "shell".into(),
            content: "\x1b[31m- old\x1b[0m".into(),
            collapsed: "\x1b[31m- old\x1b[0m".into(),
            is_error: false,
            expanded: false,
            elapsed: None,
        };
        let lines = history(&[row]);
        assert_eq!(row_text(&lines[0]), "    ✓ shell  - old");
        assert!(!row_text(&lines[0]).contains('\x1b'));
    }

    #[test]
    fn history_tool_head_summary_wraps_with_a_hanging_indent() {
        // A wide collapsed summary on a narrow terminal wraps the head; the
        // continuation rows align under the summary column instead of
        // falling back to column 0 and breaking the tool gutter.
        let mut row = tool_call_row("shell");
        if let Row::ToolCall { collapsed, .. } = &mut row {
            *collapsed = "alpha beta gamma delta epsilon zeta eta theta iota".into();
        }
        let lines = history(&[row]);
        let body: Vec<&Line> = lines.iter().filter(|l| !row_text(l).is_empty()).collect();
        assert!(body.len() > 1, "head must wrap at width {TEST_W}");
        let hang = "    → shell  ".width();
        assert!(row_text(body[0]).starts_with("    → shell  alpha"));
        for cont in &body[1..] {
            assert!(
                row_text(cont).starts_with(&" ".repeat(hang)),
                "continuation not hung at {hang}: {:?}",
                row_text(cont)
            );
            // The indent leaves room for text, not a full-width row.
            assert!(row_width(cont) <= TEST_W);
        }
        // All summary words survive the wrap.
        let all: String = body.iter().map(|l| row_text(l)).collect();
        assert!(all.contains("theta") && all.contains("iota"));
    }

    #[test]
    fn history_system_levels_are_color_graded() {
        let rows = vec![
            Row::sys_info("[done: end_turn]"),
            Row::sys_warn("[cancelling…]"),
            Row::sys_error("[error] boom"),
        ];
        let lines = history(&rows);
        // Single-line layout: one content line ("· text") + blank per row.
        // Info nests in the tool gutter; warn/error rise to column 0 so they
        // stand out from the execution detail they interrupt.
        let cases = [
            (0, DIM_FG, "    · "),
            (2, TOOL_CALL_FG, "· "),
            (4, TOOL_ERR_FG, "· "),
        ];
        for (base, fg, pad) in cases {
            // The "· " marker and the first text line share one rendered line.
            let (marker, body) = (&lines[base].spans[0], &lines[base].spans[1]);
            assert_eq!(marker.content.as_ref(), pad, "marker at {base}");
            assert_eq!(marker.style.fg, Some(fg), "marker at {base}");
            assert!(marker.style.add_modifier.contains(Modifier::BOLD));
            assert_eq!(body.style.fg, Some(fg), "body at {base}");
            assert!(!body.style.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn history_system_multiline_indents_continuation_lines() {
        // Multi-line notices (e.g. the /help reply): first line carries the
        // "· " marker, continuation lines align under it — in the gutter for
        // info, at column 0 for warn/error.
        let rows = vec![Row::sys_info("first\nsecond\nthird")];
        let lines = history(&rows);
        assert_eq!(row_text(&lines[0]), "    · first");
        assert_eq!(row_text(&lines[1]), "      second");
        assert_eq!(row_text(&lines[2]), "      third");
        assert!(!lines[1].spans[0]
            .style
            .add_modifier
            .contains(Modifier::ITALIC));
        let lines = history(&[Row::sys_warn("w1\nw2")]);
        assert_eq!(row_text(&lines[0]), "· w1");
        assert_eq!(row_text(&lines[1]), "  w2");
    }

    #[test]
    fn history_turns_use_whitespace_instead_of_full_width_rules() {
        let rows = vec![
            Row::User("one".into()),
            Row::Assistant("a".into()),
            Row::User("two".into()),
        ];
        let lines = history(&rows);
        assert_eq!(row_text(&lines[0]), "❯ one");
        assert_eq!(row_text(&lines[1]), "● a");
        assert_eq!(row_text(&lines[2]), "");
        assert_eq!(row_text(&lines[3]), "❯ two");
        assert!(lines
            .iter()
            .all(|line| !row_text(line).contains(&"─".repeat(TEST_W))));
    }

    #[test]
    fn history_first_user_after_the_welcome_line_gets_no_separator() {
        // rows[0] is the startup welcome (System), so the first User sits at
        // idx 1. The old `idx > 0` rule drew a separator above it that floated
        // mid-screen with nothing to separate — separators only between turns.
        let rows = vec![
            Row::sys_info(welcome_text("glm", 3)),
            Row::User("hi".into()),
        ];
        let lines = history(&rows);
        assert!(
            lines.iter().all(|l| row_text(l) != "─".repeat(TEST_W)),
            "no separator above the first User row"
        );
        // Welcome notice and user row are separated by whitespace, not a rule.
        assert_eq!(
            row_text(&lines[0]),
            format!("    · {}", welcome_text("glm", 3))
        );
        assert_eq!(row_text(&lines[2]), "❯ hi");
    }

    #[test]
    fn history_queued_row_renders_as_dim_ghost() {
        let rows = vec![Row::Queued("later".into())];
        let lines = history(&rows);
        // Inline dim icon + dim body, tagged `· queued` on the first line.
        assert_eq!(row_text(&lines[0]), "❯ later · queued");
        let icon = lines[0].spans[0].style;
        assert_eq!(icon.fg, Some(DIM_FG));
        assert!(icon.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[0].spans[1].style.fg, Some(DIM_FG));
    }

    #[test]
    fn history_streaming_cursor_marks_the_live_assistant_tail() {
        let rows = vec![
            Row::Assistant("done".into()),
            Row::Assistant("typing".into()),
        ];
        let mut cache = MdCache::new();
        let lines = build_history_lines(&rows, &mut cache, TEST_W, true, false);
        assert_eq!(row_text(&lines[0]), "● done");
        assert_eq!(row_text(&lines[1]), "● typing▌");
        let last = lines[1].spans.last().expect("a cursor span");
        assert_eq!(last.content.as_ref(), "▌");
        assert_eq!(last.style.fg, Some(AGENT_FG));
        // Not streaming: no cursor anywhere.
        let lines = history(&rows);
        assert!(!row_text(&lines[1]).contains('▌'));
    }

    #[test]
    fn history_thinking_appends_a_transient_placeholder_tail() {
        let rows = vec![Row::User("hi".into())];
        let mut cache = MdCache::new();
        let lines = build_history_lines(&rows, &mut cache, TEST_W, false, true);
        let last = lines.last().expect("a thinking tail line");
        assert_eq!(row_text(last), "● …");
        let icon = last.spans[0].style;
        assert_eq!(icon.fg, Some(AGENT_FG));
        assert!(icon.add_modifier.contains(Modifier::BOLD));
        assert_eq!(last.spans[1].style.fg, Some(DIM_FG));
        // Not thinking: no tail, the trailing spacer stays last.
        let lines = history(&rows);
        assert_eq!(row_text(lines.last().unwrap()), "");
        // Thinking on an EMPTY conversation (a run launched with a task,
        // before its first row): the placeholder replaces the onboarding
        // cheat-sheet instead of stacking under it.
        let mut cache = MdCache::new();
        let lines = build_history_lines(&[], &mut cache, TEST_W, false, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(row_text(&lines[0]), "● …");
    }

    #[test]
    fn queued_ghost_removal_helpers_are_fifo_and_report_shifts() {
        let mut rows = vec![
            Row::Assistant("a".into()),
            Row::Queued("q1".into()),
            Row::Assistant("b".into()),
            Row::Queued("q2".into()),
        ];
        // Oldest ghost first (matches queued.pop_front FIFO).
        assert!(remove_oldest_queued(&mut rows));
        assert!(rows.iter().filter(|r| matches!(r, Row::Queued(_))).count() == 1);
        assert!(matches!(&rows[2], Row::Queued(t) if t == "q2"));
        // Drop-all removes the rest and reports the shift; a second call is a
        // no-op reporting none.
        assert!(drop_queued_ghosts(&mut rows));
        assert!(!rows.iter().any(|r| matches!(r, Row::Queued(_))));
        assert_eq!(rows.len(), 2);
        assert!(!drop_queued_ghosts(&mut rows));
        assert!(!remove_oldest_queued(&mut rows));
    }

    #[test]
    fn fmt_elapsed_switches_to_minutes_at_60s() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(59), "59s");
        assert_eq!(fmt_elapsed(60), "1m00s");
        assert_eq!(fmt_elapsed(245), "4m05s");
        assert_eq!(fmt_elapsed(3600), "60m00s");
    }

    #[test]
    fn is_normal_stop_covers_both_provider_families() {
        // Anthropic: end_turn / tool_use / stop_sequence; OpenAI: stop /
        // tool_calls — none of these get a "[done: …]" row.
        for normal in [
            "end_turn",
            "tool_use",
            "stop",
            "tool_calls",
            "stop_sequence",
        ] {
            assert!(is_normal_stop(normal), "{normal} should be silent");
        }
        // Abnormal stops DO get a warn row.
        for abnormal in ["max_tokens", "length", "error", "unknown", ""] {
            assert!(!is_normal_stop(abnormal), "{abnormal} should be shown");
        }
    }

    #[test]
    fn welcome_text_names_model_tool_count_and_help() {
        assert_eq!(
            welcome_text("glm-4.6", 7),
            "tiny-agent · model glm-4.6 · 7 tools · /help lists commands"
        );
    }

    #[test]
    fn input_title_merges_activity_elapsed_cancel_and_queue() {
        assert_eq!(
            build_input_title(true, '⠋', "running shell", 65, 2, 200),
            " ⠋ running shell… · 1m05s · Ctrl-C cancel · 2 queued "
        );
        assert_eq!(
            build_input_title(false, '⠋', "", 0, 0, 200),
            " Ask TinyAgent "
        );
    }

    #[test]
    fn input_title_drops_activity_before_elapsed_cancel_and_queue() {
        // ratatui clips an over-long title from the right — which would eat
        // exactly the elapsed/cancel/queue tail. The title degrades instead:
        // the activity name goes first.
        let full = build_input_title(true, '⠋', "running shell", 65, 2, 200);
        // Exactly at the boundary the full title still fits; one column less
        // degrades to spinner + elapsed + cancel + queue.
        assert_eq!(
            build_input_title(true, '⠋', "running shell", 65, 2, full.width()),
            full
        );
        assert_eq!(
            build_input_title(true, '⠋', "running shell", 65, 2, full.width() - 1),
            " ⠋ 1m05s · Ctrl-C cancel · 2 queued "
        );
        // No queue: the degraded tail drops that segment too.
        assert_eq!(
            build_input_title(true, '⠋', "thinking", 8, 0, 10),
            " ⠋ 8s · Ctrl-C cancel "
        );
    }

    #[test]
    fn auto_expand_result_opens_short_failures_only() {
        // Successes never auto-open, however short.
        assert!(!auto_expand_result(false, "one line"));
        // A short failure opens; the boundary is 20 lines inclusive.
        let short = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(auto_expand_result(true, &short));
        let long = (1..=21)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!auto_expand_result(true, &long));
    }

    #[test]
    fn fmt_tool_elapsed_one_decimal_below_10s() {
        assert_eq!(fmt_tool_elapsed(Duration::from_millis(1200)), "1.2s");
        assert_eq!(fmt_tool_elapsed(Duration::from_millis(9_940)), "9.9s");
        assert_eq!(fmt_tool_elapsed(Duration::from_secs(10)), "10s");
        assert_eq!(fmt_tool_elapsed(Duration::from_secs(65)), "65s");
    }

    #[test]
    fn tool_running_tracks_calls_fifo_and_clears() {
        // Render-only tracker: on_call pushes, on_result removes the first
        // same-name entry (falling back to the oldest), clear/empty no-op.
        let mut t = ToolRunning::new();
        assert!(t.is_empty());
        t.on_call("shell");
        t.on_call("read");
        t.on_call("shell"); // parallel same-name call
        assert_eq!(t.calls.len(), 3);
        t.on_result("shell"); // first same-name (oldest shell) drops
        assert_eq!(t.calls.len(), 2);
        assert_eq!(t.calls[0].0, "read");
        t.on_result("missing"); // no same-name → oldest drops
        assert_eq!(t.calls[0].0, "shell");
        t.on_result("shell");
        t.on_result("shell"); // already empty → no-op, not a panic
        assert!(t.is_empty());
        t.on_call("x");
        t.clear();
        assert!(t.is_empty());
    }

    #[test]
    fn tool_running_line_shows_name_elapsed_and_parallel_count() {
        let now = Instant::now();
        let mut t = ToolRunning::new();
        assert!(tool_running_line(&t, now).is_none(), "nothing in flight");
        t.calls = vec![("shell".into(), now - Duration::from_millis(3200))];
        let l = tool_running_line(&t, now).expect("a placeholder line");
        assert_eq!(l.spans[0].content.as_ref(), "→ shell");
        assert_eq!(l.spans[0].style.fg, Some(TOOL_CALL_FG));
        assert!(l.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(l.spans[1].content.as_ref(), " · 3.2s…");
        assert_eq!(l.spans[1].style.fg, Some(DIM_FG));
        // Parallel calls aggregate: the OLDEST call's elapsed + (+N) count.
        t.calls.push(("read".into(), now));
        let l = tool_running_line(&t, now).expect("a placeholder line");
        assert_eq!(l.spans[0].content.as_ref(), "→ shell");
        assert_eq!(l.spans[1].content.as_ref(), " · 3.2s… (+1)");
    }

    #[test]
    fn tool_elapsed_for_matches_nearest_same_name_call_backwards() {
        let now = Instant::now();
        let call_at = |name: &str, ago: Duration| Row::ToolCall {
            name: name.into(),
            input: serde_json::Value::Null,
            collapsed: String::new(),
            expanded: false,
            started: now - ago,
        };
        let rows = vec![
            call_at("shell", Duration::from_secs(5)),
            call_at("read", Duration::from_secs(3)),
            call_at("shell", Duration::from_millis(1500)),
        ];
        // Nearest-back "shell" is the 1.5s one, not the 5s one.
        assert_eq!(tool_elapsed_for(&rows, "shell"), Some("1.5s".into()));
        assert_eq!(tool_elapsed_for(&rows, "read"), Some("3.0s".into()));
        // No matching call row (resumed session) → None, header hides it.
        assert_eq!(tool_elapsed_for(&rows, "missing"), None);
    }

    #[test]
    fn build_status_line_reduces_detail_on_narrow_terminals() {
        let stats = " · 5 in · 6 out";
        // Wide: full token detail appears as a dim third span.
        let wide = build_status_line("glm", "idle", false, None, None, stats, "", 200);
        assert_eq!(wide.spans.len(), 3);
        assert_eq!(wide.spans[2].content.as_ref(), stats);
        assert_eq!(wide.spans[2].style.fg, Some(DIM_FG));
        // Medium: stats no longer fit, so the bar drops to badge+state only.
        let medium_width = " glm ".width() + "  idle".width();
        let medium = build_status_line("glm", "idle", false, None, None, stats, "", medium_width);
        assert_eq!(medium.spans.len(), 2);
        // Narrow: model and live state remain, everything else disappears.
        let narrow = build_status_line("glm", "idle", false, None, None, stats, "", 12);
        assert_eq!(narrow.spans.len(), 2);
        // Busy state pops AGENT_FG bold — the one busy color, matching the
        // input chrome; idle is dim and carries no clock.
        let busy = build_status_line("glm", "running", true, None, None, stats, "", 200);
        let st = busy.spans[1].style;
        assert_eq!(st.fg, Some(AGENT_FG));
        assert!(st.add_modifier.contains(Modifier::BOLD));
        let idle = build_status_line("glm", "idle", false, None, None, stats, "", 200);
        assert_eq!(idle.spans[1].style.fg, Some(DIM_FG));
        // Model badge keeps its black-on-cyan look.
        assert_eq!(wide.spans[0].style.bg, Some(Color::Cyan));
    }

    #[test]
    fn build_status_line_ctx_outlives_the_stats() {
        let stats = " · 5 in · 6 out";
        let ctx = Some(("ctx 21% (54.7k/256k)", Style::default().fg(DIM_FG)));
        // Wide: ctx sits between the state and the stats, keeping its style.
        let wide = build_status_line("glm", "idle", false, ctx, None, stats, "", 200);
        assert_eq!(wide.spans.len(), 4);
        assert_eq!(wide.spans[2].content.as_ref(), " · ctx 21% (54.7k/256k)");
        assert_eq!(wide.spans[2].style.fg, Some(DIM_FG));
        assert_eq!(wide.spans[3].content.as_ref(), stats);
        // Narrower: stats drop out but ctx stays — it's the most decision-relevant
        // number, so it outlives the stats and is the last segment standing.
        let ctx_only_width = " glm ".width() + "  idle".width() + " · ctx 21% (54.7k/256k)".width();
        let minimal = build_status_line("glm", "idle", false, ctx, None, stats, "", ctx_only_width);
        assert_eq!(minimal.spans.len(), 3);
        assert_eq!(minimal.spans[2].content.as_ref(), " · ctx 21% (54.7k/256k)");
    }

    #[test]
    fn build_status_line_plan_segment_drops_after_stats_before_ctx() {
        // The plan segment sits between ctx and stats; narrowing drops stats
        // first, then plan, then ctx (ctx survives longest). plan=None ⇒ the
        // segment is absent and behavior matches pre-plan exactly.
        let stats = " · 5 in · 6 out";
        let ctx = Some(("ctx 21%", Style::default().fg(DIM_FG)));
        let plan = Some((" · plan 2/5", Style::default().fg(TOOL_CALL_FG)));
        // Wide: all four optional segments present, plan between ctx and stats.
        let wide = build_status_line("glm", "idle", false, ctx, plan, stats, "", 200);
        assert_eq!(wide.spans.len(), 5);
        assert_eq!(wide.spans[2].content.as_ref(), " · ctx 21%");
        assert_eq!(wide.spans[3].content.as_ref(), " · plan 2/5");
        assert_eq!(wide.spans[3].style.fg, Some(TOOL_CALL_FG));
        assert_eq!(wide.spans[4].content.as_ref(), stats);
        // Tight: stats drop, ctx+plan remain (plan outlives stats).
        let no_stats_w =
            " glm ".width() + "  idle".width() + " · ctx 21%".width() + " · plan 2/5".width();
        let mid = build_status_line("glm", "idle", false, ctx, plan, stats, "", no_stats_w);
        assert_eq!(mid.spans.len(), 4);
        assert_eq!(mid.spans[3].content.as_ref(), " · plan 2/5");
        // Tighter: plan drops too, ctx alone survives (ctx outlives plan).
        let ctx_only_w = " glm ".width() + "  idle".width() + " · ctx 21%".width();
        let tight = build_status_line("glm", "idle", false, ctx, plan, stats, "", ctx_only_w);
        assert_eq!(tight.spans.len(), 3);
        assert_eq!(tight.spans[2].content.as_ref(), " · ctx 21%");
    }

    #[test]
    fn right_path_seg_right_aligns_truncates_and_vanishes() {
        // Fits: right-aligned with padding, dim.
        let seg = right_path_seg("/tmp/proj", 20).expect("fits");
        assert_eq!(seg.content.as_ref(), "           /tmp/proj");
        assert_eq!(seg.style.fg, Some(DIM_FG));
        // Under 12 columns left: no segment at all.
        assert!(right_path_seg("/tmp/proj", 11).is_none());
        assert_eq!(
            right_path_seg("/tmp/proj", 12)
                .map(|s| s.content.into_owned())
                .as_deref(),
            Some("   /tmp/proj")
        );
        // Too long for the budget: LEFT-truncated with a leading "…", the
        // project directory tail kept, still right-aligned.
        let seg =
            right_path_seg("/Users/chenpu/workspace/claude-code/TinyAgent", 20).expect("truncated");
        assert_eq!(seg.content.as_ref(), "  …de-code/TinyAgent");
        assert_eq!(seg.content.width(), 20);
        // Even "…" + the basename exceeds the budget: nothing.
        assert!(right_path_seg("/a/verylongbasename", 12).is_none());
        // Empty path (cwd query failed): nothing.
        assert!(right_path_seg("", 40).is_none());
    }

    #[test]
    fn build_status_line_path_takes_what_the_left_side_leaves() {
        let stats = " · 5 in · 6 out";
        // Left side uses 5 + 6 + 13 = 24 columns of the 40: the path
        // right-aligns into the remaining 16.
        let line = build_status_line("glm", "idle", false, None, None, stats, "/tmp/proj", 40);
        let last = line.spans.last().expect("a path span");
        assert!(last.content.ends_with("/tmp/proj"));
        assert_eq!(line.width(), 40, "left + padded path fills the bar");
        // Narrow terminal: stats still fit but leave no room for the path.
        let line = build_status_line("glm", "idle", false, None, None, stats, "/tmp/proj", 26);
        assert_eq!(line.spans.len(), 3, "no path span: {:?}", line.spans);
    }

    #[test]
    fn fmt_ctx_formats_grades_and_hides_until_first_usage() {
        // No Usage event yet → no segment at all.
        assert!(fmt_ctx(0, 256_000).is_none());
        // Unknown window size → absolute value only, dim.
        let (t, s) = fmt_ctx(54_700, 0).unwrap();
        assert_eq!(t, "ctx 54.7k");
        assert_eq!(s.fg, Some(DIM_FG));
        // Known window → percentage + both numbers; round denominator drops
        // its ".0". Fill-level color grading: dim < 60%, yellow ≥ 60%, red ≥ 80%.
        let (t, s) = fmt_ctx(54_700, 256_000).unwrap();
        assert_eq!(t, "ctx 21% (54.7k/256k)");
        assert_eq!(s.fg, Some(DIM_FG));
        let (_, s) = fmt_ctx(160_000, 256_000).unwrap(); // 62%
        assert_eq!(s.fg, Some(TOOL_CALL_FG));
        let (t, s) = fmt_ctx(210_000, 256_000).unwrap(); // 82%
        assert_eq!(t, "ctx 82% (210.0k/256k)");
        assert_eq!(s.fg, Some(TOOL_ERR_FG));
    }

    #[test]
    fn chrome_fg_is_one_busy_color_for_chrome_and_status() {
        // Busy: the whole bottom chrome (input border + status state) reads as
        // one AGENT_FG unit. Idle: the USER_FG idle color.
        assert_eq!(chrome_fg(true), AGENT_FG);
        assert_eq!(chrome_fg(false), USER_FG);
    }

    #[test]
    fn slash_help_reply_is_a_multiline_list() {
        // Minimal config — the Help arm doesn't read it, but slash_reply's
        // signature needs one.
        let cfg: Config =
            serde_json::from_str(r#"{"provider":"anthropic","api_key_env":"K","model":"m"}"#)
                .unwrap();
        let help = slash_reply(&SlashCmd::Help, &cfg);
        assert!(
            help.lines().count() > 3,
            "help should be multiline: {help:?}"
        );
        for cmd in [
            "/clear", "/export", "/model", "/limits", "/tools", "/system", "/help",
        ] {
            assert!(
                help.lines().any(|l| l.contains(cmd)),
                "missing {cmd}: {help:?}"
            );
        }
        // Indented one-command-per-line layout.
        assert!(help.lines().any(|l| l.starts_with("  /export")));
    }

    // ── Slash-command completion popup ─────────────────────────────────

    #[test]
    fn slash_popup_opens_filters_and_closes_on_space_or_no_match() {
        let mut p = SlashPopup::new();
        p.update("");
        assert!(!p.is_open(), "empty input");
        p.update("abc");
        assert!(!p.is_open(), "no leading slash");
        // Bare "/" lists every command.
        p.update("/");
        assert!(p.is_open());
        assert_eq!(p.matches.len(), SLASH_COMMANDS.len());
        // Prefix filtering is case-sensitive, like classify.
        p.update("/ex");
        assert_eq!(p.matches.len(), 1);
        assert_eq!(p.matches[0].name, "export");
        p.update("/EX");
        assert!(!p.is_open(), "uppercase must match nothing");
        p.update("/zzz");
        assert!(!p.is_open(), "no match");
        // A space means the argument stage: closed even with a valid name.
        p.update("/export ");
        assert!(!p.is_open(), "argument stage closes the popup");
        // Backspacing out of the argument stage reopens it.
        p.update("/expor");
        assert!(p.is_open());
        assert_eq!(p.matches[0].name, "export");
    }

    #[test]
    fn slash_popup_completion_text_and_selection_cycle() {
        let mut p = SlashPopup::new();
        p.update("/");
        // First match is clear (table order); a no-arg command completes bare.
        assert_eq!(p.completion().as_deref(), Some("/clear"));
        // export is the one argument-taking command: trailing space included.
        p.next();
        assert_eq!(p.completion().as_deref(), Some("/export "));
        // Down wraps past the last match back to the first; Up wraps back.
        for _ in 0..SLASH_COMMANDS.len() - 1 {
            p.next();
        }
        assert_eq!(p.sel, 0, "wrap to top");
        p.prev();
        assert_eq!(p.sel, SLASH_COMMANDS.len() - 1, "wrap to bottom");
        assert_eq!(p.completion().as_deref(), Some("/quit"));
        // Editing the filter resets the selection to the top.
        p.update("/s");
        assert_eq!(p.sel, 0);
        assert_eq!(p.matches[0].name, "system");
    }

    #[test]
    fn slash_popup_dismiss_holds_until_the_text_changes() {
        let mut p = SlashPopup::new();
        p.update("/");
        assert!(p.is_open());
        p.dismiss("/");
        assert!(!p.is_open());
        // Same text next frame: stays closed (Esc would never close otherwise).
        p.update("/");
        assert!(!p.is_open(), "dismissed text must not reopen");
        // Any further edit lifts the dismissal.
        p.update("/c");
        assert!(p.is_open());
        // The completion path: buffer becomes "/clear", popup stays closed…
        p.update("/cle");
        let c = p.completion().expect("a match");
        p.dismiss(&c);
        p.update(&c);
        assert!(!p.is_open(), "just-completed text stays closed");
        // …and deleting a char reopens with the shorter prefix.
        p.update("/clea");
        assert!(p.is_open());
    }

    #[test]
    fn popup_key_intercepts_only_plain_keys_while_open() {
        let mut p = SlashPopup::new();
        p.update("/");
        // Up/Down move the selection and consume the key.
        assert!(matches!(
            popup_key(&mut p, KeyCode::Down, KeyModifiers::NONE),
            Some(PopupAction::Consumed)
        ));
        assert_eq!(p.sel, 1);
        assert!(matches!(
            popup_key(&mut p, KeyCode::Up, KeyModifiers::NONE),
            Some(PopupAction::Consumed)
        ));
        assert_eq!(p.sel, 0);
        // Tab and Enter both complete (never send); Esc only dismisses.
        assert!(matches!(
            popup_key(&mut p, KeyCode::Tab, KeyModifiers::NONE),
            Some(PopupAction::Complete(ref c)) if c == "/clear"
        ));
        assert!(matches!(
            popup_key(&mut p, KeyCode::Enter, KeyModifiers::NONE),
            Some(PopupAction::Complete(ref c)) if c == "/clear"
        ));
        assert!(matches!(
            popup_key(&mut p, KeyCode::Esc, KeyModifiers::NONE),
            Some(PopupAction::Dismiss)
        ));
        // Typing and modifier chords are NOT intercepted (Alt+Up = history).
        assert!(popup_key(&mut p, KeyCode::Char('x'), KeyModifiers::NONE).is_none());
        assert!(popup_key(&mut p, KeyCode::Up, KeyModifiers::ALT).is_none());
        assert!(popup_key(&mut p, KeyCode::Tab, KeyModifiers::SHIFT).is_none());
    }

    #[test]
    fn visible_window_keeps_the_selection_inside() {
        // Under the cap everything shows.
        assert_eq!(visible_window(5, 4, POPUP_MAX_ROWS), (0, 5));
        // Over the cap the window slides to keep the selection visible.
        assert_eq!(visible_window(12, 0, POPUP_MAX_ROWS), (0, 8));
        assert_eq!(visible_window(12, 7, POPUP_MAX_ROWS), (0, 8));
        assert_eq!(visible_window(12, 8, POPUP_MAX_ROWS), (1, 8));
        assert_eq!(visible_window(12, 11, POPUP_MAX_ROWS), (4, 8));
    }

    #[test]
    fn slash_popup_lines_format_highlight_and_pad_the_selection() {
        let mut p = SlashPopup::new();
        p.update("/");
        let lines = slash_popup_lines(&p, POPUP_MAX_ROWS);
        assert_eq!(lines.len(), SLASH_COMMANDS.len());
        // Row format: "/name" + optional " [arg]" + "  description".
        assert_eq!(
            row_text(&lines[1]),
            "/export [path]  dump the transcript to a file (or clipboard)"
        );
        assert_eq!(
            row_text(&lines[0]).trim_end(),
            "/clear  clear the conversation history"
        );
        // The selected row (sel 0) is reversed and padded to the content
        // width; the others are neither.
        let content_w = lines.iter().map(|l| l.width()).max().unwrap();
        assert!(lines[0].style.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(lines[0].width(), content_w);
        assert!(lines[0]
            .spans
            .last()
            .is_some_and(|s| s.style.add_modifier.contains(Modifier::REVERSED)));
        assert!(!lines[1].style.add_modifier.contains(Modifier::REVERSED));
        // Moving the selection moves the highlight.
        p.next();
        let lines = slash_popup_lines(&p, POPUP_MAX_ROWS);
        assert!(!lines[0].style.add_modifier.contains(Modifier::REVERSED));
        assert!(lines[1].style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn append_delta_never_drops_text_when_tail_is_not_assistant() {
        // P1: a ghost / notice / tool row pushed mid-stream used to swallow
        // every following delta. Each tail shape must open a FRESH Assistant
        // row and leave the original row untouched.
        for tail in [
            Row::Queued("q".into()),
            Row::sys_info("notice"),
            tool_call_row("shell"),
        ] {
            let mut rows = vec![Row::Assistant("done".into()), tail];
            let mut streaming = true; // stream was open when the row landed
            append_delta(&mut rows, &mut streaming, "more");
            assert_eq!(rows.len(), 3);
            assert!(
                matches!(&rows[2], Row::Assistant(t) if t == "more"),
                "delta lost or merged into the interleaved tail row"
            );
            // The interleaved row kept its content.
            assert!(matches!(
                &rows[1],
                Row::Queued(_) | Row::System { .. } | Row::ToolCall { .. }
            ));
        }
        // Normal case: tail IS the open Assistant bubble → merge.
        let mut rows = vec![Row::Assistant("a".into())];
        let mut streaming = true;
        append_delta(&mut rows, &mut streaming, "b");
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], Row::Assistant(t) if t == "ab"));
        // Fresh stream start (streaming=false) opens a row even when the tail
        // is an old Assistant bubble from a previous turn.
        let mut streaming = false;
        append_delta(&mut rows, &mut streaming, "c");
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[1], Row::Assistant(t) if t == "c"));
    }

    #[test]
    fn append_sub_delta_merges_by_name_and_recovers_after_interruption() {
        let mut rows = vec![];
        let mut sub: Option<String> = None;
        append_sub_delta(&mut rows, &mut sub, "a", "hello");
        append_sub_delta(&mut rows, &mut sub, "a", " world");
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], Row::Assistant(t) if t == "[a] hello world"));
        // A different sub-agent starts its own bubble.
        append_sub_delta(&mut rows, &mut sub, "b", "other");
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[1], Row::Assistant(t) if t == "[b] other"));
        // Same sub-agent again but the tail is now a System notice → fresh
        // bubble with the prefix re-stated (the old code dropped this delta).
        rows.push(Row::sys_warn("[warn] x"));
        append_sub_delta(&mut rows, &mut sub, "b", "resumed");
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[3], Row::Assistant(t) if t == "[b] resumed"));
    }

    #[test]
    fn strip_controls_removes_esc_csi_and_nul_keeps_newline_tab() {
        // A full ANSI color sequence: ESC and the CSI bracket codes' ESC go,
        // leaving only printable residue.
        assert_eq!(strip_controls("\x1b[31mred\x1b[0m"), "[31mred[0m");
        assert_eq!(strip_controls("a\x00b\x07c"), "abc");
        assert_eq!(strip_controls("line1\nline2\there"), "line1\nline2\there");
        // Plain text is untouched (and the common case allocates once).
        assert_eq!(strip_controls("plain"), "plain");
    }

    #[test]
    fn strip_controls_json_sanitizes_nested_strings() {
        let v = serde_json::json!({
            "cmd": "echo \x1b[1mhi",
            "args": ["a\x00b", {"nested": "x\x07y"}],
            "count": 3,
        });
        let clean = strip_controls_json(&v);
        assert_eq!(clean["cmd"], "echo [1mhi");
        assert_eq!(clean["args"][0], "ab");
        assert_eq!(clean["args"][1]["nested"], "xy");
        assert_eq!(clean["count"], 3); // non-strings pass through
    }

    // ── ANSI parsing (ansi_to_lines / ansi_guided_lines / ansi_plain) ───

    #[test]
    fn ansi_to_lines_colors_a_git_diff_and_splits_at_style_changes() {
        // The git-diff case: red removals, green additions, default context.
        let lines = ansi_to_lines("\x1b[31m- old\x1b[0m\n\x1b[32m+ new\x1b[0m\n plain");
        assert_eq!(lines.len(), 3);
        assert_eq!(row_text(&lines[0]), "- old");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(row_text(&lines[1]), "+ new");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Green));
        // After the reset the context line is back to the default style.
        assert_eq!(row_text(&lines[2]), " plain");
        assert_eq!(lines[2].spans[0].style, Style::default());
        // A mid-line SGR change splits spans: red head, default tail — not one
        // span carrying whichever style happened to be last.
        let lines = ansi_to_lines("\x1b[31mred\x1b[0mplain");
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].content.as_ref(), "red");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[0].spans[1].content.as_ref(), "plain");
        assert_eq!(lines[0].spans[1].style, Style::default());
    }

    #[test]
    fn ansi_to_lines_handles_modifiers_bg_and_channel_resets() {
        // Bold + standard fg.
        let lines = ansi_to_lines("\x1b[1;33mbold yellow\x1b[0m");
        let st = lines[0].spans[0].style;
        assert_eq!(st.fg, Some(Color::Yellow));
        assert!(st.add_modifier.contains(Modifier::BOLD));
        // Bright fg (91) + standard bg (44).
        let lines = ansi_to_lines("\x1b[91;44mhi\x1b[0m");
        let st = lines[0].spans[0].style;
        assert_eq!(st.fg, Some(Color::LightRed));
        assert_eq!(st.bg, Some(Color::Blue));
        // 39 resets only fg; the 256-color form (38;5;n) is consumed and
        // resets its channel instead of guessing a palette color.
        let lines = ansi_to_lines("\x1b[31;42mx\x1b[39my\x1b[38;5;196mz");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[0].spans[1].style.fg, None);
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Green));
        assert_eq!(lines[0].spans[2].style.fg, None);
    }

    #[test]
    fn ansi_to_lines_strips_osc_and_truncated_sequences() {
        // OSC hyperlink (BEL- and ESC\-terminated forms) stripped, text kept.
        let lines = ansi_to_lines("\x1b]8;;http://x\x07link\x1b]8;;\x1b\\ done");
        assert_eq!(row_text(&lines[0]), "link done");
        // A CSI cut off at end-of-input is dropped without panicking.
        let lines = ansi_to_lines("\x1b[31mabc\x1b[");
        assert_eq!(row_text(&lines[0]), "abc");
        // Non-m CSI (cursor moves) and stray control chars are stripped.
        let lines = ansi_to_lines("a\x1b[2Jb\x00c\x07d");
        assert_eq!(row_text(&lines[0]), "abcd");
        // Empty input still yields one (empty) line.
        assert_eq!(ansi_to_lines("").len(), 1);
    }

    #[test]
    fn ansi_guided_lines_guides_and_falls_back_to_base_style() {
        let err_style = Style::default().fg(TOOL_ERR_FG);
        let lines = ansi_guided_lines("\x1b[31m- old\x1b[0m\nplain", err_style);
        // Every line gets the dim "│ " guide.
        for l in &lines {
            assert_eq!(l.spans[0].content.as_ref(), "│ ");
            assert_eq!(l.spans[0].style.fg, Some(DIM_FG));
        }
        // ANSI-colored spans keep their own color…
        let red = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "- old")
            .expect("the red span");
        assert_eq!(red.style.fg, Some(Color::Red));
        // …unstyled spans fall back to the base style (error red here).
        assert_eq!(lines[1].spans[1].content.as_ref(), "plain");
        assert_eq!(lines[1].spans[1].style, err_style);
    }

    #[test]
    fn ansi_plain_strips_codes_keeps_text() {
        assert_eq!(ansi_plain("\x1b[31m- old\x1b[0m\nplain"), "- old\nplain");
        assert_eq!(ansi_plain("no codes"), "no codes");
    }

    #[test]
    fn ansi_tabs_expand_to_two_spaces_on_both_paths() {
        // unicode-width counts \t as zero columns, which would wrap tabbed
        // tool output wrong; both the render path (ansi_to_lines) and the
        // export/preview path (ansi_plain) expand it the same way.
        let lines = ansi_to_lines("a\tb");
        assert_eq!(row_text(&lines[0]), "a  b");
        assert_eq!(ansi_plain("a\tb"), "a  b");
    }

    #[test]
    fn flush_next_queued_honors_the_fifo_ghost_append_contract() {
        let (tx, rx) = mpsc::channel::<Option<Input>>();
        let mut queued = std::collections::VecDeque::from(vec!["q1".to_string(), "q2".to_string()]);
        let mut rows = vec![
            Row::Assistant("run output".into()),
            Row::Queued("q1".into()),
            Row::Queued("q2".into()),
        ];
        let mut cache = MdCache::new();

        // First flush: oldest message out, ITS ghost removed, the real
        // Row::User appended at the END (send order).
        let out = flush_next_queued(&mut queued, &mut rows, &mut cache, &tx);
        assert_eq!(out, Flush::Sent);
        assert!(matches!(rx.try_recv(), Ok(Some(Input::Text(t))) if t == "q1"));
        assert!(matches!(&rows[1], Row::Queued(t) if t == "q2")); // q2's ghost stays put
        assert!(matches!(rows.last(), Some(Row::User(t)) if t == "q1"));

        // Second flush: FIFO — q2 next.
        let out = flush_next_queued(&mut queued, &mut rows, &mut cache, &tx);
        assert_eq!(out, Flush::Sent);
        assert!(matches!(rx.try_recv(), Ok(Some(Input::Text(t))) if t == "q2"));
        assert!(!rows.iter().any(|r| matches!(r, Row::Queued(_))));

        // Empty queue: no mutation at all.
        let before = rows.len();
        let out = flush_next_queued(&mut queued, &mut rows, &mut cache, &tx);
        assert_eq!(out, Flush::Empty);
        assert_eq!(rows.len(), before);

        // Dead worker: popped message is reported undeliverable — its
        // Row::User stays (it records what was attempted) plus a sys_error.
        queued.push_back("q3".to_string());
        rows.push(Row::Queued("q3".into()));
        drop(rx);
        let out = flush_next_queued(&mut queued, &mut rows, &mut cache, &tx);
        assert_eq!(out, Flush::Undeliverable);
        assert!(rows.iter().any(|r| matches!(r, Row::User(t) if t == "q3")));
        assert!(rows.iter().any(|r| matches!(
            r,
            Row::System {
                level: SysLevel::Error,
                ..
            }
        )));
    }

    #[test]
    fn commit_pending_end_advances_to_end_with_no_queue_or_stream() {
        let rows = vec![Row::User("hi".into()), Row::Assistant("hey".into())];
        assert_eq!(commit_pending_end(&rows, 0, false), 2);
    }

    #[test]
    fn commit_pending_end_holds_back_a_trailing_streaming_assistant() {
        let rows = vec![Row::User("hi".into()), Row::Assistant("streaming".into())];
        // Stream open: the in-flight Assistant (last row) stays in the live
        // viewport, not committed to scrollback until the stream closes.
        assert_eq!(commit_pending_end(&rows, 0, true), 1);
    }

    #[test]
    fn commit_pending_end_holds_back_a_trailing_queued_ghost() {
        let rows = vec![Row::User("hi".into()), Row::Queued("next".into())];
        assert_eq!(commit_pending_end(&rows, 0, false), 1);
    }

    #[test]
    fn commit_pending_end_holds_back_a_ghost_even_when_a_real_row_follows_it() {
        // The regression from the audit: while a message is queued, the
        // in-flight run appends a REAL row (here a system warning) AFTER the
        // ghost. The old trailing-only strip left the ghost non-trailing and
        // committed it; the fix caps at the FIRST ghost, so the ghost AND the
        // later real row stay out of scrollback, while the Assistant before the
        // ghost still commits.
        let rows = vec![
            Row::Assistant("run output".into()),
            Row::Queued("queued msg".into()),
            Row::sys_warn("[warn] something"),
        ];
        assert_eq!(
            commit_pending_end(&rows, 0, false),
            1,
            "ghost + post-ghost row must stay out of scrollback"
        );
    }

    #[test]
    fn commit_pending_end_respects_an_existing_committed_watermark() {
        // Mid-conversation: rows[0..1] already committed. Only the uncommitted
        // prefix up to the ghost (the Assistant) may commit; the ghost and the
        // following row are held back.
        let rows = vec![
            Row::User("u1".into()),      // 0 — committed
            Row::Assistant("a1".into()), // 1 — commits
            Row::Queued("q".into()),     // 2 — cap here
            Row::sys_warn("late warn"),  // 3 — held back
        ];
        assert_eq!(commit_pending_end(&rows, 1, false), 2);
    }

    #[test]
    fn inflight_row_index_is_the_last_assistant_while_streaming() {
        let rows = vec![Row::User("hi".into()), Row::Assistant("streaming".into())];
        assert_eq!(inflight_row_index(&rows, true), Some(1));
        assert_eq!(inflight_row_index(&rows, false), None);
    }

    #[test]
    fn inflight_row_index_follows_a_new_stream_past_a_queued_ghost() {
        // The regression: a queued ghost sits in the middle, and a NEW turn's
        // Assistant streams after it. The live zone must render that new
        // streaming Assistant (last one), NOT the stale pre-ghost Assistant
        // that the commit watermark (capped at the first ghost) would point at.
        let rows = vec![
            Row::User("u".into()),           // 0
            Row::Assistant("turn 1".into()), // 1 — stale, pre-ghost
            Row::Queued("queued".into()),    // 2 — ghost
            Row::sys_warn("warn"),           // 3
            Row::Assistant("turn 2".into()), // 4 — the live streaming row
        ];
        assert_eq!(
            inflight_row_index(&rows, true),
            Some(4),
            "must render the last Assistant (turn 2), not the pre-ghost one"
        );
    }

    #[test]
    fn inflight_row_index_handles_a_ghost_pushed_after_the_streaming_row() {
        // User queues WHILE an Assistant streams: the ghost lands after it.
        // The streaming row is still the last Assistant.
        let rows = vec![
            Row::User("u".into()),              // 0
            Row::Assistant("streaming".into()), // 1
            Row::Queued("next".into()),         // 2 — pushed during the stream
        ];
        assert_eq!(inflight_row_index(&rows, true), Some(1));
    }

    #[test]
    fn history_expanded_tool_rows_render_full_bodies() {
        // 8a: expanded ToolCall shows the PRETTY JSON (not the collapsed
        // summary), expanded ToolResult the full output — both with guides.
        let call = Row::ToolCall {
            name: "shell".into(),
            input: serde_json::json!({"cmd": "ls", "sudo": true}),
            collapsed: "summary".into(),
            expanded: true,
            started: Instant::now(),
        };
        let result = Row::ToolResult {
            name: "shell".into(),
            content: "line1\nline2".into(),
            collapsed: "line1".into(),
            is_error: false,
            expanded: true,
            elapsed: None,
        };
        let lines = history(&[call, result]);
        let texts: Vec<String> = lines.iter().map(row_text).collect();
        assert!(
            texts.iter().any(|t| t.contains("\"cmd\": \"ls\"")),
            "pretty JSON: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("summary")),
            "collapsed leaked: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "    │ line1") && texts.iter().any(|t| t == "    │ line2")
        );
        // Every body line carries the role gutter followed by the guide span.
        let guided = lines
            .iter()
            .filter(|l| l.spans.get(1).is_some_and(|s| s.content.as_ref() == "│ "));
        assert!(
            guided.count() >= 4,
            "expanded bodies must be guided: {texts:?}"
        );
    }

    #[test]
    fn markdown_unclosed_fence_colors_tail_without_leaking() {
        // 8b: a fence left open at EOF (streamed partial block) must not
        // panic, leak the ``` line, or leave the tail without its border bar.
        let lines = render_markdown("intro\n\n```rust\nlet a = 1;\nlet b = 2;", 40);
        let texts: Vec<String> = lines.iter().map(row_text).collect();
        assert!(
            !texts.iter().any(|t| t.contains("```")),
            "fence leaked: {texts:?}"
        );
        for l in lines.iter().filter(|l| row_text(l).starts_with("▎ let ")) {
            assert_eq!(l.spans[0].style.fg, Some(DIM_FG));
            assert!(
                l.spans[1..].iter().all(|s| s.style.fg.is_none()),
                "unclosed fence tail must stay default fg: {:?}",
                l.spans
            );
        }
    }

    #[test]
    fn build_status_line_width_boundary_keeps_then_drops_stats() {
        let state = "idle · 3s";
        let stats = " · 5 in · 6 out";
        let used = " glm ".width() + format!("  {state}").width() + stats.width();
        let exact = used;
        // At the exact fit, the full stats are the third span.
        let full = build_status_line("glm", state, false, None, None, stats, "", exact);
        assert_eq!(full.spans[2].content.as_ref(), stats);
        // One column short: stats no longer fit and there's no ctx to fall back
        // to, so the bar is just badge+state.
        let dropped = build_status_line("glm", state, false, None, None, stats, "", exact - 1);
        assert_eq!(dropped.spans.len(), 2);
    }

    #[test]
    fn idle_prompt_glyph_is_two_columns() {
        // The caret math adds prompt.width() to the caret column on the first
        // input line; the stable "❯ " prompt must stay two columns.
        assert_eq!(UnicodeWidthStr::width("❯ "), 2);
    }

    #[test]
    fn markdown_headers_and_lists_restyled() {
        // tui-markdown 0.3 leaves ATX headers literal ("## T", unstyled) and
        // bullets literal ("- x"). render_markdown must fix them while keeping
        // inline styling (code → fg White) on list items.
        let lines = render_markdown("## 标题\n\n- 项目 `code` more\n", 40);
        // Header: a single bold span, markers stripped, just the title.
        let hdr = lines
            .iter()
            .find(|l| {
                l.spans
                    .first()
                    .is_some_and(|s| s.style.add_modifier.contains(Modifier::BOLD))
            })
            .expect("a bold header line");
        assert_eq!(hdr.spans.len(), 1);
        let ht = hdr.spans[0].content.as_ref();
        assert!(!ht.contains('#'), "header still has '#': {ht}");
        assert_eq!(ht, "标题");

        // List: first span is "• ", and a later span still carries inline code
        // styling from `code` (kept, not dropped by the marker swap).
        let list = lines
            .iter()
            .find(|l| l.spans.first().is_some_and(|s| s.content.as_ref() == "• "))
            .expect("a bulleted list line");
        assert_eq!(list.spans[0].content.as_ref(), "• ");
        assert!(
            list.spans.iter().any(|s| s.style.fg == Some(Color::White)),
            "inline code styling lost: {:?}",
            list.spans
        );
    }

    #[test]
    fn caret_column_uses_display_width() {
        // 3 CJK chars = 6 display columns; the caret prefix width must be 6,
        // not the char count 3, or the caret lands mid-text on wide input.
        let mut b = InputBuffer::new();
        for c in "看来离".chars() {
            b.put_char(c);
        }
        assert_eq!(b.cursor_pos(), (0, 6));
    }

    #[test]
    fn type_and_drain() {
        let mut b = InputBuffer::new();
        for c in "hi".chars() {
            b.put_char(c);
        }
        assert_eq!(b.text(), "hi");
        assert_eq!(b.drain_to_string(), "hi");
        assert!(b.is_empty());
    }

    #[test]
    fn insert_str_handles_multiline_crlf_and_cjk() {
        // Paste a multi-line blob at the start: each \n starts a new line.
        let mut b = InputBuffer::new();
        b.insert_str("line1\nline2\nline3");
        assert_eq!(b.text(), "line1\nline2\nline3");
        assert_eq!(b.line_count(), 3);

        // CRLF pastes collapse to a single newline (no stray CR left behind).
        let mut b = InputBuffer::new();
        b.insert_str("a\r\nb");
        assert_eq!(b.text(), "a\nb");

        // A LONE \r is kept: only the CR of a CRLF pair is dropped — content
        // that uses \r deliberately must survive the paste.
        let mut b = InputBuffer::new();
        b.insert_str("a\rb");
        assert_eq!(b.text(), "a\rb");

        // Insert mid-line lands at the cursor, splitting the existing text.
        let mut b = InputBuffer::new();
        b.set_text("abcd"); // cursor at end (pos 4)
        b.move_left(); // "abc|d"
        b.move_left(); // "ab|cd"
        b.insert_str("XY");
        assert_eq!(b.text(), "abXYcd");

        // A multi-line CJK paste round-trips byte-for-byte.
        let mut b = InputBuffer::new();
        b.insert_str("你好\n世界");
        assert_eq!(b.text(), "你好\n世界");
        assert_eq!(b.line_count(), 2);
    }

    #[test]
    fn newline_splits_line() {
        let mut b = InputBuffer::new();
        b.set_text("abcd");
        b.move_left();
        b.move_left(); // cursor at col 2
        b.newline();
        assert_eq!(b.text(), "ab\ncd");
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.cursor_pos(), (1, 0));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut b = InputBuffer::new();
        b.set_text("ab\ncd"); // cursor at end of "cd"
        b.home(); // start of line 1
        b.backspace(); // join → "abcd", cursor after "ab"
        assert_eq!(b.text(), "abcd");
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.cursor_pos(), (0, 2));
    }

    #[test]
    fn backspace_preserves_unicode() {
        let mut b = InputBuffer::new();
        b.set_text("中文");
        b.move_left(); // before 文
        b.backspace(); // delete 中
        assert_eq!(b.text(), "文");
        assert_eq!(b.col, 0);
    }

    #[test]
    fn move_left_right_over_unicode() {
        let mut b = InputBuffer::new();
        b.set_text("a中b"); // bytes: a(1) 中(3) b(1) = 5
        b.move_left(); // before b → col 4
        b.move_left(); // before 中 → col 1
        assert_eq!(b.col, 1);
        b.move_left(); // before a → col 0
        assert_eq!(b.col, 0);
        b.move_right(); // before 中 → col 1
        assert_eq!(b.col, 1);
    }

    #[test]
    fn move_up_down_clamps_col() {
        let mut b = InputBuffer::new();
        b.set_text("long line one\n短"); // cursor at end of 短 (col 3)
        b.move_up(); // col clamps to min(3, line0.len())
        assert_eq!(b.row, 0);
        assert!(b.col <= "long line one".len());
    }

    #[test]
    fn move_up_down_floors_to_char_boundary() {
        // col is a byte offset valid on the current line. Moving to a line whose
        // multibyte layout differs can place it mid-codepoint — cursor() then
        // slices lines[row][..col] and panics. The mover must floor to a boundary.
        let mut b = InputBuffer::new();
        b.set_text("中\na"); // ["中"(3B), "a"(1B)]; cursor at end of "a": row1 col1
        b.move_up(); // -> row0; col 1 would be inside '中' (bytes 0..3)
        assert_eq!(b.row, 0);
        assert!(
            b.col == 0 || b.col == 3,
            "col {} not a boundary of '中'",
            b.col
        );
        let _ = b.cursor_pos(); // slices lines[row][..col]; must not panic

        // Reverse direction: cursor after 'a' (col1) on a 1-byte line, moving
        // down onto '中' lands col1 inside it.
        let mut b = InputBuffer::new();
        b.set_text("a\n中"); // ["a", "中"]; cursor ends at row1 col3 (end of 中)
        b.home(); // row1 col0
        b.move_up(); // row0 col0
        b.end(); // row0 col1 (after 'a')
        b.move_down(); // -> row1; col 1 inside '中'
        assert_eq!(b.row, 1);
        assert!(
            b.col == 0 || b.col == 3,
            "col {} not a boundary of '中'",
            b.col
        );
        let _ = b.cursor_pos(); // must not panic
    }

    #[test]
    fn home_end() {
        let mut b = InputBuffer::new();
        b.set_text("abc");
        b.home();
        assert_eq!(b.col, 0);
        b.end();
        assert_eq!(b.col, 3);
    }

    #[test]
    fn drain_resets_to_empty() {
        let mut b = InputBuffer::new();
        b.set_text("x\ny\nz");
        let s = b.drain_to_string();
        assert_eq!(s, "x\ny\nz");
        assert!(b.is_empty());
        assert_eq!(b.line_count(), 1);
    }

    #[test]
    fn classify_messages_vs_commands() {
        assert_eq!(classify("hello"), SlashCmd::Message);
        assert_eq!(classify("/clear"), SlashCmd::Clear);
        assert_eq!(classify("/model "), SlashCmd::Model); // trailing space trimmed
        assert_eq!(classify("/limits"), SlashCmd::Limits);
        assert_eq!(classify("/tools"), SlashCmd::Tools);
        assert_eq!(classify("/system"), SlashCmd::System);
        assert_eq!(classify("/help"), SlashCmd::Help);
        assert_eq!(classify("/quit"), SlashCmd::Quit);
        assert_eq!(classify("/exit"), SlashCmd::Quit); // alias
                                                       // Case-sensitive: /Model is not /model.
        assert_eq!(classify("/Model"), SlashCmd::Unknown("Model".into()));
        assert_eq!(classify("/foo"), SlashCmd::Unknown("foo".into()));
        // Bare "/" → unknown with empty name (not a message, not a command).
        assert_eq!(classify("/"), SlashCmd::Unknown(String::new()));
        // /export takes an optional path arg, split off + trimmed.
        assert_eq!(classify("/export"), SlashCmd::Export(None));
        assert_eq!(
            classify("/export out.md"),
            SlashCmd::Export(Some("out.md".into()))
        );
        assert_eq!(
            classify("/export   a/b.txt  "),
            SlashCmd::Export(Some("a/b.txt".into()))
        );
        // /export with only trailing spaces → no arg.
        assert_eq!(classify("/export "), SlashCmd::Export(None));
        // No-arg commands reject a trailing arg (only /export takes one):
        // `/model foo` is a typo, not a silently-ignored /model.
        assert_eq!(
            classify("/model foo"),
            SlashCmd::Unknown("model foo".into())
        );
        assert_eq!(classify("/clear x"), SlashCmd::Unknown("clear x".into()));
    }

    #[test]
    fn expand_tilde_resolves_home() {
        let home = std::ffi::OsStr::new("/Users/x");
        assert_eq!(
            expand_tilde_with("~/notes.md", Some(home)),
            "/Users/x/notes.md"
        );
        assert_eq!(expand_tilde_with("~", Some(home)), "/Users/x");
        // No leading tilde → passthrough unchanged.
        assert_eq!(expand_tilde_with("foo/bar", Some(home)), "foo/bar");
        assert_eq!(expand_tilde_with("/abs/path", Some(home)), "/abs/path");
        // Only the current user's home; `~other` is left as-is.
        assert_eq!(expand_tilde_with("~other/x", Some(home)), "~other/x");
        // $HOME unset → literal path (no expansion, no error).
        assert_eq!(expand_tilde_with("~/notes.md", None), "~/notes.md");
        assert_eq!(expand_tilde_with("~", None), "~");
    }

    #[test]
    fn render_transcript_dumps_rows_as_plain_text() {
        // /export's escape hatch: the conversation as readable text. Mirrors the
        // on-screen layout (User/Tiny/tool rows), skips mid-stream empty
        // assistant rows, and tags error results distinctly.
        let rows = vec![
            Row::User("hello".into()),
            Row::Queued("later".into()),
            Row::Assistant(String::new()), // empty → skipped (no bare "Tiny")
            Row::Assistant("I am fine".into()),
            Row::ToolCall {
                name: "shell".into(),
                input: serde_json::Value::Null,
                collapsed: "null".into(),
                expanded: false,
                started: Instant::now(),
            },
            Row::ToolResult {
                name: "shell".into(),
                content: "a.txt".into(),
                collapsed: "a.txt".into(),
                is_error: false,
                expanded: false,
                elapsed: Some("1.2s".into()),
            },
            Row::ToolResult {
                name: "shell".into(),
                content: "boom".into(),
                collapsed: "boom".into(),
                is_error: true,
                expanded: false,
                elapsed: None,
            },
            Row::sys_info("[note]"),
        ];
        let out = render_transcript(&rows);
        assert!(out.contains("You\nhello"), "user row: {out:?}");
        assert!(out.contains("You (queued)\nlater"), "queued ghost: {out:?}");
        assert!(out.contains("Tiny\nI am fine"), "assistant body: {out:?}");
        assert!(!out.contains("Tiny\n\n"), "empty assistant must be skipped");
        assert!(out.contains("→ shell: null"), "tool call: {out:?}");
        assert!(out.contains("✓ shell: a.txt"), "tool result: {out:?}");
        assert!(
            out.contains("✗ shell: boom"),
            "error result tagged: {out:?}"
        );
        assert!(out.contains("· [note]"), "system row: {out:?}");
    }

    #[test]
    fn fmt_tokens_compact() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(12_000), "12.0k");
        assert_eq!(fmt_tokens(999_999), "1000.0k");
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    /// Helper: a single-span line with one style.
    fn span_line(text: &str, fg: Color) -> Line<'static> {
        Line::from(vec![Span::styled(
            text.to_string(),
            Style::default().fg(fg),
        )])
    }

    /// Display width of a wrapped row (sum of its spans), via unicode-width.
    fn row_width(l: &Line) -> usize {
        l.spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    }

    /// Joined text of a wrapped row (to assert content / dropped spaces).
    fn row_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrap_line_short_fits_one_row() {
        let rows = wrap_line(&span_line("hello world", Color::Cyan), 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "hello world");
    }

    #[test]
    fn wrap_line_breaks_at_word_boundary_and_drops_space() {
        // width 5: "hello" fits exactly; " world" would overflow → break at the
        // space, drop it, start "world" fresh on row 2. No mid-word split.
        let rows = wrap_line(&span_line("hello world", Color::Cyan), 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(row_text(&rows[0]), "hello");
        assert_eq!(row_text(&rows[1]), "world");
        // The dropped space must not leak as a leading space on the wrap.
        assert!(!row_text(&rows[1]).starts_with(' '));
        // Every row stays within width.
        for r in &rows {
            assert!(row_width(r) <= 5);
        }
    }

    #[test]
    fn wrap_line_hard_breaks_an_overlong_word() {
        // A single word wider than width must still wrap (char-break), not loop
        // forever or overflow the column.
        let rows = wrap_line(&span_line("abcdefgh", Color::Cyan), 3);
        assert!(rows.len() >= 3, "got {} rows", rows.len());
        for r in &rows {
            assert!(row_width(r) <= 3);
        }
        // No characters lost across the hard breaks.
        let joined: String = rows.iter().map(|r| row_text(r)).collect();
        assert_eq!(joined, "abcdefgh");
    }

    #[test]
    fn wrap_line_preserves_span_styles_across_break() {
        // Two styled spans in one line; the wrap splits the second span but its
        // color must survive onto the wrapped row.
        let line = Line::from(vec![
            Span::styled("short ".to_string(), Style::default().fg(Color::Cyan)),
            Span::styled("longtail".to_string(), Style::default().fg(Color::Yellow)),
        ]);
        let rows = wrap_line(&line, 8);
        assert_eq!(rows.len(), 2);
        // Row 0 holds "short " (cyan); row 1 holds "longtail" (yellow) — the
        // split span kept its style.
        assert_eq!(row_text(&rows[0]), "short");
        assert_eq!(row_text(&rows[1]), "longtail");
        assert_eq!(rows[1].spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn wrap_line_counts_wide_chars_by_display_width() {
        // 6 CJK chars = 12 display columns, no spaces (so one word, hard-broken).
        // At width 4 each row holds 2 chars (4 cols) → 3 rows. Proves width is
        // display columns, not char count — the original bug counted logical
        // lines and under-scrolled CJK text.
        let rows = wrap_line(&span_line("看来离谱的测", Color::Cyan), 4);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert!(row_width(r) <= 4);
        }
        // And only at width ≥ 12 do all 6 wide chars fit one row — char-count
        // semantics would have fit them at width 6.
        assert_eq!(
            wrap_line(&span_line("看来离谱的测", Color::Cyan), 11).len(),
            2,
            "11 cols still can't hold 12 cols of wide chars"
        );
        assert_eq!(
            wrap_line(&span_line("看来离谱的测", Color::Cyan), 12).len(),
            1,
            "12 cols exactly fits 6 wide chars on one row"
        );
    }

    #[test]
    fn wrap_line_empty_yields_one_empty_row() {
        let rows = wrap_line(&Line::default(), 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_width(&rows[0]), 0);
    }

    #[test]
    fn input_hscroll_keeps_caret_inside_the_box() {
        // Short line: no scroll, caret at its natural column.
        assert_eq!(input_hscroll(3, 10), (0, 3));
        // Caret exactly at the last inner column: still no scroll.
        assert_eq!(input_hscroll(9, 10), (0, 9));
        // Caret past the right edge: scrolls just enough to show it on the
        // last inner column — never beyond the border.
        assert_eq!(input_hscroll(10, 10), (1, 9));
        assert_eq!(input_hscroll(500, 10), (491, 9));
        // Degenerate widths can't underflow or place the caret outside.
        assert_eq!(input_hscroll(0, 0), (0, 0));
        assert_eq!(input_hscroll(42, 1), (42, 0));
        // Invariant for a sweep of values: caret_x < inner_cols always.
        for caret in 0..300usize {
            for w in 1..20usize {
                let (_, x) = input_hscroll(caret, w);
                assert!((x as usize) < w, "caret {caret} at width {w} → x {x}");
            }
        }
    }

    #[test]
    fn input_box_horizontal_scroll_renders_caret_line_window() {
        // Widget level (the project's TestBackend convention): with the scroll
        // from input_hscroll applied, the visible window of an over-wide line
        // contains the text around the caret, not the line's clipped head.
        use ratatui::backend::{Backend, TestBackend};
        use ratatui::layout::Rect;

        let body = "abcdefghijklmnopqrst"; // 20 cols, box shows 6
        let (scroll, x) = input_hscroll(20, 6); // caret at end
        assert_eq!(x, 5);
        let area = Rect::new(0, 0, 8, 3);
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| {
            let p = Paragraph::new(body)
                .block(WBlock::default().borders(Borders::ALL))
                .scroll((0, scroll));
            f.render_widget(p, area);
            f.set_cursor_position((1 + x, 1));
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row: String = (1..7).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        // Caret at end-of-line (col 20): window shows the tail + the blank
        // caret cell after 't' — the clipped head "abcd…" is scrolled away.
        assert_eq!(row, "pqrst ", "scrolled window: {row:?}");
        // The drawn cursor stays within the box's inner area.
        let pos = term.backend_mut().get_cursor_position().unwrap();
        assert!(
            pos.x >= 1 && pos.x <= 6,
            "cursor x {} outside inner area",
            pos.x
        );
    }

    #[test]
    fn md_cache_recomputes_when_text_or_width_changes() {
        // M6: the cache is keyed by (text, width). A cache hit ignores the new
        // renderer (returns the old content); a miss uses it. We prove reuse by
        // swapping renderers between calls.
        let mut c = MdCache::new();
        let upper = |t: &str| vec![Line::from(t.to_string())];
        let lower = |t: &str| vec![Line::from(t.to_string())];

        // Miss → upper renderer used.
        let r = c.get_or_render("hi", 40, upper);
        assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi");

        // Hit (same text+width): the lower renderer is IGNORED — still "hi".
        // Swap to a distinguishing renderer to prove the cached value is returned.
        let distinguishing = |t: &str| vec![Line::from(t.to_string() + "!")];
        let r = c.get_or_render("hi", 40, distinguishing);
        assert_eq!(
            r[0].spans.first().unwrap().content.as_ref(),
            "hi",
            "cache hit must NOT re-render (distinguishing renderer ignored)"
        );

        // Text grew (streaming): miss → distinguishing renderer now applied.
        let r = c.get_or_render("hi there", 40, distinguishing);
        assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi there!");

        // Same text at a different terminal width is also a miss.
        let r = c.get_or_render("hi there", 24, lower);
        assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi there");

        // The first text is still cached at width 40 (independent of width 24).
        let r = c.get_or_render("hi", 40, distinguishing);
        assert_eq!(r[0].spans.first().unwrap().content.as_ref(), "hi");

        // clear() drops entries → next access is a miss.
        c.clear();
        let r = c.get_or_render("hi there", 24, lower);
        assert_eq!(
            r[0].spans.first().unwrap().content.as_ref(),
            "hi there",
            "clear must force a re-render with the new renderer"
        );
    }

    #[test]
    fn md_cache_keys_by_text_not_index() {
        // Regression: the cache was previously keyed by row index, but
        // build_history_lines is called with different slices across frames
        // (commit window vs a single in-flight row via `from_ref`), so the
        // same relative index pointed at different rows. The stale check only
        // compared (text byte-length, width) — two different messages of equal
        // byte length would alias and the wrong cached render would be
        // committed into the terminal scrollback. Keying by (text, width)
        // makes equal-length different messages independent.
        let mut c = MdCache::new();
        let renderer = |t: &str| vec![Line::from(t.to_string())];
        // Two different 4-byte messages ("AAAA" and "BBBB").
        let r1 = c.get_or_render("AAAA", 80, renderer);
        assert_eq!(r1[0].spans.first().unwrap().content.as_ref(), "AAAA");
        // Same width, same byte length, DIFFERENT content → must NOT alias.
        let r2 = c.get_or_render("BBBB", 80, renderer);
        assert_eq!(r2[0].spans.first().unwrap().content.as_ref(), "BBBB");
        // The first entry is untouched by the second render.
        let r1_again = c.get_or_render("AAAA", 80, renderer);
        assert_eq!(
            r1_again[0].spans.first().unwrap().content.as_ref(),
            "AAAA",
            "equal-length different messages must not alias in the cache"
        );
    }

    #[test]
    fn line_to_owned_preserves_spans_style_and_alignment() {
        // M6: the cache stores owned lines; the conversion must keep every
        // span's text + style and the line's alignment, materializing the
        // (possibly borrowed) content into owned Strings.
        use ratatui::layout::Alignment;
        let src = Line::from(vec![
            Span::styled(
                "hello",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" world"),
        ])
        .alignment(Alignment::Center);
        let owned = line_to_owned(&src);
        assert_eq!(owned.spans.len(), 2);
        assert_eq!(owned.spans[0].content.as_ref(), "hello");
        assert_eq!(owned.spans[0].style.fg, Some(Color::Cyan));
        assert!(owned.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(owned.spans[1].content.as_ref(), " world");
        assert_eq!(owned.alignment, Some(Alignment::Center));
        // CJK / multi-byte content round-trips through the owned snapshot too
        // (to_string materializes the whole codepoint sequence, not bytes).
        let cjk = Line::from(vec![Span::raw("你好")]);
        let owned = line_to_owned(&cjk);
        assert_eq!(owned.spans[0].content.as_ref(), "你好");
    }

    #[test]
    fn cached_owned_lines_render_identically_to_fresh_markdown() {
        // M6 end-to-end at the widget layer (the project's TestBackend
        // convention): the cache stores `render_markdown` output run through
        // `line_to_owned`. That snapshot must render into a real buffer with
        // the SAME cells as rendering the borrowed `render_markdown` lines
        // directly — otherwise the cache silently drifts what the user sees.
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        // Header + list + inline code: exercises render_markdown's restyling
        // (cyan bold header, bullet swap, inline-code span).
        let md = "## 标题\n\n- 项目 `代码` more\n";
        let borrowed: Vec<Line> = render_markdown(md, 40);
        let owned: Vec<Line<'static>> = render_markdown(md, 40).iter().map(line_to_owned).collect();

        // Wide enough that the content isn't wrapped (we're comparing caches,
        // not wrapping). Render both into equal buffers and flatten cells.
        let area = Rect::new(0, 0, 40, 5);
        let cells_of = |lines: Vec<Line>| -> String {
            let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            term.draw(|f| f.render_widget(Paragraph::new(lines), area))
                .unwrap();
            let buf = term.backend().buffer();
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("│")
        };
        assert_eq!(
            cells_of(borrowed.clone()),
            cells_of(owned.clone()),
            "owned (cached) snapshot must render identically to the borrowed source"
        );
        // Sanity: the buffers carry real content (not both-empty-and-thus-equal).
        // ASCII renders monotonically; CJK wide chars spread across cells (each
        // leaves a trailing empty cell), so check an ASCII fragment plus a CJK
        // glyph rather than the contiguous "标题" substring.
        let rendered = cells_of(borrowed);
        assert!(rendered.contains("more"), "list text missing: {rendered:?}");
        assert!(
            rendered.contains('标'),
            "CJK header glyph missing: {rendered:?}"
        );
    }

    #[test]
    fn render_zone_tail_bottom_aligns_short_content_and_truncates_overflow() {
        // The transcript zone above the input box: content that FITS is padded
        // to the bottom (latest line flush against the input box); content that
        // OVERFLOWS gets a dim `↑ +N` hint on top and the tail below it.
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        let rows_of = |lines: Vec<Line<'static>>, h: u16| -> Vec<String> {
            let area = Rect::new(0, 0, 20, h);
            let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            term.draw(|f| render_zone_tail(f, area, &lines)).unwrap();
            let buf = term.backend().buffer();
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect()
        };
        // Fits (1 line in a 3-row zone): bottom-aligned — the top two rows blank.
        let out = rows_of(vec![Line::from("only")], 3);
        assert_eq!(out.len(), 3);
        assert!(
            out[0].trim().is_empty(),
            "top must be blank (bottom-align): {out:?}"
        );
        assert!(out[1].trim().is_empty(), "middle must be blank: {out:?}");
        assert!(
            out[2].trim_start().starts_with("only"),
            "content on the bottom row: {out:?}"
        );
        // Overflow (5 lines in a 3-row zone): the hint takes one row, leaving 2
        // for the tail — so 3 of 5 lines are hidden. Row 0 = hint, rows 1-2 = tail.
        let many: Vec<Line<'static>> = (0..5u32).map(|i| Line::from(format!("line{i}"))).collect();
        let out = rows_of(many, 3);
        assert!(
            out[0].contains('↑'),
            "overflow must show the truncation hint: {out:?}"
        );
        assert!(
            out[0].contains("+3"),
            "hint must name the hidden count (3): {out:?}"
        );
        assert!(
            out[1].trim_start().starts_with("line3"),
            "tail row 1 wrong: {out:?}"
        );
        assert!(
            out[2].trim_start().starts_with("line4"),
            "tail must end on the last line: {out:?}"
        );
    }

    #[test]
    fn slash_popup_lines_windows_to_zone_height_keeping_selection_visible() {
        // 7 commands in a 3-row zone: windowing to the ACTUAL height (not the
        // POPUP_MAX_ROWS ceiling) keeps the highlighted row on-screen wherever
        // the selection is. The old code rendered all 7 then scrolled the tail,
        // stranding the selection off the top.
        let mut p = SlashPopup::new();
        p.update("/"); // every command starts with '/'
        assert_eq!(p.matches.len(), SLASH_COMMANDS.len());
        // Top of the list: window starts at 0, selection (0) is the first row.
        p.sel = 0;
        let top = slash_popup_lines(&p, 3);
        assert_eq!(top.len(), 3, "popup must cap to the zone height");
        assert!(top[0].style.add_modifier.contains(Modifier::REVERSED));
        // Bottom of the list: the window follows so the last selection shows.
        p.sel = SLASH_COMMANDS.len() - 1;
        let bot = slash_popup_lines(&p, 3);
        assert_eq!(bot.len(), 3);
        assert!(
            bot[2].style.add_modifier.contains(Modifier::REVERSED),
            "last selection must be the bottom visible row"
        );
        let want = format!("/{}", p.matches[p.sel].name);
        assert!(
            bot.iter().any(|l| row_text(l).starts_with(&want)),
            "selected command {want:?} not among visible rows"
        );
    }

    // ── Word motion / line editing (Alt+←→, Ctrl-W/U/K/A/E) ──────────

    #[test]
    fn word_boundary_left_finds_previous_word_start() {
        // Caret at end / mid-word → start of "world".
        assert_eq!(word_boundary_left("hello world", 11), 6);
        assert_eq!(word_boundary_left("hello world", 8), 6);
        // A wider gap: caret mid-word still lands on the word start.
        assert_eq!(word_boundary_left("hello  world", 8), 7);
        // Caret on whitespace: the gap AND the word before it are crossed,
        // landing on the previous word's start.
        assert_eq!(word_boundary_left("hello  world", 6), 0);
        // Single word / line start: 0.
        assert_eq!(word_boundary_left("hello", 3), 0);
        assert_eq!(word_boundary_left("hello", 0), 0);
        // CJK has no spaces: a whole non-space run is one word.
        assert_eq!(word_boundary_left("中文 测试", 13), 7);
        assert_eq!(word_boundary_left("中文 测试", 6), 0);
    }

    #[test]
    fn word_boundary_right_finds_next_word_start() {
        // From a word start, mid-word, or on the gap: all land on "world".
        assert_eq!(word_boundary_right("hello world", 0), 6);
        assert_eq!(word_boundary_right("hello world", 2), 6);
        assert_eq!(word_boundary_right("hello world", 5), 6);
        // A wider gap is skipped too.
        assert_eq!(word_boundary_right("hello  world", 5), 7);
        // At the line end it stays put (the buffer crosses to the next line).
        assert_eq!(word_boundary_right("hello", 5), 5);
        // CJK run counts as one word.
        assert_eq!(word_boundary_right("中文 测试", 0), 7);
    }

    #[test]
    fn word_left_right_cross_lines() {
        let mut b = InputBuffer::new();
        b.set_text("foo bar\nbaz qux"); // cursor at end: row 1, col 7
        b.word_left(); // start of "qux"
        assert_eq!((b.row, b.col), (1, 4));
        b.word_left(); // start of "baz"
        assert_eq!((b.row, b.col), (1, 0));
        b.word_left(); // line start → end of the previous line
        assert_eq!((b.row, b.col), (0, 7));
        b.word_left(); // start of "bar"
        assert_eq!((b.row, b.col), (0, 4));

        b.word_right(); // past "bar" → end of line 0
        assert_eq!((b.row, b.col), (0, 7));
        b.word_right(); // line end → start of the next line
        assert_eq!((b.row, b.col), (1, 0));
        b.word_right(); // start of "qux"
        assert_eq!((b.row, b.col), (1, 4));
        b.word_right(); // end of buffer: stays
        assert_eq!((b.row, b.col), (1, 7));
    }

    #[test]
    fn delete_word_back_deletes_word_gap_and_joins_lines() {
        let mut b = InputBuffer::new();
        b.set_text("hello world"); // cursor at end
        b.delete_word_back();
        assert_eq!(b.text(), "hello ");
        assert_eq!(b.col, 6);

        // Caret right after whitespace: the gap plus the word before it go.
        let mut b = InputBuffer::new();
        b.set_text("hello  world"); // cursor at end (col 12)
        for _ in 0..5 {
            b.move_left(); // col 7, right after the two spaces
        }
        b.delete_word_back();
        assert_eq!(b.text(), "world");
        assert_eq!(b.col, 0);

        // A CJK "word" (no internal spaces) deletes as one run.
        let mut b = InputBuffer::new();
        b.set_text("中文 测试"); // cursor at end
        b.delete_word_back();
        assert_eq!(b.text(), "中文 ");
        assert_eq!(b.col, 7);

        // At the start of a line it degrades to backspace (joins lines).
        let mut b = InputBuffer::new();
        b.set_text("ab\ncd");
        b.home(); // row 1, col 0
        b.delete_word_back();
        assert_eq!(b.text(), "abcd");
        assert_eq!((b.row, b.col), (0, 2));
    }

    #[test]
    fn kill_to_line_start_and_end_work_with_unicode() {
        let mut b = InputBuffer::new();
        b.set_text("hello world"); // cursor at end (col 11)
        b.move_left();
        b.move_left();
        b.move_left(); // col 8
        b.kill_to_line_start();
        assert_eq!(b.text(), "rld");
        assert_eq!(b.col, 0);

        let mut b = InputBuffer::new();
        b.set_text("中文 测试"); // cursor at end (col 13)
        b.move_left();
        b.move_left(); // col 7
        b.kill_to_line_start();
        assert_eq!(b.text(), "测试");
        assert_eq!(b.col, 0);

        let mut b = InputBuffer::new();
        b.set_text("hello world");
        b.home();
        b.move_right();
        b.move_right(); // col 2
        b.kill_to_line_end();
        assert_eq!(b.text(), "he");
        assert_eq!(b.col, 2);

        let mut b = InputBuffer::new();
        b.set_text("中文测试"); // cursor at end (col 12)
        b.move_left();
        b.move_left(); // col 6
        b.kill_to_line_end();
        assert_eq!(b.text(), "中文");
        assert_eq!(b.col, 6);
    }
}

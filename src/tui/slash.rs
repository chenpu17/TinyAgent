//! Slash-command surface: the command grammar (`classify` / `SlashCmd`),
//! the read-only replies, the completion popup state machine, and the
//! `/context` report renderer.

use super::theme::{dim_fg, fmt_tokens, fmt_tokens_round, USER_FG};
use crate::config::Config;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// What a `/`-prefixed input line asks for. `Message` = not a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SlashCmd {
    Message,
    Clear,
    /// `/export [path]` — dump the conversation to a file (or clipboard when no
    /// path is given and a clipboard tool is available). The optional arg is the
    /// destination path, whitespace-trimmed.
    Export(Option<String>),
    /// `/out [n]` — reprint the nth-last tool result in full (default n=1,
    /// the most recent). Successful results stay collapsed in scrollback
    /// (only short failures auto-expand), so this is the in-TUI way to read
    /// a full output without /export round-tripping a file.
    Out(Option<usize>),
    Model,
    Limits,
    Tools,
    System,
    /// `/context` — the estimated token composition of the next request's
    /// context (system / tools / messages by kind, calibrated when an anchor
    /// is live). Worker-side query (the agent lives there), rendered by the
    /// UI from the returned `ContextBreakdown`.
    Context,
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
pub(super) fn classify(trimmed: &str) -> SlashCmd {
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
    // Only /export and /out take arguments. After the classify refactor the
    // other commands silently dropped a trailing arg (`/model foo` ran /model
    // with "foo" ignored); reject them as unknown so the typo is surfaced.
    // Unknown carries the full `rest` so the message names what was typed.
    let stray_arg = arg.is_some();
    match word {
        "clear" if !stray_arg => SlashCmd::Clear,
        "export" => SlashCmd::Export(arg),
        // /out takes an optional 1-based index; anything unparseable (or 0,
        // which has no nth-last meaning) reads as unknown so the message
        // names the typo.
        "out" => match arg.as_deref().map(str::parse::<usize>) {
            None => SlashCmd::Out(None),
            Some(Ok(n)) if n >= 1 => SlashCmd::Out(Some(n)),
            Some(_) => SlashCmd::Unknown(rest.to_string()),
        },
        "model" if !stray_arg => SlashCmd::Model,
        "limits" if !stray_arg => SlashCmd::Limits,
        "tools" if !stray_arg => SlashCmd::Tools,
        "system" if !stray_arg => SlashCmd::System,
        "context" if !stray_arg => SlashCmd::Context,
        "help" if !stray_arg => SlashCmd::Help,
        "quit" | "exit" if !stray_arg => SlashCmd::Quit,
        _ => SlashCmd::Unknown(rest.to_string()),
    }
}

/// Read-only reply text for an info slash command (everything but Clear, which
/// has its own "[history cleared]" side effect). Pure given the config.
pub(super) fn slash_reply(cmd: &SlashCmd, cfg: &Config) -> String {
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
        // Message/Clear never reach here; empty keeps the match exhaustive.
        // Export/Out, Context and Clear have their own side-effecting arms in
        // the key handler (Context asks the worker for the breakdown).
        SlashCmd::Message
        | SlashCmd::Clear
        | SlashCmd::Export(_)
        | SlashCmd::Out(_)
        | SlashCmd::Context
        | SlashCmd::Quit => String::new(),
    }
}

// ── Slash-command completion popup ────────────────────────────────────

/// Render `/context`'s report from a breakdown snapshot. Pure given
/// (breakdown, window) so the tests drive it with hand-built literals. Every
/// number is an ESTIMATE on the compress yardstick (see context_report's
/// module docs); the header line states which register the numbers are in —
/// `calibrated ×s` when a measured anchor is live, `uncalibrated heuristic`
/// otherwise. Per-tool lines are capped per category (sorted by tokens,
/// display-only copy — the breakdown itself keeps tool-list order).
pub(super) fn render_context_report(
    bd: &crate::context_report::ContextBreakdown,
    max_context_tokens: u64,
) -> String {
    const BAR_WIDTH: u64 = 14;
    const PER_CATEGORY_TOOLS: usize = 5;
    // Label column: the longest standing label is "skill content" (13) —
    // a narrower pad lets long labels push the tokens column out of
    // alignment (external re-review #1). Tool names are identifiers that
    // can exceed any sane column (MCP display names run long), so those
    // truncate with "…" instead of drifting; the column-alignment test
    // pins both widths.
    const LABEL_COL: usize = 13;
    const TOOL_COL: usize = 24;
    let total = bd.total_tokens;
    // Percent with one decimal over a 0-safe denominator, u64 all the way
    // (the ×1000 trick keeps the layout arithmetic integer and saturating).
    let pct_milli = |x: u64| x.saturating_mul(1000) / total.max(1);
    let bar = |x: u64| {
        let n = (pct_milli(x).saturating_mul(BAR_WIDTH) / 1000) as usize;
        if n == 0 {
            "·".to_string()
        } else {
            "#".repeat(n)
        }
    };
    let section = |out: &mut String, label: &str, tokens: u64| {
        out.push_str(&format!(
            "  {:<lw$} {:>7}  {:>5}.{}%  {}\n",
            label,
            fmt_tokens(tokens),
            pct_milli(tokens) / 10,
            pct_milli(tokens) % 10,
            bar(tokens),
            lw = LABEL_COL,
        ));
    };
    let sub = |out: &mut String, label: &str, tokens: u64| {
        out.push_str(&format!(
            "    {:<lw$} {:>7}  {:>5}.{}%\n",
            label,
            fmt_tokens(tokens),
            pct_milli(tokens) / 10,
            pct_milli(tokens) % 10,
            lw = LABEL_COL,
        ));
    };

    let mut s = String::new();
    match bd.calibration {
        Some(c) => s.push_str(&format!(
            "context (estimated): {} tok · calibrated ×{:.2} (anchor: measured prompt {} tok)\n",
            fmt_tokens(total),
            c.scale,
            fmt_tokens(c.measured_prompt_tokens),
        )),
        None => s.push_str(&format!(
            "context (estimated): {} tok · uncalibrated heuristic (chars/4; no measurement yet)\n",
            fmt_tokens(total),
        )),
    }
    section(&mut s, "system", bd.system.tokens);
    sub(&mut s, "env header", bd.system.env);
    sub(&mut s, "base", bd.system.base);
    sub(&mut s, "skill intro", bd.system.skill_intro);

    section(&mut s, "tools", bd.tools.tokens);
    for (label, cat) in [
        ("builtin", &bd.tools.builtin),
        ("mcp", &bd.tools.mcp),
        ("skill", &bd.tools.skill),
        ("custom", &bd.tools.custom),
    ] {
        sub(&mut s, label, cat.tokens);
        // Display-only copy, sorted by size — the breakdown itself keeps
        // tool-list order (same as /tools reports it).
        let mut entries: Vec<&crate::context_report::ToolEntry> = cat.tools.iter().collect();
        entries.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.name.cmp(&b.name)));
        for e in entries.iter().take(PER_CATEGORY_TOOLS) {
            // Fit long identifiers (MCP display names) to the column —
            // chars, not bytes, so the "…" stays a single column unit.
            let name = if e.name.chars().count() > TOOL_COL {
                let mut t: String = e.name.chars().take(TOOL_COL - 1).collect();
                t.push('…');
                t
            } else {
                e.name.clone()
            };
            s.push_str(&format!(
                "      {:<tw$} {:>7}\n",
                name,
                fmt_tokens(e.tokens),
                tw = TOOL_COL
            ));
        }
        if entries.len() > PER_CATEGORY_TOOLS {
            s.push_str(&format!(
                "      … +{} more\n",
                entries.len() - PER_CATEGORY_TOOLS
            ));
        }
    }

    section(&mut s, "messages", bd.messages.tokens);
    let k = &bd.messages.by_kind;
    for (label, tokens) in [
        ("user text", k.user_text),
        ("assistant", k.assistant_text),
        ("tool use", k.tool_use),
        ("tool results", k.tool_result),
        ("images", k.image),
        ("documents", k.document),
        ("skill content", k.skill_content),
        ("other", k.other),
    ] {
        sub(&mut s, label, tokens);
    }

    if let Some(used_pct) = total.saturating_mul(100).checked_div(max_context_tokens) {
        let free = max_context_tokens.saturating_sub(total);
        // Round window numbers read cleaner without a trailing ".0".
        let max_s = fmt_tokens_round(max_context_tokens);
        s.push_str(&format!(
            "window: {}% of {} max_context_tokens ({} free)\n",
            used_pct,
            max_s,
            fmt_tokens(free),
        ));
    } else {
        s.push_str("window: no max_context_tokens configured\n");
    }
    // Multi-line, rendered by its row as-is (same as /help's listing).
    s
}

/// One slash command as the completion popup shows it. KEEP IN SYNC with
/// `classify` above — that match is the actual command set; this table only
/// describes it (name, argument hint, one-line description).
pub(super) struct SlashSpec {
    pub(super) name: &'static str,
    arg_hint: &'static str,
    desc: &'static str,
}

pub(super) const SLASH_COMMANDS: [SlashSpec; 10] = [
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
        name: "out",
        arg_hint: "[n]",
        desc: "show a tool result in full (default: last)",
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
        name: "context",
        arg_hint: "",
        desc: "context window composition (estimated tokens)",
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
pub(super) const POPUP_MAX_ROWS: usize = 8;

/// Floating slash-command completion state. Pure state machine, no terminal:
/// `update` recomputes the visible matches from the input text (empty list =
/// closed), the key handling lives in `popup_key`. No mouse support —
/// click-to-complete buys little over Tab/Enter.
pub(super) struct SlashPopup {
    /// The `/`-stripped prefix the current matches were filtered with;
    /// selection resets whenever it changes.
    prefix: String,
    pub(super) matches: Vec<&'static SlashSpec>,
    pub(super) sel: usize,
    /// Text a dismiss (Esc, or a completion just applied) was taken on. The
    /// popup stays closed while the buffer still equals it — otherwise
    /// `update` would reopen on the very next frame, making Esc/completion
    /// unable to close the popup at all. Any further edit lifts it.
    dismissed: Option<String>,
}

impl SlashPopup {
    pub(super) fn new() -> Self {
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
    pub(super) fn update(&mut self, text: &str) {
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

    pub(super) fn is_open(&self) -> bool {
        !self.matches.is_empty()
    }

    /// Wrap-around selection moves (Up/Down cycle through the matches).
    pub(super) fn next(&mut self) {
        if self.is_open() {
            self.sel = (self.sel + 1) % self.matches.len();
        }
    }
    pub(super) fn prev(&mut self) {
        if self.is_open() {
            self.sel = (self.sel + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// The completion text for the selected command: bare "/name", or
    /// "/name " WITH a trailing space for argument-taking commands (/export),
    /// so the path can be typed straight on.
    pub(super) fn completion(&self) -> Option<String> {
        let c = self.matches.get(self.sel)?;
        Some(if c.arg_hint.is_empty() {
            format!("/{}", c.name)
        } else {
            format!("/{} ", c.name)
        })
    }

    /// Close until the text changes (see `dismissed`).
    pub(super) fn dismiss(&mut self, text: &str) {
        self.dismissed = Some(text.to_string());
        self.matches.clear();
        self.prefix.clear();
        self.sel = 0;
    }
}

/// What an Input-mode key does while the popup is open. None = not
/// intercepted, the caller's normal handling applies unchanged.
pub(super) enum PopupAction {
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
pub(super) fn popup_key(
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
pub(super) fn visible_window(len: usize, sel: usize, max: usize) -> (usize, usize) {
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
pub(super) fn slash_popup_lines(popup: &SlashPopup, max_rows: usize) -> Vec<Line<'static>> {
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
                    Span::styled(format!(" {}", c.arg_hint), Style::default().fg(dim_fg()))
                },
                Span::styled(format!("  {}", c.desc), Style::default().fg(dim_fg())),
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

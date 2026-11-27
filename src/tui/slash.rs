//! Slash-command surface: the command grammar (`classify` / `SlashCmd`),
//! the read-only replies, the completion popup state machine, and the
//! `/context` report renderer.

use super::render::{keycap, truncate_line};
use super::theme::{dim_fg, fmt_tokens, fmt_tokens_round, keycap_bg, USER_FG};
use crate::config::Config;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::Style;
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
    /// `/call [n]` — reprint the nth-last tool call with its full input JSON.
    Call(Option<usize>),
    /// `/thinking [n]` — reprint the nth-last reasoning block in full.
    Thinking(Option<usize>),
    /// `/queue` — list messages waiting behind the in-flight run.
    Queue,
    /// `/unqueue` — withdraw the newest queued message for editing.
    Unqueue,
    /// `/resume` — explicitly continue a queue paused after a failed run.
    Resume,
    Model,
    Limits,
    Tools,
    System,
    /// `/context` — the estimated token composition of the next request's
    /// context (system / tools / messages by kind, calibrated when an anchor
    /// is live). Worker-side query (the agent lives there), rendered by the
    /// UI from the returned `ContextBreakdown`.
    Context,
    /// `/compact [focus]` — fold the history into a running summary now
    /// (idle only). The optional arg is a free-form focus hint for the
    /// summarizer ("focus on the API changes"). Worker-side, like `/context`:
    /// the fold runs on the thread that owns the agent and reports back
    /// through `UiMsg::CompactDone`; the `Compressed { trigger: Manual }`
    /// event it emits on the way is the transcript's audit row.
    Compact(Option<String>),
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
        Some(raw_arg)
    };
    let unknown = || SlashCmd::Unknown(rest.to_string());
    let Some(spec) = SLASH_COMMANDS.iter().find(|c| c.accepts(word)) else {
        return unknown();
    };
    // Argument grammar belongs to the same table as completion and /help.
    // Invalid input remains Unknown (including the original argument), so the
    // caller can leave it in the editor for correction instead of losing it.
    let parsed = match (spec.arg_rule, arg) {
        (ArgRule::None, None) => Some(ParsedArg::None),
        (ArgRule::None, Some(_)) => None,
        (ArgRule::OptionalText, value) => Some(ParsedArg::Text(value.map(str::to_string))),
        (ArgRule::OptionalIndex, None) => Some(ParsedArg::Index(None)),
        (ArgRule::OptionalIndex, Some(value)) => match value.parse::<usize>() {
            Ok(n) if n >= 1 => Some(ParsedArg::Index(Some(n))),
            _ => None,
        },
    };
    match (spec.kind, parsed) {
        (SlashKind::Clear, Some(ParsedArg::None)) => SlashCmd::Clear,
        (SlashKind::Export, Some(ParsedArg::Text(value))) => SlashCmd::Export(value),
        (SlashKind::Compact, Some(ParsedArg::Text(value))) => SlashCmd::Compact(value),
        (SlashKind::Out, Some(ParsedArg::Index(n))) => SlashCmd::Out(n),
        (SlashKind::Call, Some(ParsedArg::Index(n))) => SlashCmd::Call(n),
        (SlashKind::Thinking, Some(ParsedArg::Index(n))) => SlashCmd::Thinking(n),
        (SlashKind::Queue, Some(ParsedArg::None)) => SlashCmd::Queue,
        (SlashKind::Unqueue, Some(ParsedArg::None)) => SlashCmd::Unqueue,
        (SlashKind::Resume, Some(ParsedArg::None)) => SlashCmd::Resume,
        (SlashKind::Model, Some(ParsedArg::None)) => SlashCmd::Model,
        (SlashKind::Limits, Some(ParsedArg::None)) => SlashCmd::Limits,
        (SlashKind::Tools, Some(ParsedArg::None)) => SlashCmd::Tools,
        (SlashKind::System, Some(ParsedArg::None)) => SlashCmd::System,
        (SlashKind::Context, Some(ParsedArg::None)) => SlashCmd::Context,
        (SlashKind::Help, Some(ParsedArg::None)) => SlashCmd::Help,
        (SlashKind::Quit, Some(ParsedArg::None)) => SlashCmd::Quit,
        _ => unknown(),
    }
}

/// Read-only reply text for an info slash command (everything but Clear, which
/// has its own "[history cleared]" side effect, and the worker-backed Compact
/// and Context). Pure given the config.
pub(super) fn slash_reply(cmd: &SlashCmd, cfg: &Config) -> String {
    match cmd {
        SlashCmd::Model => format!(
            "model {} · provider {:?} · max_tokens {} · base {}",
            cfg.model,
            cfg.provider,
            cfg.max_tokens,
            // Same credential-display discipline as every other URL surface:
            // a `https://user:pass@host` base_url echoed raw into the TUI
            // would put the password on screen (round-2 review).
            cfg.base_url
                .as_deref()
                .map(|u| tiny_agent::config::redact_url_credentials(u).into_owned())
                .unwrap_or_else(|| "(default)".to_string()),
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
            "system (configured base): {}",
            cfg.system_prompt.as_deref().unwrap_or("(none)"),
        ),
        // Commands and keyboard help intentionally live here, next to the
        // grammar: adding a table entry updates both completion and /help.
        SlashCmd::Help => render_slash_help(),
        SlashCmd::Unknown(name) => format!("[unknown command: /{name} — try /help]"),
        // Message/Clear never reach here; empty keeps the match exhaustive.
        // Export/Out, Compact, Context and Clear have their own side-effecting
        // arms in the key handler (Compact and Context ask the worker).
        SlashCmd::Message
        | SlashCmd::Clear
        | SlashCmd::Export(_)
        | SlashCmd::Compact(_)
        | SlashCmd::Out(_)
        | SlashCmd::Call(_)
        | SlashCmd::Thinking(_)
        | SlashCmd::Queue
        | SlashCmd::Unqueue
        | SlashCmd::Resume
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
/// display-only copy — the breakdown itself keeps tool-list order). A table
/// over the kernel's advisory lines (10k estimated tokens / 30 tools) gets a
/// short advisory block under the tool categories — prose only, nothing is
/// trimmed.
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
    // Fixed-cost advisory: the whole table rides on every request and no
    // compaction folds it. Judged on the numbers as printed above (scaled
    // when a calibration is live); absent under both lines. Prose, not a
    // column row — the kernel's one text, also used by its diag line.
    if let Some(lines) =
        crate::context_report::tool_defs_advisory_lines(bd.tools.tokens, bd.tools.tool_count())
    {
        for (i, line) in lines.iter().enumerate() {
            let lead = if i == 0 { "    advisory: " } else { "      " };
            s.push_str(lead);
            s.push_str(line);
            s.push('\n');
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
    // Carried reasoning state (`Config.preserved_reasoning`) rides on every
    // request until its turn ends — context the model pays for. Sparse: a
    // session that carries none shows no line, as before the bucket existed.
    if k.reasoning > 0 {
        sub(&mut s, "reasoning", k.reasoning);
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
        // Unset window: say what is missing and how to fix it — the same
        // fact the startup note (`Limits::context_window_hint_line`, on the
        // welcome row) announces.
        s.push_str(
            "window: no max_context_tokens configured — set it to the model's context \
             window to compact on a token estimate\n",
        );
    }
    // Multi-line, rendered by its row as-is (same as /help's listing).
    s
}

/// One command table drives classification, completion, availability and the
/// command portion of /help. Command names must never be repeated in app.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashKind {
    Clear,
    Export,
    Out,
    Call,
    Thinking,
    Queue,
    Unqueue,
    Resume,
    Model,
    Limits,
    Tools,
    System,
    Context,
    Compact,
    Help,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgRule {
    None,
    OptionalText,
    OptionalIndex,
}

#[derive(Debug)]
enum ParsedArg {
    None,
    Text(Option<String>),
    Index(Option<usize>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpGroup {
    Session,
    Details,
    Queue,
}

pub(super) struct SlashSpec {
    pub(super) name: &'static str,
    aliases: &'static [&'static str],
    arg_hint: &'static str,
    desc: &'static str,
    kind: SlashKind,
    arg_rule: ArgRule,
    group: HelpGroup,
    /// Hide from completion while a run is active. Direct invocation keeps its
    /// established app-side refusal, so this is presentation, not enforcement.
    idle_only: bool,
}

impl SlashSpec {
    fn accepts(&self, word: &str) -> bool {
        self.name == word || self.aliases.contains(&word)
    }

    fn usage(&self) -> String {
        if self.arg_hint.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.arg_hint)
        }
    }
}

const fn no_arg(
    name: &'static str,
    desc: &'static str,
    kind: SlashKind,
    group: HelpGroup,
    idle_only: bool,
) -> SlashSpec {
    SlashSpec {
        name,
        aliases: &[],
        arg_hint: "",
        desc,
        kind,
        arg_rule: ArgRule::None,
        group,
        idle_only,
    }
}

const fn indexed(name: &'static str, desc: &'static str, kind: SlashKind) -> SlashSpec {
    SlashSpec {
        name,
        aliases: &[],
        arg_hint: "[n]",
        desc,
        kind,
        arg_rule: ArgRule::OptionalIndex,
        group: HelpGroup::Details,
        idle_only: false,
    }
}

pub(super) const SLASH_COMMANDS: &[SlashSpec] = &[
    no_arg(
        "clear",
        "clear the conversation history",
        SlashKind::Clear,
        HelpGroup::Session,
        true,
    ),
    SlashSpec {
        name: "export",
        aliases: &[],
        arg_hint: "[path]",
        desc: "dump the transcript to a file (or clipboard)",
        kind: SlashKind::Export,
        arg_rule: ArgRule::OptionalText,
        group: HelpGroup::Details,
        idle_only: false,
    },
    indexed(
        "out",
        "show a tool result in full (default: last)",
        SlashKind::Out,
    ),
    indexed(
        "call",
        "show a tool call's full input (default: last)",
        SlashKind::Call,
    ),
    indexed(
        "thinking",
        "show a reasoning block in full (default: last)",
        SlashKind::Thinking,
    ),
    no_arg(
        "queue",
        "list messages queued behind the current run",
        SlashKind::Queue,
        HelpGroup::Queue,
        false,
    ),
    no_arg(
        "unqueue",
        "withdraw the newest queued message for editing",
        SlashKind::Unqueue,
        HelpGroup::Queue,
        false,
    ),
    no_arg(
        "resume",
        "continue a queue paused after a failed run",
        SlashKind::Resume,
        HelpGroup::Queue,
        true,
    ),
    no_arg(
        "model",
        "model, provider, max_tokens, base URL",
        SlashKind::Model,
        HelpGroup::Session,
        false,
    ),
    no_arg(
        "limits",
        "turn and timeout limits",
        SlashKind::Limits,
        HelpGroup::Session,
        false,
    ),
    no_arg(
        "tools",
        "enabled tool list",
        SlashKind::Tools,
        HelpGroup::Session,
        false,
    ),
    no_arg(
        "system",
        "configured base system prompt",
        SlashKind::System,
        HelpGroup::Session,
        false,
    ),
    no_arg(
        "context",
        "context window composition (estimated tokens)",
        SlashKind::Context,
        HelpGroup::Session,
        true,
    ),
    SlashSpec {
        name: "compact",
        aliases: &[],
        arg_hint: "[focus]",
        desc: "fold the history into a summary now (idle only)",
        kind: SlashKind::Compact,
        arg_rule: ArgRule::OptionalText,
        group: HelpGroup::Session,
        idle_only: true,
    },
    no_arg(
        "help",
        "commands and keyboard help",
        SlashKind::Help,
        HelpGroup::Session,
        false,
    ),
    SlashSpec {
        aliases: &["exit"],
        ..no_arg(
            "quit",
            "exit the TUI (alias: /exit)",
            SlashKind::Quit,
            HelpGroup::Session,
            false,
        )
    },
];

fn render_slash_help() -> String {
    // A single width across groups keeps the detail column stable. ASCII usage
    // strings make byte length and terminal cell width identical here.
    let kw = SLASH_COMMANDS
        .iter()
        .map(|spec| spec.usage().len())
        .max()
        .unwrap_or(0);
    let mut s = String::new();
    for (group, title) in [
        (HelpGroup::Session, "commands — session:"),
        (HelpGroup::Details, "commands — details:"),
        (HelpGroup::Queue, "commands — queue:"),
    ] {
        s.push_str(title);
        for spec in SLASH_COMMANDS.iter().filter(|spec| spec.group == group) {
            s.push_str(&format!("\n  {:<kw$}  {}", spec.usage(), spec.desc));
        }
        s.push('\n');
    }
    s.push_str(
        "editing:\n\
         \x20 Left/Right move · Ctrl/Alt+Left/Right by word · Home/End or Ctrl-A/Ctrl-E\n\
         \x20 Backspace deletes left · Delete/Ctrl-D deletes right · Ctrl-W deletes a word\n\
         \x20 Ctrl-U kills to line start · Ctrl-K kills to line end\n\
         history:\n\
         \x20 Up/Down recall in a single-line input; Alt+Up/Down always recall\n\
         multiline:\n\
         \x20 Shift+Enter, Alt+Enter or Ctrl+J inserts a newline; trailing \\ + Enter does too\n\
         queue / cancel:\n\
         \x20 Enter while running queues; /queue lists and /unqueue recalls the newest\n\
         \x20 Esc or Ctrl-C cancels the run and drops its queued messages\n\
         context:\n\
         \x20 /context shows the estimated composition; /compact [focus] folds the history now\n\
         \x20 /compact is idle-only, bills the summary request(s), and Esc cancels it\n\
         native scroll / copy:\n\
         \x20 Use the terminal's scrollback, wheel and selection; /export copies or saves\n\
         exit:\n\
         \x20 /quit or /exit · Ctrl-D on empty idle input · Ctrl-C twice while idle\n\
         details:\n\
         \x20 /out, /call and /thinking take a 1-based nth-last index (default: latest)\n\
         \x20 Ctrl+O reprints the latest tool call, result or thinking detail",
    );
    s
}

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
    /// Current run state: idle-only commands are hidden while busy.
    busy: bool,
}

impl SlashPopup {
    pub(super) fn new() -> Self {
        Self {
            prefix: String::new(),
            matches: Vec::new(),
            sel: 0,
            dismissed: None,
            busy: false,
        }
    }

    /// Compatibility entry point for pure grammar tests: idle, all commands.
    #[cfg(test)]
    pub(super) fn update(&mut self, text: &str) {
        self.update_for(text, false);
    }

    /// Recompute from input text and run state. Open only while typing the
    /// command NAME: `/` with no whitespace yet. Bare `/` lists all commands
    /// available in this state; busy mode hides (rather than merely disables)
    /// idle-only commands. Prefix matching remains case-sensitive.
    pub(super) fn update_for(&mut self, text: &str, busy: bool) {
        let availability_changed = self.busy != busy;
        self.busy = busy;
        if self.dismissed.as_deref() == Some(text) {
            return; // dismissed on exactly this text: stay closed
        }
        self.dismissed = None;
        let matches: Vec<&'static SlashSpec> = text
            .strip_prefix('/')
            .filter(|rest| !rest.contains(char::is_whitespace))
            .map(|p| {
                SLASH_COMMANDS
                    .iter()
                    .filter(|c| (!busy || !c.idle_only) && c.name.starts_with(p))
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
        if p != self.prefix || availability_changed {
            self.sel = 0; // the filtered set changed; restart at the top
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
    /// "/name " WITH a trailing space for argument-taking commands (/export,
    /// /compact), so the argument can be typed straight on.
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
    /// user presses Enter a second time to actually run it. (The key
    /// handler lets Enter run the command when the typed text already equals
    /// the completion — nothing to complete, see `Session::on_popup_key`.)
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
    // Terminals disagree on whether Shift+Tab arrives as BackTab or Tab+SHIFT;
    // accept both spellings. Other modifier chords still fall through.
    let reverse_tab = (code == KeyCode::BackTab
        && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT))
        || (code == KeyCode::Tab && modifiers == KeyModifiers::SHIFT);
    if reverse_tab {
        popup.prev();
        return Some(PopupAction::Consumed);
    }
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

/// Original renderer retained as a small test-compatible wrapper: unbounded
/// width, no footer. The app should use [`slash_popup_lines_for`].
#[cfg(test)]
pub(super) fn slash_popup_lines(popup: &SlashPopup, max_rows: usize) -> Vec<Line<'static>> {
    render_slash_popup(popup, max_rows, usize::MAX, false)
}

/// Render popup rows within the actual zone width, with a one-line key footer.
/// `/name [arg]` keeps its accent; descriptions and the footer use the theme's
/// dim color in both dark and light mode. Long rows are cell-width truncated
/// with an ellipsis, never byte-sliced or allowed to push the caret sideways.
/// The footer reserves a row only when at least two rows are available.
pub(super) fn slash_popup_lines_for(
    popup: &SlashPopup,
    max_rows: usize,
    max_width: usize,
) -> Vec<Line<'static>> {
    render_slash_popup(popup, max_rows, max_width, true)
}

fn render_slash_popup(
    popup: &SlashPopup,
    max_rows: usize,
    max_width: usize,
    with_footer: bool,
) -> Vec<Line<'static>> {
    if max_width == 0 || popup.matches.is_empty() {
        return Vec::new();
    }
    let max = max_rows.clamp(1, POPUP_MAX_ROWS);
    let show_footer = with_footer && max >= 2;
    let command_rows = if show_footer { max - 1 } else { max };
    let (skip, take) = visible_window(popup.matches.len(), popup.sel, command_rows);
    let mut lines: Vec<Line<'static>> = popup.matches[skip..skip + take]
        .iter()
        .enumerate()
        .map(|(row, c)| {
            let selected = skip + row == popup.sel;
            Line::from(vec![
                Span::styled(
                    if selected { " ❯ " } else { "   " },
                    Style::default().fg(USER_FG),
                ),
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
    let content_w = lines
        .iter()
        .map(|line| line.width())
        .max()
        .unwrap_or(0)
        .min(max_width);
    for line in &mut lines {
        *line = truncate_line(std::mem::take(line), content_w);
    }
    if popup.sel >= skip && popup.sel < skip + take {
        let line = &mut lines[popup.sel - skip];
        line.style = Style::default().bg(keycap_bg());
        let pad = content_w.saturating_sub(line.width());
        if pad > 0 {
            line.spans.push(Span::raw(" ".repeat(pad)));
        }
    }
    if show_footer {
        lines.push(truncate_line(
            Line::from(vec![
                Span::raw(" "),
                keycap("Tab"),
                Span::styled(" complete  ", Style::default().fg(dim_fg())),
                keycap("Enter"),
                Span::styled(" select / again run  ", Style::default().fg(dim_fg())),
                keycap("Esc"),
                Span::styled(" close", Style::default().fg(dim_fg())),
            ]),
            max_width,
        ));
    }
    lines
}

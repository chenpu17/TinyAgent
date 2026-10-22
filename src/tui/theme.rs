//! Theme: the one place history-row / chrome colors are defined, plus the
//! light/dark knob (`TINYAGENT_TUI_THEME`) and the small number-formatting
//! helpers shared by the status bar, the slash replies, and the tool
//! timing lines.

use ratatui::style::{Color, Style};
use std::time::Duration;

// Theme: the one place history-row / chrome colors are defined. `dim_fg()`
// is a function, not a const, because it flips with TINYAGENT_TUI_THEME
// (see `theme_light_from`): Gray (color 7) is near-invisible on light
// terminals, where DarkGray (color 8) reads as the intended mid-gray.
pub(super) const USER_FG: Color = Color::Cyan;
pub(super) const AGENT_FG: Color = Color::Green;
pub(super) const TOOL_CALL_FG: Color = Color::Yellow;
pub(super) const TOOL_OK_FG: Color = Color::Green;
pub(super) const TOOL_ERR_FG: Color = Color::Red;

// Markdown header emphasis by level: h1 pops, h2/h3 steady, deeper recedes.
pub(super) const MD_H1_FG: Color = Color::Yellow;
pub(super) const MD_H23_FG: Color = Color::Cyan;
pub(super) const MD_DEEP_FG: Color = Color::DarkGray;

// Status-bar model badge: black on cyan (echoes USER_FG as a solid block).
pub(super) const BADGE_FG: Color = Color::Black;
pub(super) const BADGE_BG: Color = Color::Cyan;

// ── Light-terminal theme knob ────────────────────────────────────────────
// The named colors above map onto the terminal's 16-color palette, which
// FLIPS with the terminal theme. On a light background Gray (color 7) — the
// one dim color used across system rows, previews, status segments, code
// bars, and the empty-state sheet — is near-white and effectively invisible,
// and tui-markdown styles inline code fg White (white-on-white). No
// autodetection (COLORFGBG et al. are unreliable); `TINYAGENT_TUI_THEME=`
// light|dark (default dark) is the explicit knob, read once at TUI start.
// Everything else (role/tool colors, the badge, the ANSI-parsing palettes
// that carry TOOL OUTPUT colors chosen by other programs) stays put — those
// read fine on both backgrounds.

/// Whether the light palette is active. Set once in `run` from the env;
/// tests read the unset (dark) default.
static THEME_LIGHT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Pure decision core of the env knob: exactly `light` selects the light
/// palette; anything else (including unset) stays dark.
pub(super) fn theme_light_from(var: Option<&str>) -> bool {
    var.is_some_and(|v| v.eq_ignore_ascii_case("light"))
}

/// Bind the knob from the real environment. Called once at TUI start; later
/// calls are no-ops (OnceLock keeps the first value).
pub(super) fn set_theme_from_env(var: Option<String>) {
    let _ = THEME_LIGHT.set(theme_light_from(var.as_deref()));
}

/// The dim foreground: Gray on dark terminals (color 7, the classic dim),
/// DarkGray on light ones (color 8 — there it IS the mid-gray).
pub(super) fn dim_fg() -> Color {
    if *THEME_LIGHT.get().unwrap_or(&false) {
        Color::DarkGray
    } else {
        Color::Gray
    }
}

/// Inline-code foreground: tui-markdown hardcodes fg White, which vanishes on
/// light backgrounds — the light theme re-dyes it cyan (readable on both).
pub(super) fn code_fg() -> Color {
    if *THEME_LIGHT.get().unwrap_or(&false) {
        MD_H23_FG
    } else {
        Color::White
    }
}

/// Compact tool duration: one decimal under 10s ("1.2s"), whole seconds at
/// 10s and above ("12s") — the last digit stops jittering on long calls.
pub(super) fn fmt_tool_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{secs:.0}s")
    }
}

/// Session/run elapsed for the status bar: "Ns" under a minute, else "MmSSs"
/// ("4m05s") so long runs stay readable without growing the segment.
pub(super) fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Compact token count for the status line: 850, 1.2k, 12k, 1.0M.
pub(super) fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// fmt_tokens for a ROUND configured size: "256k" reads cleaner than
/// "256.0k", so drop a trailing ".0" on the denominator only — live
/// measured numerators keep their decimal (e.g. "54.7k"). Shared by the
/// status bar's ctx segment and /context's window line so the two cannot
/// drift apart.
pub(super) fn fmt_tokens_round(n: u64) -> String {
    fmt_tokens(n).replace(".0k", "k").replace(".0M", "M")
}

/// Format the status bar's context-window segment: `ctx 54.7k` when the
/// model's window size is unknown (limits.max_context_tokens unset), or
/// `ctx 21% (54.7k/256.0k)` when it is configured. The color grades with
/// fill level — dim while comfortable, yellow past 60%, red past 80% — so a
/// glance tells you how much headroom is left. None before the first Usage
/// event (nothing meaningful to show).
pub(super) fn fmt_ctx(ctx_tokens: u64, max_context_tokens: u64) -> Option<(String, Style)> {
    if ctx_tokens == 0 {
        return None;
    }
    if max_context_tokens == 0 {
        return Some((
            format!("ctx {}", fmt_tokens(ctx_tokens)),
            Style::default().fg(dim_fg()),
        ));
    }
    let pct = ctx_tokens.saturating_mul(100) / max_context_tokens.max(1);
    let fg = if pct >= 80 {
        TOOL_ERR_FG
    } else if pct >= 60 {
        TOOL_CALL_FG
    } else {
        dim_fg()
    };
    // The window size is a round configured number — see fmt_tokens_round.
    let max_s = fmt_tokens_round(max_context_tokens);
    Some((
        format!("ctx {}% ({}/{})", pct, fmt_tokens(ctx_tokens), max_s),
        Style::default().fg(fg),
    ))
}

//! Theme: the one place history-row / chrome colors are defined, plus the
//! light/dark knob (`TINYAGENT_TUI_THEME`), the color-depth knob
//! (`TINYAGENT_TUI_COLOR` / `COLORTERM`) and the small number-formatting
//! helpers shared by the status bar, the slash replies, and the tool timing
//! lines.
//!
//! The palette is deliberately small. Conversation text — the user's message
//! and the agent's answer — keeps the terminal's default foreground, so it
//! reads exactly like the rest of the terminal. Around it sit five inks, each
//! with one job:
//!
//! | ink    | used for                                                       |
//! |--------|----------------------------------------------------------------|
//! | accent | role and tool glyphs, the busy frame, inline code               |
//! | dim    | secondary text: tool lines, thinking, notices, the status readout |
//! | faint  | tertiary marks: guides, rules, key labels                       |
//! | warn   | the few states that want a glance (cancelling, queue held)      |
//! | error  | failures                                                       |
//!
//! Nothing paints a background: the terminal's own theme stays the surface,
//! so the UI never shows a tinted panel that clashes with it.

use ratatui::style::{Color, Style};
use std::time::Duration;

/// How many colors the terminal is trusted to draw. Picked once at TUI start
/// (see [`color_depth_from`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ColorDepth {
    /// 24-bit: the exact tints below.
    TrueColor,
    /// xterm's 256-color palette: a hand-picked standard entry of matching
    /// visual weight (not the computed nearest one).
    Indexed256,
    /// Only the 16 ANSI colors: whatever the terminal's theme maps there.
    Ansi16,
}

/// Pure decision core of the depth knob. `override_var` (`TINYAGENT_TUI_COLOR`)
/// wins when it names a depth (`truecolor` / `256` / `16`, case-insensitive);
/// otherwise `COLORTERM=truecolor|24bit` — what modern terminals export —
/// selects true color, and anything else (including unset, or a multiplexer
/// that dropped the variable) stays on the 256-color palette.
pub(super) fn color_depth_from(override_var: Option<&str>, colorterm: Option<&str>) -> ColorDepth {
    match override_var
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("truecolor") => ColorDepth::TrueColor,
        Some("256") => ColorDepth::Indexed256,
        Some("16") => ColorDepth::Ansi16,
        _ => match colorterm.map(|c| c.to_ascii_lowercase()).as_deref() {
            Some("truecolor" | "24bit") => ColorDepth::TrueColor,
            _ => ColorDepth::Indexed256,
        },
    }
}

/// One ink in the three depths. `idx` is an entry of xterm's 256-color
/// palette (the grayscale ramp for the grays); `ansi` is the 16-color slot a
/// forced-16 terminal falls back on.
#[derive(Clone, Copy)]
struct Ink {
    rgb: (u8, u8, u8),
    idx: u8,
    ansi: Color,
}

const fn ink(rgb: (u8, u8, u8), idx: u8, ansi: Color) -> Ink {
    Ink { rgb, idx, ansi }
}

const ACCENT_DARK: Ink = ink((122, 162, 247), 110, Color::LightBlue);
const ACCENT_LIGHT: Ink = ink((37, 99, 235), 27, Color::Blue);
const DIM_DARK: Ink = ink((138, 146, 158), 245, Color::Gray);
const DIM_LIGHT: Ink = ink((100, 116, 139), 246, Color::DarkGray);
const FAINT_DARK: Ink = ink((104, 110, 124), 240, Color::DarkGray);
const FAINT_LIGHT: Ink = ink((164, 174, 192), 248, Color::DarkGray);
const WARN_DARK: Ink = ink((229, 180, 102), 179, Color::Yellow);
const WARN_LIGHT: Ink = ink((178, 106, 0), 130, Color::Yellow);
const ERROR_DARK: Ink = ink((224, 108, 117), 210, Color::LightRed);
const ERROR_LIGHT: Ink = ink((200, 50, 50), 160, Color::Red);

// ── Knobs ───────────────────────────────────────────────────────────────
// The 16 ANSI colors flip with the terminal theme and the 256-color grays are
// fixed, so neither can serve a light and a dark background at once. No
// autodetection of the background (COLORFGBG et al. are unreliable):
// `TINYAGENT_TUI_THEME=light|dark` (default dark) is the explicit knob, read
// once at TUI start. The color depth comes from `TINYAGENT_TUI_COLOR` /
// `COLORTERM` the same way.

/// Whether the light palette is active. Set once in `run` from the env;
/// tests read the unset (dark) default.
static THEME_LIGHT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
/// The bound color depth. Set once in `run`; tests read the unset
/// (256-color) default.
static COLOR_DEPTH: std::sync::OnceLock<ColorDepth> = std::sync::OnceLock::new();

/// Pure decision core of the theme knob: exactly `light` selects the light
/// palette; anything else (including unset) stays dark.
pub(super) fn theme_light_from(var: Option<&str>) -> bool {
    var.is_some_and(|v| v.eq_ignore_ascii_case("light"))
}

/// Bind the theme knob from the real environment. Called once at TUI start;
/// later calls are no-ops (OnceLock keeps the first value).
pub(super) fn set_theme_from_env(var: Option<String>) {
    let _ = THEME_LIGHT.set(theme_light_from(var.as_deref()));
}

/// Bind the depth knob from the real environment (same once-only contract).
pub(super) fn set_color_depth_from_env(override_var: Option<String>, colorterm: Option<String>) {
    let _ = COLOR_DEPTH.set(color_depth_from(
        override_var.as_deref(),
        colorterm.as_deref(),
    ));
}

fn resolve(light: bool, depth: ColorDepth, dark: Ink, light_ink: Ink) -> Color {
    let ink = if light { light_ink } else { dark };
    match depth {
        ColorDepth::TrueColor => Color::Rgb(ink.rgb.0, ink.rgb.1, ink.rgb.2),
        ColorDepth::Indexed256 => Color::Indexed(ink.idx),
        ColorDepth::Ansi16 => ink.ansi,
    }
}

/// The bound theme and depth: dark and 256-color until `run` binds them.
fn bound() -> (bool, ColorDepth) {
    (
        *THEME_LIGHT.get().unwrap_or(&false),
        *COLOR_DEPTH.get().unwrap_or(&ColorDepth::Indexed256),
    )
}

/// The one accent: role/tool glyphs, the busy frame, inline code.
pub(super) fn accent_fg() -> Color {
    let (light, depth) = bound();
    accent_fg_for(light, depth)
}

/// Secondary text: tool lines, thinking, notices, the status readout.
pub(super) fn dim_fg() -> Color {
    let (light, depth) = bound();
    dim_fg_for(light, depth)
}

/// Tertiary marks: guides, rules, key labels.
pub(super) fn faint_fg() -> Color {
    let (light, depth) = bound();
    faint_fg_for(light, depth)
}

/// States that want a glance: cancelling, a held queue, a context window
/// filling up.
pub(super) fn warn_fg() -> Color {
    let (light, depth) = bound();
    warn_fg_for(light, depth)
}

/// Failures.
pub(super) fn error_fg() -> Color {
    let (light, depth) = bound();
    error_fg_for(light, depth)
}

/// The five inks for an explicit theme and depth: the pure cores behind the
/// [`accent_fg`] family, so tests assert a color without reading (or racing)
/// process-global state.
pub(super) fn accent_fg_for(light: bool, depth: ColorDepth) -> Color {
    resolve(light, depth, ACCENT_DARK, ACCENT_LIGHT)
}

pub(super) fn dim_fg_for(light: bool, depth: ColorDepth) -> Color {
    resolve(light, depth, DIM_DARK, DIM_LIGHT)
}

pub(super) fn faint_fg_for(light: bool, depth: ColorDepth) -> Color {
    resolve(light, depth, FAINT_DARK, FAINT_LIGHT)
}

pub(super) fn warn_fg_for(light: bool, depth: ColorDepth) -> Color {
    resolve(light, depth, WARN_DARK, WARN_LIGHT)
}

pub(super) fn error_fg_for(light: bool, depth: ColorDepth) -> Color {
    resolve(light, depth, ERROR_DARK, ERROR_LIGHT)
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
/// fill level — dim while comfortable, warn past 60%, error past 80% — so a
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
        error_fg()
    } else if pct >= 60 {
        warn_fg()
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

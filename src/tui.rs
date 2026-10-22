//! TUI module root (`feature tui`). `run` stays the entry point main.rs
//! calls; the implementation is split by concern — `theme` (palette + light/
//! dark knob + format helpers), `transcript` (the conversation `Row` model,
//! history / delta / streaming-split logic), `slash` (command grammar +
//! completion popup), `render` (scrollback commit + live-zone painting +
//! status chrome), `markdown` (markdown/ANSI → styled lines), `input` (the
//! multi-line editor + history recall), and `app` (session wiring: `run`,
//! the worker thread, the channels, `ui_loop`).
mod app;
mod input;
mod markdown;
mod render;
mod slash;
#[cfg(test)]
mod tests;
mod theme;
mod transcript;

pub use app::run;
pub(crate) use transcript::transcript_rows;

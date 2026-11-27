//! TUI module root (`feature tui`). `run` stays the entry point main.rs
//! calls; the implementation is split by concern — `theme` (palette + light/
//! dark knob + format helpers), `transcript` (the conversation `Row` model,
//! history / delta / streaming-split logic), `slash` (command grammar +
//! completion popup), `render` (scrollback commit + live-zone painting +
//! status chrome), `markdown` (markdown/ANSI → styled lines), `input` (the
//! multi-line editor + history recall), and — the session itself — `app`
//! (wiring: `run` + the event loop), `terminal` (guards, tty watch, diag
//! bridge), `worker` (the agent-owning thread and its messages), `session`
//! (all loop state + queue/run transitions), `events` (worker messages →
//! state), `keys` / `commands` (keyboard and slash commands → state), `view`
//! (commit + paint).
mod app;
mod commands;
mod events;
mod input;
mod keys;
mod markdown;
mod render;
mod session;
mod slash;
mod terminal;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_loop;
mod theme;
mod transcript;
mod view;
mod worker;

pub use app::run;
pub(crate) use transcript::transcript_rows;

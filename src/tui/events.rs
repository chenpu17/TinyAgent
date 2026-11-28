//! The event reducer: worker messages → session state. [`Session::on_msg`] is
//! the single entry the loop calls; it owns the run-level outcomes (error,
//! finished, compact done, `/context`, kernel diag) and hands agent events to
//! [`Session::on_event`], which has one arm per `AgentEvent`.

use super::markdown::ansi_plain;
use super::render::{strip_controls, strip_controls_json};
use super::session::{compact_ledger, ctx_after_usage, pause_for_held, RunPhase, Session};
use super::slash::render_context_report;
use super::transcript::{
    append_delta, append_sub_delta, append_thinking_delta, arg_summary, auto_expand_result,
    code_log_row, split_streaming_row, tool_elapsed_for, Row,
};
use super::worker::UiMsg;
use crate::agent::AgentEvent;
use crate::config::Config;
use std::time::Instant;

/// Stop reasons that mean "the turn ended normally", across both provider
/// families (Anthropic: end_turn / tool_use / stop_sequence; OpenAI: stop /
/// tool_calls). Only abnormal stops (max_tokens / length / error / unknown)
/// get a System row — a routine end needs no noise. `cancelled` is here too:
/// a run stopped by Esc ends in the one `[cancelled]` notice of the completion
/// path, so a `[done: cancelled]` line would just duplicate it.
pub(super) fn is_normal_stop(reason: &str) -> bool {
    matches!(
        reason,
        "end_turn" | "tool_use" | "stop" | "tool_calls" | "stop_sequence" | "cancelled"
    )
}

/// Display name for a tool call/result. Tool names are model-controlled
/// (execute_tool_call emits whatever the model wrote), so control characters
/// are stripped once here and every consumer — the row name, the activity
/// segment, the `tool_running` key, `tool_elapsed_for`'s match — sees the
/// same sanitized string. Sanitizing only one side would shift the name's
/// display width and break the nearest-back call/result matching.
pub(super) fn tool_display_name(name: &str) -> String {
    strip_controls(name)
}

impl Session {
    /// Apply one worker message and mark the session dirty (the loop repaints
    /// once per drained batch).
    pub(super) fn on_msg(&mut self, msg: UiMsg, cfg: &Config, split_w: usize) {
        self.dirty = true;
        match msg {
            UiMsg::Event(ev) => self.on_event(ev, split_w),
            UiMsg::Error(e) => {
                self.rows.push(Row::sys_error(format!("[error] {e}")));
                self.run.activity.clear();
                self.run.tool_running.clear();
                self.run.tool_building = None;
                // End every open stream: a leftover streaming flag would
                // let the next run merge into a stale row.
                self.run.end_all_streams();
                self.run.fail();
                self.queue_paused = pause_for_held(true, self.queued.len());
                self.push_cancel_dropped();
                if !self.queued.is_empty() {
                    self.rows.push(Row::sys_warn(format!(
                        "[queue paused — {} message(s) held; /resume continues, /unqueue edits]",
                        self.queued.len()
                    )));
                }
            }
            msg @ (UiMsg::RunFinished | UiMsg::CompactDone(_)) => {
                // A finished `/compact` takes the run's completion path
                // (busy state cleared, the queue flushed) after its own
                // bookkeeping: an optional row, the summary's bill into
                // the status-bar totals, and the ctx segment dropped by a
                // real fold (see `compact_ledger`).
                let compact = match &msg {
                    UiMsg::CompactDone(report) => Some(report),
                    _ => None,
                };
                if let Some(report) = compact {
                    if let Some((level, text)) = &report.note {
                        self.rows.push(match level {
                            tiny_agent::diag::DiagLevel::Info => Row::sys_info(text.clone()),
                            tiny_agent::diag::DiagLevel::Warn => Row::sys_warn(text.clone()),
                        });
                    }
                    let (totals, ctx) = compact_ledger(
                        report,
                        [
                            self.usage.in_tokens,
                            self.usage.out_tokens,
                            self.usage.cache_create,
                            self.usage.cache_read,
                        ],
                        self.usage.ctx_tokens,
                    );
                    [
                        self.usage.in_tokens,
                        self.usage.out_tokens,
                        self.usage.cache_create,
                        self.usage.cache_read,
                    ] = totals;
                    self.usage.ctx_tokens = ctx;
                }
                self.run.end_all_streams();
                self.run.activity.clear();
                self.run.tool_running.clear();
                self.run.tool_building = None;
                // A run was cancelled iff Esc was pressed; a compact
                // only if the cancel really aborted it (`aborted`).
                let aborted = match compact {
                    Some(report) => report.aborted,
                    None => self.run.phase == RunPhase::Cancelling,
                };
                if aborted {
                    // The one notice of a stopped run — it follows the answer,
                    // together with the queue the keypress discarded (the
                    // waiting state itself was chrome only).
                    self.rows.push(Row::sys_warn("[cancelled]"));
                    self.push_cancel_dropped();
                    self.run.idle();
                } else if compact.is_some() && self.run.phase == RunPhase::Cancelling {
                    // Only reachable for a compact: an Esc raced a fold
                    // that landed anyway (or a request that concluded on
                    // its own). The outcome's row is already up — report
                    // what happened, not a cancel that did nothing. Nothing
                    // was cancelled, but the queue the Esc discarded is
                    // still gone: its notice is owed.
                    self.push_cancel_dropped();
                    self.run.idle();
                } else if self.queue_paused {
                    // A separate prompt may have completed while an older
                    // failed queue remains explicitly held.
                    self.run.idle();
                } else {
                    let flush = self.flush_next_queued();
                    self.apply_flush(flush);
                }
            }
            UiMsg::Context(bd) => {
                // Read-only report: no busy/streaming state is touched —
                // the command is refused while running anyway, so this
                // lands in an idle gap.
                self.rows.push(Row::sys_info(render_context_report(
                    &bd,
                    cfg.limits.max_context_tokens,
                )));
            }
            UiMsg::Diag(level, msg) => {
                // Kernel diag forwarded by the bridge (see UiMsg::Diag):
                // reuse the ordinary system-row mechanism — the row rides
                // the normal commit path into scrollback. The text is
                // self-describing ("[warn] MCP stdio read stopped: …"), so
                // the level only picks the color.
                self.rows.push(match level {
                    tiny_agent::diag::DiagLevel::Info => Row::sys_info(msg),
                    tiny_agent::diag::DiagLevel::Warn => Row::sys_warn(msg),
                });
            }
        }
    }

    /// Add the row for a tool call and mark the tool as running. One place
    /// for the parent's calls and for a sub-agent's: `display_name` is the
    /// row and tracker name — the sanitized tool name for the former,
    /// `[agent] tool` for the latter — so the result row and the live
    /// placeholder line up with it either way.
    fn push_tool_call(&mut self, display_name: String, input: &serde_json::Value) {
        // Strip control chars once at entry so both the collapsed summary
        // and the expanded pretty-print render clean (a fresh value: the
        // event's own is left as it came).
        let input = strip_controls_json(input);
        let collapsed = arg_summary(&input);
        self.run.tool_running.on_call(&display_name);
        self.rows.push(Row::ToolCall {
            name: display_name,
            input,
            collapsed,
            expanded: false,
            started: Instant::now(),
        });
    }

    /// Add the row for a tool result — with the call→result elapsed time —
    /// and retire the tool's running entry. `display_name` is the matching
    /// call's row name.
    fn push_tool_result(&mut self, display_name: String, is_error: bool, content: &str) {
        self.run.tool_running.on_result(&display_name);
        let elapsed = tool_elapsed_for(&self.rows, &display_name);
        // Content stays RAW: ANSI colors survive into ansi_guided_lines at
        // render (which strips non-SGR sequences itself).
        let collapsed = crate::agent::preview(&ansi_plain(content));
        let expanded = auto_expand_result(is_error, content);
        self.rows.push(Row::ToolResult {
            collapsed,
            name: display_name,
            content: content.to_owned(),
            is_error,
            expanded,
            elapsed,
        });
    }

    /// Apply one agent event. `split_w` is the body width the streaming
    /// splitter estimates its wraps at.
    pub(super) fn on_event(&mut self, ev: AgentEvent, split_w: usize) {
        self.dirty = true;
        match ev {
            AgentEvent::TurnStart(_) => {
                self.run.end_parent_streams();
                self.run.activity = "thinking".into();
                self.run.thinking = true;
                self.run.tool_running.clear();
                self.run.tool_building = None;
            }
            AgentEvent::TextDelta(t) => {
                // First visible output: the thinking placeholder is
                // superseded by real content (same below at ToolCall).
                self.run.thinking = false;
                // The answer starts: the trace row is final.
                self.run.thinking_stream = None;
                // The PARENT is streaming: no parent tool is pending
                // anymore. (Sub-agent deltas below don't clear — a
                // sibling parallel tool may legitimately still run.)
                self.run.tool_running.clear();
                self.run.tool_building = None;
                // Parent speaks: close any open sub-agent bubble AND
                // keep this delta out of it — streaming=false forces
                // append_delta to open a fresh row even though the sub
                // bubble is an Assistant row (the old code merged the
                // parent's text into the sub-agent's bubble).
                if self.run.close_sub_bubble() {
                    self.run.streaming = false;
                }
                // Guard the assignment: this fires per streamed delta,
                // so only touch the string on the thinking→writing
                // transition instead of allocating every chunk.
                if self.run.activity != "writing" {
                    self.run.activity = "writing".into();
                }
                append_delta(&mut self.rows, &mut self.run.streaming, &t);
                // Incremental commit: flush finished paragraphs into
                // scrollback while the rest keeps streaming.
                split_streaming_row(&mut self.rows, split_w);
            }
            AgentEvent::ThinkingDelta(t) => {
                // Reasoning trace: its own dim ✻ block, never merged
                // into the answer row (the old fold made the trace and
                // the reply read as one blob).
                self.run.thinking = false;
                self.run.tool_running.clear();
                self.run.tool_building = None;
                // Parent speaks: close any open sub bubble so the
                // trace doesn't merge into it (same rule as TextDelta).
                if self.run.close_sub_bubble() {
                    self.run.streaming = false;
                }
                if self.run.activity != "thinking" {
                    self.run.activity = "thinking".into();
                }
                append_thinking_delta(&mut self.rows, &mut self.run.thinking_stream, "", &t);
                // Thinking commits as one compact/folded row at close;
                // only Assistant text uses incremental row splitting.
            }
            AgentEvent::ToolInputDelta { name, .. } => {
                let name = tool_display_name(&name);
                self.run.thinking = false;
                // Argument construction follows any text/reasoning from
                // this assistant turn. Close those live rows so the new
                // render-only tool-building state is actually visible.
                self.run.end_all_streams();
                self.run.activity = format!("→ {name} · receiving args");
                if self
                    .run
                    .tool_building
                    .as_ref()
                    .is_none_or(|(n, _)| n != &name)
                {
                    self.run.tool_building = Some((name, Instant::now()));
                }
            }
            AgentEvent::ToolCall { name, input } => {
                self.run.tool_building = None;
                // A child's bubble may stay open: sibling tools can still
                // be running (same rule as the parent's Done below).
                self.run.end_parent_streams();
                self.run.thinking = false;
                // Sanitize once, before every consumer: the row, the
                // activity segment, and the tool tracker key all see
                // the same string (see tool_display_name).
                let name = tool_display_name(&name);
                self.run.activity = format!("→ {name}");
                self.push_tool_call(name, &input);
            }
            AgentEvent::ToolResult {
                name,
                is_error,
                content,
            } => {
                self.run.end_parent_streams();
                // Same sanitized key as the ToolCall arm above, so
                // the duration match and the tracker removal line up.
                self.push_tool_result(tool_display_name(&name), is_error, &content);
            }
            AgentEvent::Done { stop_reason } => {
                self.run.end_parent_streams();
                // A normal turn end is the common case — no row. Only
                // abnormal stops (max_tokens/length/error/unknown) are
                // worth a notice.
                if !is_normal_stop(&stop_reason) {
                    self.rows
                        .push(Row::sys_warn(format!("[done: {stop_reason}]")));
                }
            }
            AgentEvent::Compressed {
                dropped,
                kept,
                trigger,
                ..
            } => {
                self.run.end_parent_streams();
                let how = match trigger {
                    crate::agent::CompressTrigger::Preflight => "preflight",
                    crate::agent::CompressTrigger::Overflow => "overflow",
                    crate::agent::CompressTrigger::Manual => "manual",
                };
                self.rows.push(Row::sys_info(format!(
                    "[context compressed ({how}): dropped {dropped} messages, {kept} kept]"
                )));
            }
            AgentEvent::Pruned {
                messages,
                chars_freed,
            } => {
                self.run.end_parent_streams();
                self.rows.push(Row::sys_info(format!(
                "[context pruned: {messages} messages lost stale tool output, {chars_freed} chars freed]"
            )));
            }
            AgentEvent::Warning(w) => {
                self.rows.push(Row::sys_warn(format!("[warn] {w}")));
            }
            AgentEvent::NoteInjected { text, deferred, .. } => {
                // Supervisor note (R1): a control signal injected by
                // an external observer — warn-level visibility so it
                // stands out; /export carries it like any row. The
                // deferred tag says it missed every delivery point
                // this run and rides history into the next one.
                if deferred {
                    self.rows.push(Row::sys_warn(format!(
                        "[supervisor note, deferred to next run] {text}"
                    )));
                } else {
                    self.rows
                        .push(Row::sys_warn(format!("[supervisor note] {text}")));
                }
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                tool_folded,
                // A split of output_tokens — the status bar keeps its
                // four counters (the TUI stays minimal).
                reasoning_tokens: _,
            } => {
                self.usage.in_tokens = self.usage.in_tokens.saturating_add(input_tokens);
                self.usage.out_tokens = self.usage.out_tokens.saturating_add(output_tokens);
                self.usage.cache_create = self
                    .usage
                    .cache_create
                    .saturating_add(cache_creation_input_tokens);
                self.usage.cache_read = self
                    .usage
                    .cache_read
                    .saturating_add(cache_read_input_tokens);
                // Only a DIRECT provider request re-measures the
                // context — see ctx_after_usage: a tool-folded report
                // (a `task` child's multi-turn sum, a run_code inner
                // llm, a host tool's bill) must not SET the ctx
                // segment (it would show a bogus, possibly >100%
                // number until the next real request). Totals above
                // still count it — honest cost, skipped only here.
                self.usage.ctx_tokens = ctx_after_usage(
                    self.usage.ctx_tokens,
                    tool_folded,
                    input_tokens,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                    output_tokens,
                );
            }
            AgentEvent::TodoUpdate { todos } => {
                // Update the live plan state — the status bar renders a
                // `plan done/total` segment from it each frame (see
                // build_status_line). No transcript row: the full
                // checklist is already visible in the `todo` tool-result
                // row, and a per-update sys_info line would just clutter
                // the scrollback. Full-replacement semantics: each call
                // carries the entire plan, so a plain assign is correct.
                self.plan = todos.clone();
            }
            AgentEvent::CodeLog(line) => {
                // Sandbox console.log → committed transcript row (design
                // L6: reviewable in scrollback and `/export`-able — a
                // render-only transient would lose the execution log).
                // sys_info styling keeps it visually apart from replies;
                // the payload stays bare — the renderer and /export
                // draw their own `·` marker (see code_log_row).
                self.rows.push(code_log_row(line));
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
                // Sanitize the model-supplied name once, so every use
                // (bubble prefix, row names, activity segment) is
                // terminal-safe.
                let name = tool_display_name(&name);
                match &*event {
                    AgentEvent::TextDelta(t) => {
                        self.run.thinking = false;
                        // The child's answer starts: its trace row
                        // (or any sibling's) is final.
                        self.run.thinking_stream = None;
                        append_sub_delta(
                            &mut self.rows,
                            &mut self.run.sub_streaming_name,
                            &name,
                            t,
                        );
                    }
                    AgentEvent::ThinkingDelta(t) => {
                        // The child's trace: its own dim ✻ block with
                        // the child's name on the head line — never
                        // merged into its text bubble (same rule as
                        // the parent's trace vs the answer).
                        self.run.thinking = false;
                        // Close the child's text bubble so the answer
                        // restarts in a fresh bubble after the trace.
                        self.run.sub_streaming_name = None;
                        append_thinking_delta(
                            &mut self.rows,
                            &mut self.run.thinking_stream,
                            &name,
                            t,
                        );
                    }
                    AgentEvent::ToolCall { name: tn, input } => {
                        self.run.tool_building = None;
                        self.run.end_child_streams();
                        self.run.thinking = false;
                        // Same sanitized display name as the row
                        // below, so the placeholder and the committed
                        // head agree (and their tracker keys match).
                        let tn = tool_display_name(tn);
                        self.run.activity = format!("[{name}] → {tn}");
                        self.push_tool_call(format!("[{name}] {tn}"), input);
                    }
                    AgentEvent::ToolResult {
                        name: tn,
                        is_error,
                        content,
                    } => {
                        self.run.end_child_streams();
                        // The row name mirrors the ToolCall arm's
                        // format (with the same sanitized tn), so
                        // nearest-back matching finds it.
                        let row_name = format!("[{name}] {}", tool_display_name(tn));
                        self.push_tool_result(row_name, *is_error, content);
                    }
                    AgentEvent::Done { .. } => {
                        self.run.end_child_streams();
                    }
                    AgentEvent::Warning(w) => {
                        self.run.end_child_streams();
                        self.rows
                            .push(Row::sys_warn(format!("[{name}] [warn] {w}")));
                    }
                    _ => {} // the child's internal bookkeeping (TurnStart/Usage/Compressed/Pruned/MaxTokensSalvage/…): folded at the agent level
                }
            }
            AgentEvent::MaxTokensSalvage => {
                // The wrap-up turn after a max_tokens cutoff is billed
                // and hook-visible like any turn, but its trigger is
                // otherwise invisible here (harness writes stderr /
                // NDJSON `{"type":"salvage"}`) — warn level matches
                // the `[done: max_tokens]` notice above.
                self.rows.push(Row::sys_warn(
                    "[max_tokens hit — one wrap-up turn appended]",
                ));
            }
            AgentEvent::TurnArbitration {
                continued, note, ..
            } => {
                // 轮末仲裁（FR-1）：仅在宿主 hook 给出非默认决策时
                // 出现。continued 的投递可见性由随后的 NoteInjected
                // 行承担，这里标决策点本身；capped 的 note 不会投递
                // （只活在事件里），所以把它带上——否则这段意图在
                // 会话里就无迹可寻了。warn 级与 NoteInjected 对齐。
                // 流式卫生（round-3 评审 R1-10）：与子 agent 的 Warning 臂
                // 同款重置（顶层 Warning 臂不收流）——仲裁行之后到下轮
                // TurnStart 之间，残留的 streaming 状态会把系统行错当
                // in-flight 流式行。
                self.run.end_child_streams();
                if continued {
                    self.rows.push(Row::sys_warn(
                        "[arbitration: completion rejected — one more turn]",
                    ));
                } else {
                    self.rows.push(Row::sys_warn(format!(
                        "[arbitration capped — note not delivered] {note}"
                    )));
                }
            }
            // ToolInputDelta is streaming plumbing: the ToolCall row
            // and the live tool placeholder own its presentation, and
            // a row per fragment would flood the transcript.
            // non_exhaustive forward-compat: AgentEvent may grow new
            // variants (0.x SemVer window); they render nothing here
            // until given a dedicated arm. Compile-adaptation only —
            // no behavior change for the variants above.
            _ => {}
        }
    }
}

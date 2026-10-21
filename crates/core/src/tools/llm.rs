//! The sandbox `llm` primitive (code_llm_design §3 LD-1…LD-18, M3): a
//! guest-only, stateless single-shot model call, reached through the
//! ordinary `tools.call("llm", …)` channel so the hooks gate, the
//! ToolCall/ToolResult event pair, the audit trace and the call quota all
//! apply for free (LD-1 — the same dispatch face MCP/file tools use).
//!
//! Three properties are load-bearing and easy to break:
//! - **Single-shot** (LD-2): no tools, no history, no streaming, one round.
//!   Recursion depth is 1 by construction — the bounded cousin of the `task`
//!   tool D11 excluded as an unbounded cost bomb.
//! - **Same provider instance** (LD-16): inner calls ride the VERY provider
//!   the outer agent was constructed with, through the shared cell the
//!   `RunCodeTool` owns and `Agent::set_code_tool` late-fills. Custom
//!   providers (the language bindings) therefore see inner calls too and
//!   answer with their own honest error when they cannot serve a model.
//! - **Reserve-then-settle** (LD-17): the token budget is PRE-HELD before
//!   dispatch (estimate + output cap) and settled exactly once against the
//!   real usage — unknown usage keeps the reservation (the gate may
//!   over-hold, never under-hold), so a fleet of concurrent single shots
//!   cannot collectively overshoot between check and charge.
//!
//! Not in the top-level tool list, ever: the registry admits it only when
//! the build-time snapshot has `code.llm.enabled` (LD-14).

use crate::config::LlmConfig;
use crate::provider::{Block, ChatRequest, Message, Provider, Role, ToolDef};
use crate::tools::{Tool, ToolConcurrency, ToolCtx, ToolOrigin, ToolOutput};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

/// Per-request timeout, hardcoded (LD-10): 120s, aligned with the MCP
/// `CALL_TIMEOUT` precedent, deliberately NOT min'd against the sandbox wall
/// clock — an in-flight call crossing the deadline is the existing honest
/// boundary formula's second term (in-flight MCP calls cross it the same
/// way), and a deep-reasoning model legally takes over 60s for one shot.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Protocol slack folded into the input estimate (§3.1): the chat envelope
/// around prompt+system costs tokens the char heuristic cannot see. A
/// calibration allowance, not a bound — over-reserve is reported, never
/// promised away.
const PROTOCOL_SLACK_TOKENS: u64 = 64;

/// LD-17 v1 input estimate: ASCII chars / 4, wide (non-ASCII) chars 1:1,
/// plus the fixed protocol slack. The design doc is explicit that this is a
/// heuristic to calibrate per target model, NOT a mathematical upper bound.
fn est_in_tokens(s: &str) -> u64 {
    let ascii = s.chars().filter(|c| c.is_ascii()).count() as u64;
    let wide = (s.chars().count() as u64).saturating_sub(ascii);
    ascii / 4 + wide + PROTOCOL_SLACK_TOKENS
}

/// LD-18 minimal output normalization: trim → strip ONE wrapping pair of
/// quotes or backticks → case-fold. Deliberately minimal — it fixes
/// typography tics only; anything "smarter" (first-token extraction,
/// fuzzy match) would silently launder model drift, against the honesty
/// principle the whole feature stands on.
fn strip_wrapping(t: &str) -> &str {
    let mut chars = t.chars();
    match (chars.next(), chars.next_back()) {
        (Some(a), Some(b))
            if (a == '"' && b == '"') || (a == '\'' && b == '\'') || (a == '`' && b == '`') =>
        {
            &t[a.len_utf8()..t.len() - b.len_utf8()]
        }
        _ => t,
    }
}

fn normalize_label(s: &str) -> String {
    strip_wrapping(s.trim()).to_lowercase()
}

/// A declared label: the canonical (declared-spelling) value returned on a
/// hit, paired with its case-folded match key.
struct Label {
    canonical: String,
    key: String,
}

/// The tool. Constructed only by the guest registry build
/// (`RunCodeTool::registry`) when the lineage's snapshot has the gate on.
pub struct LlmTool {
    /// Build-time snapshot (LD-14): the registry caches its decision for the
    /// lineage's lifetime; a later hot flip of `code.llm.enabled` is not
    /// sensed (the OnceLock registry discipline).
    cfg: LlmConfig,
    /// Shared provider cell (LD-16): late-filled by `Agent::set_code_tool`
    /// with the same `Arc<dyn Provider>` the outer agent chats through. The
    /// cell itself lives on the `RunCodeTool` (clone-shared across its whole
    /// lineage — a plain field would split between clones and the fill would
    /// never reach the instance guests actually call).
    provider: Arc<OnceLock<Arc<dyn Provider>>>,
}

impl LlmTool {
    pub fn new(cfg: LlmConfig, provider_cell: Arc<OnceLock<Arc<dyn Provider>>>) -> Self {
        Self {
            cfg,
            provider: provider_cell,
        }
    }

    /// The audit row for one call (LD-12): folded by run_code's completion
    /// tails into `meta["llm"]`. `actual` is `None` when usage never
    /// arrived (missing / error / panic) — unknown ≠ zero, the reservation
    /// stands and the audit says so.
    fn audit_row(model: &str, reserved: u64, actual: Option<u64>, over_reserve: bool) -> Value {
        json!({
            "model": model,
            "reserved": reserved,
            "actual": actual,
            "known": actual.is_some(),
            "over_reserve": over_reserve,
        })
    }
}

/// The reservation-relevant parameter read — `prompt` / `system` / `model` /
/// `max_tokens` validated ONCE for both dispatch faces that need it:
/// [`Tool::run`] reports the rejection text as its param error;
/// [`Tool::panic_audit_row`] maps any rejection to `None` (the call was
/// refused BEFORE the reservation, so nothing is held). Until this function
/// existed the two were parallel copies — a one-sided edit would silently
/// desync the panic audit row from the reservation it re-derives. `Err`
/// carries run()'s exact param-error messages, byte for byte. Labels stay
/// OUT (validated in `run` only): they precede the reserve there, but a batch
/// worker only ever panics mid-dispatch — long after labels cleared — so the
/// panic row never depends on them.
struct ReserveParams<'a> {
    prompt: &'a str,
    system: Option<&'a str>,
    model: String,
    max_tokens: u32,
}

fn reserve_params<'a>(cfg: &LlmConfig, input: &'a Value) -> Result<ReserveParams<'a>, String> {
    let Some(prompt) = input.get("prompt").and_then(Value::as_str) else {
        return Err("llm: `prompt` is required and must be a string".into());
    };
    let system = match input.get("system") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err("llm: `system` must be a string".into()),
    };
    let model: String = match input.get("model") {
        None | Some(Value::Null) => match &cfg.default_model {
            // Final-model check (external audit P2-5): every trusted config
            // entrance validates this invariant now, but the registry
            // snapshots whatever cfg it was BUILT with — refuse here rather
            // than dispatch to a model outside the tier, before any
            // reservation or provider call.
            Some(m) if cfg.models.contains(m) => m.clone(),
            Some(m) => {
                return Err(format!(
                    "llm: code.llm.default_model `{m}` is not in the \
                     code.llm.models allowlist"
                ))
            }
            None => return Err("llm: no `model` given and code.llm.default_model is unset".into()),
        },
        Some(Value::String(s)) => {
            if !cfg.models.iter().any(|m| m == s) {
                return Err(format!(
                    "llm: model `{s}` is not in the code.llm.models allowlist"
                ));
            }
            s.clone()
        }
        Some(_) => return Err("llm: `model` must be a string".into()),
    };
    let max_tokens: u32 = match input.get("max_tokens") {
        None | Some(Value::Null) => 1024,
        Some(v) => match v.as_u64() {
            Some(n) if (1..=8192).contains(&n) => n as u32,
            _ => {
                return Err(
                    "llm: `max_tokens` must be an integer in 1..=8192 (default 1024)".into(),
                )
            }
        },
    };
    Ok(ReserveParams {
        prompt,
        system,
        model,
        max_tokens,
    })
}

/// LD-17 reserve estimate over the shared read: est(prompt) + est(system) +
/// max_tokens — the SAME arithmetic `run` reserves with and `panic_audit_row`
/// re-derives, kept beside [`reserve_params`] so the two faces cannot drift.
fn reserve_est(prompt: &str, system: Option<&str>, max_tokens: u32) -> u64 {
    est_in_tokens(prompt)
        .saturating_add(system.map_or(0, est_in_tokens))
        .saturating_add(max_tokens as u64)
}

/// An `llm`-class failure envelope (`kind="llm"`): a param/allowlist/provider
/// failure with no cost to report. `meta_extra` rides along for dispatched
/// failures that DO have a reservation standing.
fn err_llm(msg: String, meta_extra: Option<Value>) -> ToolOutput {
    let mut meta = json!({ "llm_err": "llm" });
    if let Some(extra) = meta_extra {
        for (k, v) in extra.as_object().into_iter().flatten() {
            meta[k.as_str()] = v.clone();
        }
    }
    ToolOutput {
        content: msg,
        image: None,
        is_error: true,
        usage: None,
        usage_in_shared_budget: false,
        meta: Some(meta),
    }
}

impl Tool for LlmTool {
    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Builtin
    }

    fn def(&self) -> ToolDef {
        // Description is design appendix A① VERBATIM (LLM-visible copy is
        // spec — change it only through the doc's revision log).
        ToolDef {
            name: "llm".into(),
            description: "调用 LLM 做单次判别/生成（无工具、无历史、无流式）。适合逐项语义判断：分类、抽取、\n校对、打标。labels 校验词表，不保证判断正确；副作用遵循宿主授权政策。返回\n{text, label?, usage, model}。".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The complete instruction for this single shot — self-contained: no tools, no history, no follow-up turns."
                    },
                    "system": {
                        "type": "string",
                        "description": "Optional system prompt for this call."
                    },
                    "model": {
                        "type": "string",
                        "description": "Model override; must be in the code.llm.models allowlist. Omit to use default_model."
                    },
                    "max_tokens": {
                        "type": "integer",
                        "description": "Output cap, 1..=8192 (default 1024)."
                    },
                    "labels": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Closed output vocabulary (non-empty, ≤16 items): the host validates the normalized output against it and returns `label` on a hit; a miss is kind=\"llm_output\". The vocabulary requirement must ALSO be stated in the prompt — the host never injects it. Undeclared = free-form generation."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn concurrency(&self) -> ToolConcurrency {
        // LD-11: the request is stateless and reentrant — this is the
        // callBatch routing promise (the FIRST Parallel batchable tool).
        // `Send + Sync` only guarantees memory safety; a custom provider
        // that serializes internally still works, at its own throughput.
        ToolConcurrency::Parallel
    }

    fn available_to_subagent(&self) -> bool {
        // Unreachable in practice: the sub-agent's tool list filters
        // `run_code` itself out, so no child ever builds a registry that
        // could hold this. False keeps the invariant explicit.
        false
    }

    fn available_to_code(&self) -> bool {
        true
    }

    fn batchable(&self) -> bool {
        // LD-19: a single shot is a pure read of a model — no side effects
        // in any input shape, the exact property the batch surface wants.
        true
    }

    fn envelope_error_kind(&self, output: &ToolOutput) -> Option<&'static str> {
        match output
            .meta
            .as_ref()
            .and_then(|m| m.get("llm_err"))
            .and_then(Value::as_str)?
        {
            "llm" => Some("llm"),
            "llm_output" => Some("llm_output"),
            "budget" => Some("budget"),
            _ => None,
        }
    }

    /// External audit P2-7: the batch panic placeholder's audit row. A
    /// worker running THIS tool that died mid-call held an LD-17 reservation
    /// its guard's Drop kept — this re-derives (model, est) from the
    /// EFFECTIVE input the worker was dispatched with, through the SAME
    /// shared read [`Tool::run`] validates with ([`reserve_params`] /
    /// [`reserve_est`]) — one code path, so `reserved` matches what the
    /// dropped guard holds by construction, not by parallel copy. `None`
    /// when the entry never reached the reservation (invalid params — run
    /// refuses before reserving) or no gate is installed (nothing was held).
    fn panic_audit_row(&self, input: &Value, ctx: &ToolCtx) -> Option<Value> {
        match ctx.budget.as_ref() {
            Some(g) if g.cap > 0 => {}
            // run() reserves only under an installed gate — nothing is held.
            _ => return None,
        }
        // Any rejection here is a param error run() refused BEFORE
        // reserving — no row.
        let p = reserve_params(&self.cfg, input).ok()?;
        Some(LlmTool::audit_row(
            &p.model,
            reserve_est(p.prompt, p.system, p.max_tokens),
            None,
            false,
        ))
    }

    fn run(&self, input: &Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        // ── params — validated on the EFFECTIVE input (§3.1: hooks Modify
        // landed before us; a modified model/max_tokens is re-checked against
        // the allowlist/range here, reservation below uses the modified
        // request too). The read itself is [`reserve_params`] — ONE shared
        // validation `panic_audit_row` re-derives its audit row from, so the
        // two faces cannot drift apart.
        let ReserveParams {
            prompt,
            system,
            model,
            max_tokens,
        } = match reserve_params(&self.cfg, input) {
            Ok(p) => p,
            // Param/allowlist failure: no reservation, no dispatch.
            Err(msg) => return Ok(err_llm(msg, None)),
        };
        // LD-18 labels: non-empty array, ≤16 non-empty strings; the
        // DECLARATION goes through the same normalization as outputs and is
        // checked for collisions — ["Yes","yes"] overlaps post-fold and is a
        // param error, not an undecidable match at validation time.
        let labels: Option<Vec<Label>> = match input.get("labels") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => {
                if items.is_empty() || items.len() > 16 {
                    return Ok(err_llm(
                        "llm: `labels` must be a non-empty array of at most 16 strings".into(),
                        None,
                    ));
                }
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    let Some(s) = it.as_str() else {
                        return Ok(err_llm(
                            "llm: `labels` entries must be strings".into(),
                            None,
                        ));
                    };
                    if s.trim().is_empty() {
                        return Ok(err_llm(
                            "llm: `labels` entries must be non-empty".into(),
                            None,
                        ));
                    }
                    let key = normalize_label(s);
                    if out.iter().any(|l: &Label| l.key == key) {
                        return Ok(err_llm(
                            format!("llm: `labels` collide after normalization (case-fold): {s:?}"),
                            None,
                        ));
                    }
                    out.push(Label {
                        canonical: s.to_string(),
                        key,
                    });
                }
                Some(out)
            }
            Some(_) => {
                return Ok(err_llm(
                    "llm: `labels` must be an array of strings".into(),
                    None,
                ))
            }
        };

        // ── LD-17 reserve: est_in(prompt + system) + max_tokens — the SAME
        // shared arithmetic `panic_audit_row` re-derives. The gate is only
        // installed with a cap > 0; absent → skip the reserve (the
        // usage/unknown audit below still runs — §3.1 cap=0 row).
        let est = reserve_est(prompt, system, max_tokens);
        let mut guard = match ctx.budget.as_ref().filter(|g| g.cap > 0) {
            Some(g) => match g.reserve(est) {
                Some(guard) => Some(guard),
                // Soft budget reject (LD-9): NO halted latch — the
                // reservation was never held, the sandbox stays alive, and
                // headroom freed by other calls' settlements can re-open
                // dispatch. The single-call envelope carries no `halted`
                // field (design revision ⑪) — the message text below IS the
                // recovery contract.
                None => {
                    return Ok(ToolOutput {
                        content: format!(
                            "llm: token budget exceeded — this call was NOT dispatched (no \
                             reservation held; the sandbox stays alive: settle in-flight work, \
                             write_file partial results, or return); needed ≈{est} tokens, {} left",
                            g.tokens_left()
                        ),
                        image: None,
                        is_error: true,
                        usage: None,
                        usage_in_shared_budget: false,
                        meta: Some(json!({ "llm_err": "budget" })),
                    })
                }
            },
            None => None,
        };

        // ── provider from the shared cell (LD-16). Unfilled = the lineage
        // never passed through an Agent (only the two build_agent* fill
        // points exist) — an honest pre-dispatch error, reservation fully
        // refunded (§3.1 未派发全退).
        let Some(provider) = self.provider.get() else {
            if let Some(g) = guard.take() {
                let _ = g.release();
            }
            return Ok(err_llm(
                "llm: provider cell not filled — this run_code lineage was never installed \
                 into an agent, so there is no provider to serve inner model calls"
                    .into(),
                None,
            ));
        };

        // ── dispatch (LD-2/LD-4): one round, no tools, no event streaming,
        // temperature pinned at the provider layer (the only in-tree
        // setter), 120s per-request backstop (LD-10).
        let messages = [Message {
            // Not entering any history — no id allocation point applies.
            id: None,
            role: Role::User,
            content: vec![Block::Text(prompt.to_string())],
        }];
        let cancel = ctx
            .cancel
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let req = ChatRequest {
            system,
            messages: &messages,
            tools: &[],
            model: &model,
            max_tokens,
            per_request_timeout: Some(CALL_TIMEOUT),
            cancel,
            // One-shot, never re-sent: skip the cache-write premium.
            cache: false,
            reasoning_effort: None,
            temperature: Some(0.0),
        };
        let resp = match provider.chat(&req, &mut |_| {}) {
            Ok(r) => r,
            Err(e) => {
                // Dispatched and failed with no usage: the reservation
                // STANDS (drop without settle — the gate may over-hold, it
                // must never under-hold; LD-17 matrix row 2).
                drop(guard.take());
                return Ok(err_llm(
                    format!("llm: model call failed: {e:#}"),
                    Some(json!({ "llm_call": LlmTool::audit_row(&model, est, None, false) })),
                ));
            }
        };

        // ── settle exactly once (§3.1 matrix). Labels validation happens
        // AFTER: a failed validation still settles actual — the call
        // happened, its cost is real.
        let total = resp.usage.map(|u| u.total());
        let (reserved, actual, over_reserve) = match (guard.take(), total) {
            (Some(g), Some(t)) => {
                let s = g.settle(t);
                (s.reserved, Some(s.actual), s.over_reserve)
            }
            // Unknown usage: keep the reservation, report actual:null.
            (Some(g), None) => {
                drop(g);
                (est, None, false)
            }
            // No gate installed (cap = 0): skip the reserve per §3.1, but
            // the known-usage audit still records the real number.
            (None, Some(t)) => (0, Some(t), false),
            (None, None) => (0, None, false),
        };
        let row = LlmTool::audit_row(&model, reserved, actual, over_reserve);

        // ── response text + termination check + LD-18 label validation.
        let mut text: String = resp
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        // Transport Ok ≠ response complete (external audit P1-1). This is a
        // COMPLETE-RESPONSE WHITELIST, not an abort blacklist (external
        // re-review P1): providers pass unknown finish_reasons through raw
        // (openai maps stop/tool_calls/length and forwards anything else
        // verbatim; anthropic forwards its native vocabulary — content_filter,
        // refusal, pause_turn … — unchanged), so a blacklist can never
        // enumerate the failing side. Only three reasons PROVE a complete
        // response: end_turn (canonical clean stop), tool_use (the turn ended
        // by a tool call — tolerated here even though this tool sends
        // tools=[]), stop_sequence (the host-configured clean stop — the
        // output is complete by construction). Everything else — the abort
        // vocabulary (cancelled/stream_idle/stream_truncated/stream_error),
        // max_tokens (BY CONTRACT the caller's cap cut the generation), any
        // filtered/refused/paused/unknown future reason, even "" — fails
        // CLOSED: an unrecognized termination cannot be proven complete, and
        // a truncated "yes" passing label validation is exactly the honesty
        // hole. Custom-provider hosts are unaffected: SimpleChatResponse's
        // conversion already defaults an omitted stop_reason to end_turn.
        // The check runs AFTER settlement (the call's cost is real) and
        // BEFORE label validation (termination, not vocabulary, is the
        // failure being reported).
        if !matches!(
            resp.stop_reason.as_str(),
            "end_turn" | "tool_use" | "stop_sequence"
        ) {
            let mut msg = if resp.stop_reason == "max_tokens" {
                format!(
                    "llm: output hit the max_tokens cap (stop_reason=max_tokens) — the \
                     text is cut mid-generation and was not returned as a result; raise \
                     max_tokens and re-ask if it was needed; partial text: {text}"
                )
            } else {
                format!(
                    "llm: response ended abnormally (stop_reason={}) — the output is \
                     incomplete and was not returned as a result; partial text: {text}",
                    resp.stop_reason
                )
            };
            crate::tools::truncate(&mut msg, ctx.max_output_chars);
            return Ok(ToolOutput {
                content: msg,
                image: None,
                is_error: true,
                // The partial generation's cost is real: fold exactly like
                // the llm_output-miss path (self-settled above — the settle
                // already adjusted the counter; totals merge without
                // re-sharing).
                usage: resp.usage,
                usage_in_shared_budget: true,
                meta: Some(json!({
                    "llm_err": "llm",
                    "llm_call": row,
                })),
            });
        }
        let label = match &labels {
            None => None,
            Some(ls) => {
                let norm = normalize_output(&text);
                match ls.iter().find(|l| l.key == norm) {
                    Some(l) => Some(l.canonical.clone()),
                    // Miss / empty / multi-word: kind="llm_output", not
                    // retryable, guest decides the fallback (cheaper-model
                    // redo is a NEW call it may choose to make; the host
                    // never retries semantically — LD-7). Usage still
                    // reports and folds: 非法输出仍有成本.
                    None => {
                        let mut msg = format!(
                            "llm: output did not match any declared label (kind=llm_output) — \
                             got: {text}"
                        );
                        // Model-visible error text is size-bounded like any
                        // other tool error (the raw text can be a full
                        // max_tokens generation).
                        crate::tools::truncate(&mut msg, ctx.max_output_chars);
                        return Ok(ToolOutput {
                            content: msg,
                            image: None,
                            is_error: true,
                            usage: resp.usage,
                            usage_in_shared_budget: true,
                            meta: Some(json!({
                                "llm_err": "llm_output",
                                "llm_call": row,
                            })),
                        });
                    }
                }
            }
        };

        // ── LD-6 success envelope: {text, label?, usage, model}. usage is
        // the provider's actual object or null — never a fabricated
        // estimate dressed as measured.
        // The text is size-bounded HERE, after label validation (which must
        // see what the model actually said, not a cut of it) and exactly like
        // the two error envelopes above: max_tokens only binds providers that
        // honor it, and a custom/mock provider (the language-binding hosts)
        // can legally return an arbitrarily large text that would otherwise
        // flow raw into the guest result and the audit trace. The non-text
        // fields (usage/model/label) are tiny metadata and stay verbatim.
        crate::tools::truncate(&mut text, ctx.max_output_chars);
        let mut payload = json!({
            "text": text,
            "usage": resp.usage,
            "model": model,
        });
        if let Some(l) = label {
            payload["label"] = json!(l);
        }
        Ok(ToolOutput {
            content: payload.to_string(),
            image: None,
            is_error: false,
            // Self-settled (or no gate exists at all): run_code's fold must
            // merge the totals WITHOUT re-sharing the counter half — the
            // anti-double-count rule of `usage_in_shared_budget`.
            usage: resp.usage,
            usage_in_shared_budget: true,
            meta: Some(json!({ "llm_call": row })),
        })
    }
}

/// Output-side normalization (LD-18): trim → strip wrapping quotes/backticks
/// → case-fold. Same shape as the declaration's [`normalize_label`].
fn normalize_output(s: &str) -> String {
    strip_wrapping(s.trim()).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::mock::{mk_usage, resp_text, MockProvider};

    fn tool_with_scripted_provider(turns: Vec<crate::provider::mock::MockTurn>) -> LlmTool {
        let (prov, _state) = MockProvider::new(turns);
        let cell: Arc<OnceLock<Arc<dyn Provider>>> = Arc::new(OnceLock::new());
        let _ = cell.set(Arc::new(prov) as Arc<dyn Provider>);
        LlmTool::new(
            crate::config::LlmConfig {
                enabled: true,
                default_model: Some("m1".into()),
                models: vec!["m1".into()],
            },
            cell,
        )
    }

    fn ctx_with_cap(max_output_chars: usize) -> ToolCtx {
        ToolCtx {
            tool_absent_notes: Vec::new(),
            workdir: std::env::temp_dir(),
            max_output_chars,
            restrict_to_workdir: false,
            cancel: None,
            shell_timeout_secs: 120,
            hooks: None,
            subagent: None,
            budget: None,
            deadline: None,
        }
    }

    #[test]
    fn success_text_is_truncated_to_max_output_chars_like_the_error_paths() {
        // The success envelope's text must pass the SAME max_output_chars
        // bound the two error envelopes already apply: max_tokens only binds
        // providers that honor it, and a custom/mock provider can return an
        // arbitrarily large text that would otherwise flow raw into the guest
        // result and the audit trace. Before the fix, payload.to_string()
        // shipped the whole thing.
        let big = "x".repeat(5000);
        let tool = tool_with_scripted_provider(vec![resp_text(&big, mk_usage(10, 5))]);
        let out = tool
            .run(&json!({"prompt": "hello"}), &ctx_with_cap(100))
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // The envelope is JSON; the TEXT field carries the truncation marker
        // and the whole serialized payload stays small (usage/model are tiny).
        let v: Value = serde_json::from_str(&out.content).unwrap();
        let text = v["text"].as_str().unwrap();
        assert!(
            text.contains("[truncated, limit 100 chars]"),
            "marker must ride the text field: {text}"
        );
        assert!(
            text.chars().count() < 200,
            "text bounded near the cap, got {} chars",
            text.chars().count()
        );
        assert!(out.content.chars().count() < 400, "whole envelope bounded");
        // Non-text metadata rides verbatim — truncation touches only text.
        assert_eq!(v["model"], json!("m1"));
        assert_eq!(v["usage"]["input_tokens"], json!(10));
        assert_eq!(v["usage"]["output_tokens"], json!(5));
    }

    #[test]
    fn success_text_under_the_cap_roundtrips_verbatim_with_label() {
        // The bound is a ceiling, not a rewrite: a short output passes through
        // untouched (no marker injected), the label hit survives, and the
        // label match itself is computed on the model's full answer —
        // truncation is presentation, validation is semantics.
        let tool = tool_with_scripted_provider(vec![resp_text("yes", mk_usage(3, 2))]);
        let out = tool
            .run(
                &json!({"prompt": "is it?", "labels": ["yes", "no"]}),
                &ctx_with_cap(100),
            )
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["text"], json!("yes"));
        assert_eq!(v["label"], json!("yes"));
        assert!(!out.content.contains("[truncated"));
    }
}

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

    fn run(&self, input: &Value, ctx: &ToolCtx) -> anyhow::Result<ToolOutput> {
        // ── params — validated on the EFFECTIVE input (§3.1: hooks Modify
        // landed before us; a modified model/max_tokens is re-checked
        // against the allowlist/range here, reservation below uses the
        // modified request too) ──────────────────────────────────────────
        let Some(prompt) = input.get("prompt").and_then(Value::as_str) else {
            return Ok(err_llm(
                "llm: `prompt` is required and must be a string".into(),
                None,
            ));
        };
        let system = match input.get("system") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => {
                return Ok(err_llm("llm: `system` must be a string".into(), None));
            }
        };
        let model: String = match input.get("model") {
            None | Some(Value::Null) => match &self.cfg.default_model {
                // Config validation guarantees default_model ∈ models when
                // enabled — no re-check needed on this arm.
                Some(m) => m.clone(),
                None => {
                    return Ok(err_llm(
                        "llm: no `model` given and code.llm.default_model is unset".into(),
                        None,
                    ))
                }
            },
            Some(Value::String(s)) => {
                if !self.cfg.models.iter().any(|m| m == s) {
                    return Ok(err_llm(
                        format!("llm: model `{s}` is not in the code.llm.models allowlist"),
                        None,
                    ));
                }
                s.clone()
            }
            Some(_) => return Ok(err_llm("llm: `model` must be a string".into(), None)),
        };
        let max_tokens: u32 = match input.get("max_tokens") {
            None | Some(Value::Null) => 1024,
            Some(v) => match v.as_u64() {
                Some(n) if (1..=8192).contains(&n) => n as u32,
                _ => {
                    return Ok(err_llm(
                        "llm: `max_tokens` must be an integer in 1..=8192 (default 1024)".into(),
                        None,
                    ))
                }
            },
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

        // ── LD-17 reserve: est_in(prompt + system) + max_tokens. The gate
        // is only installed with a cap > 0; absent → skip the reserve (the
        // usage/unknown audit below still runs — §3.1 cap=0 row).
        let est = est_in_tokens(prompt)
            .saturating_add(system.map_or(0, est_in_tokens))
            .saturating_add(max_tokens as u64);
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

        // ── response text + LD-18 label validation.
        let text: String = resp
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
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

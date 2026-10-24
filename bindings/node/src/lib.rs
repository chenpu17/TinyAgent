//! Node.js / TypeScript bindings for TinyAgent — `import { chat, Agent } from 'tiny-agent'`.
//!
//! Exposes the agent loop, streaming events, cancel, and honest token usage to
//! JavaScript. The blocking `run()` runs on a dedicated `std::thread` per call
//! (no tokio — the core stays synchronous, like the Python binding's
//! `py.allow_threads`) and settles the returned Promise via a napi deferred;
//! events cross back to the JS main thread via a `ThreadsafeFunction`. A
//! dedicated thread — NOT napi's `AsyncTask` on the shared libuv pool: a run
//! blocks that thread waiting on JS tool/provider round-trips, and the wrapped
//! JS callbacks schedule their own async work (fs, timers, Promises) onto the
//! same pool — with as many concurrent runs as the pool has threads (default
//! 4) every worker is blocked waiting on callbacks whose own pool work can
//! never be scheduled: a deadlock. Dedicated threads scale independently of
//! `UV_THREADPOOL_SIZE` (external review finding).
//!
//! serde ↔ JS goes through `serde_json::Value` (napi's `serde-json` feature
//! gives it `ToNapiValue`): outcomes, events, and usage are converted to a
//! `Value` on the Rust side and materialized as a plain JS object by napi, so
//! the JS-visible shape matches the Rust struct (snake_case field names,
//! identical to the Python binding's dicts).
//!
//! `ta` is the Rust core library `tiny_agent` (package `tiny-agent`, aliased
//! here because this crate's own lib name — the npm import — is *also*
//! `tiny-agent`).
//!
//! Constructor: `new Agent(cfg, ...)` synchronously assembles tools —
//! including spawning and handshaking every configured MCP server — on the JS
//! main thread, so a slow MCP server freezes the event loop for seconds.
//! `Agent.create(cfg, ...)` is the non-blocking variant: same result, but the
//! config load + assembly run on a dedicated worker thread (it settles a
//! Promise). The class instance itself is still materialized in the deferred's
//! resolver on the JS thread — napi's class registry is thread-local — which
//! is also why the JS wrapper re-points its prototype. Scope note: the
//! `setModel` family and MCP hot add/remove ARE exposed (`setModel` /
//! `setReasoningEffort` / `setMaxTokens` / `setMaxContextTokens` /
//! `addMcpServer` / `removeMcpServer`); like every sync method they follow the
//! busy contract above and, by the kernel's scope decision, do NOT reach
//! `task` sub-agents constructed before the swap. The hooks interception
//! layer IS exposed (`setHooks` / `hooks` option): `beforeTool` round-trips
//! like a tool call (blocking decision), the observe-only hooks are
//! fire-and-forget TSFNs.

// napi's `#[napi]` attribute macro can trip clippy's `useless_conversion` on
// `Result` returns the same way pyo3's macros do. Scoped to this pure-binding
// file so a genuine useless conversion elsewhere still trips.
#![allow(clippy::useless_conversion)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{JsDeferred, JsObject, NapiRaw};
use napi_derive::napi;
use serde_json::Value;

use ta_core as ta;

// ── error helper ─────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
}

/// Error for the runCode methods when this module was built without its
/// `code` cargo feature (design §8.6: off by default so the standard npm
/// artifact stays ~10MB lighter) — a clear rebuild hint instead of a silent
/// no-op.
#[cfg(not(feature = "code"))]
fn code_feature_missing() -> Error {
    err(
        "this tiny-agent build lacks the `code` feature — rebuild with \
         `npm run build -- --features code` to use enableCode()/\
         disableCode()/setCodeConfig()",
    )
}

// ── deferred-Promise resolution wrapper ──────────────────────────────────────

/// Carries a serde `Value` through napi's deferred-Promise resolution path.
/// The deferred's resolver closure returns `Env -> Result<Data>` where `Data:
/// ToNapiValue + TypeName`; `serde_json::Value` has the former (via the
/// `serde-json` feature) but not the latter, so this thin wrapper supplies
/// `TypeName` and delegates the JS materialization unchanged. The JS side
/// sees the raw outcome object — no extra nesting.
pub struct Outcome(pub Value);

impl TypeName for Outcome {
    fn type_name() -> &'static str {
        "RunOutcome"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for Outcome {
    unsafe fn to_napi_value(env: napi::sys::napi_env, val: Self) -> Result<napi::sys::napi_value> {
        <Value as ToNapiValue>::to_napi_value(env, val.0)
    }
}

// ── custom JS tools: TSFN round-trip ────────────────────────────────────────
//
// A JS tool's `run(args)` callback can't be called synchronously from the
// agent's worker thread (JS functions are bound to the main thread), and napi's
// `ThreadsafeFunction` is fire-and-forget (its return value is lost). So a tool
// call round-trips: the worker thread sends `{ id, args }` to the JS `run`
// (via a TSFN), then blocks on a oneshot channel; the JS side (wrapped by
// `wrapper.js`) runs the user's function and calls back the exported
// `resolveTool(id, result)`, which looks up the id and fulfills the channel.
// No tokio, no core change — reuses the lib's `CallbackTool` (same path the
// Python binding takes).

/// Backstop timeout for a JS callback round-trip (tool / custom-provider): if
/// the host function hasn't resolved within this many seconds, the agent worker
/// gives up so it can't hang forever (the JS main thread must stay free during
/// run() for the TSFN callback to dispatch — a stuck callback otherwise deadlocks
/// the worker). CLAUDE.md documents this as a deliberate backstop, not a knob;
/// the value is centralized here so the timeout and its error message agree.
const JS_CALLBACK_TIMEOUT_SECS: u64 = 120;

/// Monotonic id tagging each in-flight tool call so `resolveTool` can find the
/// waiting channel. `i64` (not u64) because napi only reads JS numbers as i64.
static NEXT_TOOL_ID: AtomicI64 = AtomicI64::new(1);

/// id → fulfillment sender for each tool call awaiting a JS result. Cleared by
/// `resolveTool` (success) or by the closure's timeout/error path.
static PENDING_TOOLS: OnceLock<Mutex<HashMap<i64, mpsc::Sender<ToolResult>>>> = OnceLock::new();

fn pending_tools() -> &'static Mutex<HashMap<i64, mpsc::Sender<ToolResult>>> {
    PENDING_TOOLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Payload handed to the JS `run` callback: the call's id (to report back via
/// `resolveTool`) and the model's parsed arguments as a plain object.
#[napi(object)]
pub struct ToolRequest {
    pub id: i64,
    pub args: Value,
}

/// What `resolveTool` receives from JS. `content` becomes the tool's textual
/// result; `is_error` marks a failed tool.
#[napi(object)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Wait ceiling for a JS round-trip: the deadline's remaining time, capped by
/// the 120s backstop; no deadline = the backstop alone. An ALREADY-EXPIRED
/// deadline yields ZERO (fail fast) — `checked_duration_since` returns None
/// for it, which the old code read as "no deadline" and re-granted a fresh
/// 120s window to every callback dispatched after the budget ran out
/// (external review P2-4).
fn callback_wait_ceiling(deadline: Option<Instant>) -> Duration {
    match deadline.and_then(|d| d.checked_duration_since(Instant::now())) {
        Some(rem) => rem.min(Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS)),
        // Some(deadline) whose remainder computed None = in the past.
        None if deadline.is_some() => Duration::ZERO,
        None => Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS),
    }
}

/// Slice granularity for the interruptible callback wait: bounds how long
/// after a cancel flip the blocked round-trip actually gives up. Mirrors the
/// core's CancellableSse POLL_SLICE.
const CALLBACK_POLL_SLICE: Duration = Duration::from_millis(200);

/// Blocking wait for the JS side to resolve `rx`, bounded by `wait` overall
/// but polled in [`CALLBACK_POLL_SLICE`] slices so a cancel flip is honored
/// within ~one slice instead of blocking out the whole (possibly 120s)
/// ceiling — the ToolConcurrency contract expects tools to observe
/// `ctx.cancel` mid-call.
fn recv_interruptible<T>(
    rx: &mpsc::Receiver<T>,
    wait: Duration,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> std::result::Result<T, ()> {
    let end = Instant::now() + wait;
    loop {
        if let Some(c) = cancel {
            if c.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(());
            }
        }
        let now = Instant::now();
        if now >= end {
            return Err(());
        }
        match rx.recv_timeout((end - now).min(CALLBACK_POLL_SLICE)) {
            Ok(r) => return Ok(r),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            // Disconnected without a send: no resolver is ever coming.
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(()),
        }
    }
}

/// Convert a `pushUser` argument into core blocks. A string is one text block
/// (the original shorthand); an object is one block; an array is one block per
/// element, order preserved (a bare string inside the array is a text block).
///
/// Two object shapes are accepted: the Anthropic-style tagged form
/// `{type: "text"|"image"|"document", …}` and the externally-tagged serde form
/// `messages()` actually reports — `{text: "…"}`, `{image: {media_type, data}}`
/// — so a transcript block round-trips through `pushUser(messages()[i].content[j])`.
///
/// Only the model-facing variants are accepted: `tool_use` / `tool_result` are
/// agent-loop internals whose ids must pair with the model's own output, and a
/// host injecting them would forge that pairing.
fn push_user_blocks(v: &Value) -> Result<Vec<ta::Block>> {
    /// One block object → core block.
    fn one(v: &Value) -> Result<ta::Block> {
        // Normalize the messages() serde form onto the tagged form so the
        // parser below is the single source of shape truth.
        let normalized;
        let v = if v.get("type").is_some() {
            v
        } else {
            normalized = serde_block_to_tagged(v)?;
            &normalized
        };
        let tag = v
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| err("content block needs a string \"type\" field"))?;
        reject_mixed_shape(v, tag)?;
        let s = |key: &str| -> Result<String> {
            v.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| err(format!("{tag} block needs a string \"{key}\" field")))
        };
        match tag {
            "text" => Ok(ta::Block::Text(s("text")?)),
            "image" => Ok(ta::Block::Image {
                media_type: s("media_type")?,
                data: s("data")?,
            }),
            "document" => Ok(ta::Block::Document {
                media_type: s("media_type")?,
                data: s("data")?,
            }),
            "tool_use" | "tool_result" => Err(internal_block_error(tag)),
            other => Err(err(format!(
                "unknown content block type {other:?} (expected \"text\", \"image\", or \"document\")"
            ))),
        }
    }

    match v {
        Value::String(s) => Ok(vec![ta::Block::Text(s.clone())]),
        Value::Object(_) => Ok(vec![one(v)?]),
        Value::Array(items) => {
            if items.is_empty() {
                return Err(err(
                    "pushUser needs at least one content block (got an empty array)",
                ));
            }
            items
                .iter()
                .map(|item| match item {
                    Value::String(s) => Ok(ta::Block::Text(s.clone())),
                    _ => one(item),
                })
                .collect()
        }
        _ => Err(err(
            "pushUser expects a string, a content-block object, or an array of those",
        )),
    }
}

/// The rejection for `tool_use` / `tool_result`, shared by both input shapes.
fn internal_block_error(tag: &str) -> Error {
    err(format!(
        "content block type {tag:?} is agent-loop internal (its ids must pair with \
         the model's own output) and cannot be pushed by the host"
    ))
}

/// Reject a tagged block that ALSO carries a different externally-tagged shape
/// key (`{type: "text", image: {...}}`): the tagged branch would silently drop
/// the second block. The same-name pair is the normal tagged form
/// (`{type: "text", text: "…"}`), so only keys other than `tag` count.
fn reject_mixed_shape(v: &Value, tag: &str) -> Result<()> {
    for key in ["text", "image", "document", "tool_use", "tool_result"] {
        if key != tag && v.get(key).is_some() {
            return Err(err(format!(
                "content block has type {tag:?} but also carries the {key:?} shape key; \
                 send exactly one block shape per object"
            )));
        }
    }
    Ok(())
}

/// Normalize the serde shape `messages()` emits (externally tagged:
/// `{text: "…"}`, `{image: {media_type, data}}`, …) onto the tagged form
/// `one()` parses. `tool_use` / `tool_result` are recognized only so they get
/// the precise rejection rather than a generic shape error. A block carrying
/// MORE than one shape key is rejected — silently keeping the first would drop
/// the rest of the host's content.
fn serde_block_to_tagged(v: &Value) -> Result<Value> {
    let obj = v
        .as_object()
        .ok_or_else(|| err("content block must be an object"))?;
    let present: Vec<&str> = ["text", "image", "document", "tool_use", "tool_result"]
        .into_iter()
        .filter(|key| obj.contains_key(*key))
        .collect();
    if present.len() > 1 {
        return Err(err(format!(
            "content block mixes multiple message shapes ({}); send exactly one shape per object",
            present.join(", ")
        )));
    }
    match present.first().copied() {
        Some("text") => match obj.get("text") {
            Some(Value::String(s)) => Ok(serde_json::json!({ "type": "text", "text": s.clone() })),
            _ => Err(err("text block needs a string \"text\" field")),
        },
        Some(tag @ ("image" | "document")) => {
            let fields = obj
                .get(tag)
                .and_then(Value::as_object)
                .ok_or_else(|| err(format!("{tag} block needs an object \"{tag}\" field")))?;
            let mut m = fields.clone();
            m.insert("type".into(), Value::String(tag.into()));
            Ok(Value::Object(m))
        }
        Some(tag @ ("tool_use" | "tool_result")) => Err(internal_block_error(tag)),
        _ => Err(err(
            "content block needs a string \"type\" field (\"text\" / \"image\" / \
             \"document\") or the messages() shape, e.g. {text: \"hello\"}",
        )),
    }
}

/// Turn each JS tool `{ name, description, schema, run }` into a `CallbackTool`
/// whose closure drives the TSFN round-trip. Called on the JS thread (the
/// `Function`s are taken from JS and converted to `ThreadsafeFunction`s here,
/// before the agent's worker thread ever runs).
fn build_extra_tools(env: Env, tools: Option<Vec<Object>>) -> Result<Vec<Box<dyn ta::Tool>>> {
    let mut out: Vec<Box<dyn ta::Tool>> = Vec::new();
    let Some(tools) = tools else { return Ok(out) };
    for t in tools {
        let name: String = t
            .get("name")?
            .ok_or_else(|| err("custom tool missing \"name\""))?;
        let description: String = t
            .get("description")?
            .ok_or_else(|| err("custom tool missing \"description\""))?;
        let schema: Value = t
            .get("schema")?
            .ok_or_else(|| err("custom tool missing \"schema\""))?;
        let run: JsFunction = t
            .get("run")?
            .ok_or_else(|| err("custom tool missing \"run\""))?;

        // The TSFN hands the JS callback a single ToolRequest (CalleeHandled:
        // env.create_threadsafe_function returns this strategy, so JS receives
        // `(err, value)` — wrapper.js reads `value` and ignores `err`, which is
        // always null since we only ever `call(Ok(req))`). The user's `run(args)`
        // is wrapped (in wrapper.js) to read `req.args`, run, then call
        // resolveTool(req.id, result) — the closure below blocks until then.
        let tsfn = env.create_threadsafe_function(&run, 0, |ctx| Ok(vec![ctx.value]))?;

        let def = ta::ToolDef {
            name: name.clone(),
            description,
            schema,
        };
        let tool = ta::CallbackTool::new(def, move |input: &Value, ctx: &ta::ToolCtx| {
            let id = NEXT_TOOL_ID.fetch_add(1, Ordering::Relaxed);
            // Check BEFORE dispatching: a run deadline that already expired
            // must fail the call outright — dispatching anyway would hand JS a
            // request whose result can only be dropped (the wait is ZERO).
            if ctx.deadline.is_some_and(|d| d <= Instant::now()) {
                return Err(anyhow::anyhow!(
                    "tool \"{name}\": run deadline already exhausted"
                ));
            }
            let (tx, rx) = mpsc::channel();
            pending_tools().lock().unwrap().insert(id, tx);

            let req = ToolRequest {
                id,
                args: input.clone(),
            };
            if tsfn.call(Ok(req), ThreadsafeFunctionCallMode::NonBlocking) != Status::Ok {
                pending_tools().lock().unwrap().remove(&id);
                return Err(anyhow::anyhow!("tool \"{name}\": dispatch to JS failed"));
            }

            // Block the agent worker thread for the JS result. The JS main
            // thread is free (the run() caller awaits it), so the wrapped run
            // resolves the channel promptly. When the run set a wall-clock
            // budget (limits.timeout_secs), the tool ctx carries its deadline —
            // the wait is then the REMAINDER (capped by the backstop, ZERO and
            // failing fast once it expires), so a never-returning JS tool can't
            // outlive the run budget. Same shape as the custom-provider wait
            // below.
            let wait = callback_wait_ceiling(ctx.deadline);
            // Round UP for the message: a sub-second budget remainder would
            // otherwise read as "within 0s".
            let wait_secs = wait.as_secs() + u64::from(wait.subsec_millis() > 0);
            match recv_interruptible(&rx, wait, ctx.cancel.as_deref()) {
                Ok(r) => Ok(ta::ToolOutput {
                    content: r.content,
                    is_error: r.is_error,
                    image: None,
                    usage: None,
                    usage_in_shared_budget: false,
                    meta: None,
                }),
                Err(_) => {
                    pending_tools().lock().unwrap().remove(&id);
                    let why = if ctx
                        .cancel
                        .as_ref()
                        .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                    {
                        "cancelled".to_string()
                    } else {
                        format!("JS callback did not resolve within {wait_secs}s")
                    };
                    Err(anyhow::anyhow!("tool \"{name}\": {why}"))
                }
            }
        });
        out.push(Box::new(tool));
    }
    Ok(out)
}

/// Fulfill the channel for an in-flight tool call. Called from JS after the
/// user's `run` returns. A missing id means the call already timed out (the
/// closure dropped its sender) — the late result is dropped, harmless.
#[napi(js_name = "resolveTool")]
pub fn resolve_tool(id: i64, result: ToolResult) -> Result<()> {
    if let Some(tx) = pending_tools().lock().unwrap().remove(&id) {
        let _ = tx.send(result);
    }
    Ok(())
}

// ── custom JS provider: TSFN round-trip (mirrors the tool round-trip) ────────
//
// A custom provider is a JS function `chat(request) -> Promise<{text, …}>`.
// Like a JS tool it can't run on the agent's worker thread, so its call
// round-trips: the worker sends `{ id, request }` to JS (via a TSFN), blocks on
// a channel; the wrapped JS provider runs on the main thread and fulfills via
// `resolveProvider(id, result)`. The result is a `serde_json::Value` (the raw
// JS object), parsed into [`ta::SimpleChatResponse`] on the Rust side.
//
// Non-streaming v1: the provider returns the *whole* text at once; it's emitted
// as a single `TextDelta` via [`ta::ChatResponse::from_simple`]. Incremental
// streaming from a JS provider is future work (it'd need a second TSFN to push
// deltas JS→Rust mid-call).

static NEXT_PROVIDER_ID: AtomicI64 = AtomicI64::new(1);
static PENDING_PROVIDERS: OnceLock<Mutex<HashMap<i64, mpsc::Sender<Value>>>> = OnceLock::new();

fn pending_providers() -> &'static Mutex<HashMap<i64, mpsc::Sender<Value>>> {
    PENDING_PROVIDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Payload handed to the wrapped JS provider callback: the call's id (to report
/// back via `resolveProvider`) and the request object.
#[napi(object)]
pub struct ProviderCall {
    pub id: i64,
    pub request: Value,
}

/// Fulfill the channel for an in-flight provider call. Called from JS after the
/// wrapped provider resolves. A missing id means the call already timed out —
/// the late result is dropped, harmless (same discipline as `resolveTool`).
#[napi(js_name = "resolveProvider")]
pub fn resolve_provider(id: i64, result: Value) -> Result<()> {
    if let Some(tx) = pending_providers().lock().unwrap().remove(&id) {
        let _ = tx.send(result);
    }
    Ok(())
}

/// The request shape handed to a JS custom provider (excludes cancel / timeout
/// / cache — agent-internal plumbing the host function shouldn't see).
#[derive(serde::Serialize)]
struct ProviderRequest<'a> {
    system: Option<&'a str>,
    messages: &'a [ta::Message],
    tools: &'a [ta::ToolDef],
    model: &'a str,
    max_tokens: u32,
}

fn provider_request_value(req: &ta::ChatRequest) -> Result<Value> {
    serde_json::to_value(ProviderRequest {
        system: req.system,
        messages: req.messages,
        tools: req.tools,
        model: req.model,
        max_tokens: req.max_tokens,
    })
    .map_err(err)
}

/// A custom provider backed by a JS function (wrapped by `wrapper.js` into the
/// `(err, call)` CalleeHandled TSFN shape). Holds the TSFN; `chat` drives the
/// round-trip and blocks the agent worker thread for the JS result.
struct NodeProvider {
    tsfn: ThreadsafeFunction<ProviderCall, ErrorStrategy::CalleeHandled>,
}

impl ta::Provider for NodeProvider {
    fn chat(
        &self,
        req: &ta::ChatRequest,
        on_event: &mut dyn FnMut(ta::provider::ChatEvent),
    ) -> anyhow::Result<ta::ChatResponse> {
        let id = NEXT_PROVIDER_ID.fetch_add(1, Ordering::Relaxed);
        // Serialize BEFORE registering in the pending map: a request that
        // fails to serialize must not strand a registry entry —
        // resolveProvider never fires for a call that was never dispatched,
        // and unlike the timeout path there's no other reclaimer (the same
        // leak the timeout arm already guards against, on the rarer arm).
        let call = ProviderCall {
            id,
            request: provider_request_value(req)?,
        };
        let (tx, rx) = mpsc::channel();
        pending_providers().lock().unwrap().insert(id, tx);
        if self
            .tsfn
            .call(Ok(call), ThreadsafeFunctionCallMode::NonBlocking)
            != Status::Ok
        {
            pending_providers().lock().unwrap().remove(&id);
            return Err(err("provider: dispatch to JS failed").into());
        }

        // Block the agent worker thread for the JS result. The JS main thread
        // is free (the run() caller awaits it), so the wrapped provider
        // resolves promptly; the timeout is a backstop against a provider that
        // never returns. When the run set a wall-clock budget, the request
        // carries its REMAINDER as `per_request_timeout` (the built-in HTTP
        // providers cap themselves the same way) — the wait never outlives
        // the run budget; JS_CALLBACK_TIMEOUT_SECS stays the ceiling for
        // unbudgeted runs.
        let wait = req
            .per_request_timeout
            .filter(|d| *d < Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS))
            .unwrap_or(Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS));
        // Round UP for the message: the budget remainder is sub-second
        // precision, and "within 0s" for a ~1s wait reads like a bug.
        let wait_secs = wait.as_secs() + u64::from(wait.subsec_millis() > 0);
        let resp = rx.recv_timeout(wait).map_err(|_| {
            // Reclaim the registry entry on timeout: resolveProvider is the
            // only other remover and it never fires for an abandoned call —
            // each timed-out request would strand one entry forever (and the
            // budget-bounded wait makes timeouts routine, not exceptional).
            // The tools registry does the same in its timeout arm.
            pending_providers().lock().unwrap().remove(&id);
            err(format!(
                "provider: JS callback did not resolve within {wait_secs}s"
            ))
        })?;
        // The wrapper signals a thrown user provider via a `{__error}` marker so
        // the run fails honestly (mirroring the Python binding, where a raised
        // exception fails the turn) instead of producing a silent empty turn.
        if let Some(msg) = resp.get("__error").and_then(|v| v.as_str()) {
            return Err(anyhow::anyhow!("provider callback failed: {msg}"));
        }
        let simple = ta::SimpleChatResponse::from_value(&resp)?;
        Ok(ta::ChatResponse::from_simple(simple, on_event))
    }
}

/// Turn the user's wrapped JS provider `{ chat: async (err, call) => … }` into a
/// `Provider` trait object. `wrapper.js` normalizes the user's `provider`
/// (function or `{ chat }` object) into that single-callable shape, the same way
/// it wraps tools — so here we just extract `.chat` as a TSFN.
fn build_provider(env: Env, provider: Option<Object>) -> Result<Option<Box<dyn ta::Provider>>> {
    let Some(p) = provider else {
        return Ok(None);
    };
    let chat_fn: JsFunction = p
        .get("chat")?
        .ok_or_else(|| err("custom provider object missing \"chat\""))?;
    let tsfn = env.create_threadsafe_function(&chat_fn, 0, |ctx| Ok(vec![ctx.value]))?;
    Ok(Some(Box::new(NodeProvider { tsfn })))
}

// ── JS hooks: interception layer (mirrors the tool round-trip for the gate) ──
//
// The hooks layer needs three call shapes:
//   beforeTool: must RETURN a decision → blocking round-trip, identical
//               machinery to the custom-tool round-trip (TSFN → JS runs the
//               user fn on the main thread → resolveHooksDecision(id, result)
//               fulfills the channel). A never-resolving / ill-formed gate
//               FAILS CLOSED: the tool call is denied with a message the
//               model sees — a broken policy must not degrade into
//               allow-everything.
//   afterTool / onChatRequest: observe-only, no return value → fire-and-forget
//               TSFNs (ErrorStrategy::Fatal, same as the event TSFN). wrapper.js
//               wraps them with the same swallow guard as onEvent, because a
//               raw throw in a Fatal-strategy callback is an UNCATCHABLE
//               process abort.

static NEXT_HOOK_ID: AtomicI64 = AtomicI64::new(1);
static PENDING_HOOKS: OnceLock<Mutex<HashMap<i64, mpsc::Sender<Value>>>> = OnceLock::new();

fn pending_hooks() -> &'static Mutex<HashMap<i64, mpsc::Sender<Value>>> {
    PENDING_HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Payload handed to the wrapped JS `beforeTool`: the call's id (to report
/// back via `resolveHooksDecision`), the tool name, the model's ORIGINAL
/// arguments, and the attribution ctx (`{workdir, subagent}`).
#[napi(object)]
pub struct HookGateCall {
    pub id: i64,
    pub tool: String,
    pub input: Value,
    pub ctx: Value,
}

/// Fulfill the channel for an in-flight gate call. Called from JS after the
/// wrapped `beforeTool` returns. A missing id means the call already timed
/// out — the late decision is dropped, harmless (same discipline as
/// `resolveTool` / `resolveProvider`).
#[napi(js_name = "resolveHooksDecision")]
pub fn resolve_hooks_decision(id: i64, result: Value) -> Result<()> {
    if let Some(tx) = pending_hooks().lock().unwrap().remove(&id) {
        let _ = tx.send(result);
    }
    Ok(())
}

/// Parse a JS decision into a [`ta::ToolDecision`]: `null` → Allow, a string
/// → Deny(message), `{deny: string}` / `{modify: input}` → the matching
/// decision. Anything else fails closed.
fn decision_from_value(v: Value, tool: &str) -> ta::ToolDecision {
    match v {
        Value::Null => ta::ToolDecision::Allow,
        Value::String(s) => ta::ToolDecision::Deny(s),
        Value::Object(ref m) => match (m.get("deny"), m.get("modify")) {
            (Some(Value::String(d)), _) => ta::ToolDecision::Deny(d.clone()),
            (None, Some(modv)) => ta::ToolDecision::Modify(modv.clone()),
            _ => ta::ToolDecision::Deny(format!(
                "[hooks] beforeTool('{tool}') refused the call: object must be \
                 {{deny: string}} or {{modify: input}}"
            )),
        },
        _ => ta::ToolDecision::Deny(format!(
            "[hooks] beforeTool('{tool}') refused the call: expected null, a deny \
             string, or {{deny|modify: …}}"
        )),
    }
}

/// The ctx projection (same minimal attribution surface as the Python
/// binding: full ToolCtx plumbing stays binding-internal).
#[derive(serde::Serialize)]
struct HookCtx<'a> {
    workdir: &'a str,
    subagent: Option<&'a str>,
}

/// The observe-only afterTool payload: original input, an output view
/// ({content, is_error, meta}), whether it executed (false only for a Deny),
/// and the ctx.
#[derive(serde::Serialize)]
struct HookAfterCall<'a> {
    tool: &'a str,
    input: &'a Value,
    output: HookAfterOutput<'a>,
    executed: bool,
    ctx: HookCtx<'a>,
}

#[derive(serde::Serialize)]
struct HookAfterOutput<'a> {
    content: &'a str,
    is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<&'a Value>,
}

/// The onChatRequest projection (same shape the custom provider receives,
/// minus the agent-internal cancel / timeout plumbing).
#[derive(serde::Serialize)]
struct HookChatRequest<'a> {
    system: Option<&'a str>,
    messages: &'a [ta::Message],
    tools: &'a [ta::ToolDef],
    model: &'a str,
    max_tokens: u32,
    cache: bool,
}

struct JsHooks {
    before: Option<ThreadsafeFunction<HookGateCall, ErrorStrategy::CalleeHandled>>,
    after: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
    on_chat: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
}

impl ta::Hooks for JsHooks {
    fn before_tool(&self, tool: &str, input: &Value, ctx: &ta::ToolCtx) -> ta::ToolDecision {
        let Some(tsfn) = self.before.as_ref() else {
            return ta::ToolDecision::Allow;
        };
        let id = NEXT_HOOK_ID.fetch_add(1, Ordering::Relaxed);
        let Ok(ctx_v) = serde_json::to_value(HookCtx {
            workdir: &ctx.workdir.to_string_lossy(),
            subagent: ctx.subagent.as_deref(),
        }) else {
            return ta::ToolDecision::Deny(format!(
                "[hooks] beforeTool('{tool}') refused the call: ctx serialization failed"
            ));
        };
        let call = HookGateCall {
            id,
            tool: tool.to_string(),
            input: input.clone(),
            ctx: ctx_v,
        };
        let (tx, rx) = mpsc::channel();
        pending_hooks().lock().unwrap().insert(id, tx);
        if tsfn.call(Ok(call), ThreadsafeFunctionCallMode::NonBlocking) != Status::Ok {
            pending_hooks().lock().unwrap().remove(&id);
            return ta::ToolDecision::Deny(format!(
                "[hooks] beforeTool('{tool}') refused the call: dispatch to JS failed"
            ));
        }
        // Same wait discipline as the tool round-trip: the run's remaining
        // wall-clock budget (capped by the backstop), cancel-aware in slices.
        let wait = callback_wait_ceiling(ctx.deadline);
        match recv_interruptible(&rx, wait, ctx.cancel.as_deref()) {
            Ok(v) => decision_from_value(v, tool),
            Err(_) => ta::ToolDecision::Deny(format!(
                "[hooks] beforeTool('{tool}') refused the call: JS callback did not \
                 resolve within {}s",
                wait.as_secs() + u64::from(wait.subsec_millis() > 0)
            )),
        }
    }

    fn after_tool(
        &self,
        tool: &str,
        input: &Value,
        output: &ta::ToolOutput,
        executed: bool,
        ctx: &ta::ToolCtx,
    ) {
        if let Some(tsfn) = self.after.as_ref() {
            if let Ok(v) = serde_json::to_value(HookAfterCall {
                tool,
                input,
                output: HookAfterOutput {
                    content: &output.content,
                    is_error: output.is_error,
                    meta: output.meta.as_ref(),
                },
                executed,
                ctx: HookCtx {
                    workdir: &ctx.workdir.to_string_lossy(),
                    subagent: ctx.subagent.as_deref(),
                },
            }) {
                let _ = tsfn.call(v, ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }

    fn on_chat_request(&self, req: &ta::ChatRequest) {
        if let Some(tsfn) = self.on_chat.as_ref() {
            if let Ok(v) = serde_json::to_value(HookChatRequest {
                system: req.system,
                messages: req.messages,
                tools: req.tools,
                model: req.model,
                max_tokens: req.max_tokens,
                cache: req.cache,
            }) {
                let _ = tsfn.call(v, ThreadsafeFunctionCallMode::NonBlocking);
            }
        }
    }
}

/// One optional hook method. `null` / `undefined` count as absent; anything
/// else except a function is rejected before it can reach the TSFN
/// constructor (see [`build_hooks`] for why that matters).
fn hook_function(h: &Object, key: &str) -> Result<Option<JsFunction>> {
    let Some(v) = h.get::<_, napi::JsUnknown>(key)? else {
        return Ok(None);
    };
    match v.get_type()? {
        napi::ValueType::Undefined | napi::ValueType::Null => Ok(None),
        napi::ValueType::Function => Ok(Some(unsafe { v.cast::<JsFunction>() })),
        other => Err(err(format!(
            "hooks.{key} must be a function when present (got {other})"
        ))),
    }
}

/// Turn the user's duck-typed hooks object into a [`ta::Hooks`]. Methods are
/// resolved HERE, on the JS thread, once — a present-but-not-a-function
/// property errors at install time instead of failing silently mid-run.
/// Returns None when no hooks object was passed (no layer installed).
///
/// The shape check is not cosmetic: handing a non-function to napi's TSFN
/// constructor trips an internal assertion (`(func) != nullptr`) and ABORTS
/// the whole process — uncatchable from JS. Checking `typeof` here, before any
/// TSFN exists, turns that abort into an ordinary throwable error on every
/// entry point (`new`, `Agent.create`, `setHooks`, `chat`).
fn build_hooks(env: Env, hooks: Option<Object>) -> Result<Option<Arc<dyn ta::Hooks>>> {
    let Some(h) = hooks else { return Ok(None) };
    let before: Option<JsFunction> = hook_function(&h, "beforeTool")?;
    let after: Option<JsFunction> = hook_function(&h, "afterTool")?;
    let on_chat: Option<JsFunction> = hook_function(&h, "onChatRequest")?;
    // Env::create_threadsafe_function hardwires the CalleeHandled strategy;
    // the observe-only hooks need Fatal (fire-and-forget), so go through the
    // FromNapiValue conversion the #[napi] params themselves use, which takes
    // the strategy from the target type. Same mechanism, same safety (the
    // function object is pinned for the TSFN's lifetime by napi).
    let before = before
        .map(|f: JsFunction| unsafe {
            ThreadsafeFunction::<HookGateCall, ErrorStrategy::CalleeHandled>::from_napi_value(
                env.raw(),
                f.raw(),
            )
        })
        .transpose()?;
    let after = after
        .map(|f: JsFunction| unsafe {
            ThreadsafeFunction::<Value, ErrorStrategy::Fatal>::from_napi_value(env.raw(), f.raw())
        })
        .transpose()?;
    let on_chat = on_chat
        .map(|f: JsFunction| unsafe {
            ThreadsafeFunction::<Value, ErrorStrategy::Fatal>::from_napi_value(env.raw(), f.raw())
        })
        .transpose()?;
    Ok(Some(Arc::new(JsHooks {
        before,
        after,
        on_chat,
    })))
}

// ── agent work on dedicated threads (NOT the libuv pool) ─────────────────────
//
// `run`/`compact`/`chat` drive the synchronous core agent on a freshly spawned
// `std::thread` and settle the returned Promise via napi's `JsDeferred` (Send —
// resolvable from any thread). They must NOT run as a napi `AsyncTask`: the
// agent thread blocks in the JS tool/provider round-trips above, and the
// wrapped JS callbacks schedule their own async work (fs.promises, timers,
// promise continuations) onto the shared libuv worker pool those same
// AsyncTasks occupy — `min(runs, UV_THREADPOOL_SIZE)` (default 4) concurrent
// runs exhaust the pool and every blocked run waits on callbacks whose pool
// work can never be scheduled: a total deadlock. Dedicated threads cost one
// OS thread per in-flight run and scale independently of the pool size.

/// The resolver shape napi runs on the JS thread to settle a deferred: turn
/// the value the worker produced into a JS value (`Outcome` is pre-serialized
/// to a `Value`; the async constructor materializes an `Agent` class instance).
type Resolver<T> = Box<dyn FnOnce(Env) -> Result<T>>;

/// Every agent Promise (`run` / `compact` / `chat`) settles with an [`Outcome`].
type AgentResolver = Resolver<Outcome>;

/// A deferred parked between the JS thread and a worker until exactly one side
/// settles it (see [`spawn_agent_work`]).
type DeferredSlot<T> = Arc<Mutex<Option<JsDeferred<T, Resolver<T>>>>>;

/// Surface the structured cost a failed run burned, not just the Display
/// message — the binding previously dropped usage / tool_calls / turns /
/// elapsed, contradicting the core library's "honest cost" design (CLAUDE.md
/// "重试诚实成本"). Serialize RunError (manual Serialize impl) to a JSON string
/// so JS callers can JSON.parse(err.message) to read e.g. .usage.input_tokens;
/// the `error` field carries the human-readable message.
fn run_error_to_napi(e: &ta::RunError) -> Error {
    match serde_json::to_string(e) {
        Ok(json) => Error::from_reason(json),
        Err(_) => err(e),
    }
}

/// Spawn `work` on a dedicated thread and wire its result into `deferred`:
/// Ok(value) resolves, Err rejects, and a PANIC rejects too (debug builds —
/// release is `panic = "abort"`, where no unwinding happens at all) so the
/// JS side is never left with an unsettled Promise. The 8 MiB stack matches
/// the libuv worker default the AsyncTask shape used to run on, keeping
/// headroom for deep JSON (de)serialization and provider call chains.
///
/// Generic over the settled value: `run` / `chat` / `compact` settle with an
/// [`Outcome`] (a pre-serialized JSON `Value`), the async constructor
/// (`Agent::create`) settles with an `Agent`. The RESOLVER runs on the JS
/// thread (napi's TSFN), which is what makes the constructor case work at all:
/// napi's class registry is thread-local, so a class instance can only be
/// materialized where the class was registered.
fn spawn_agent_work<Data, F>(deferred: JsDeferred<Data, Resolver<Data>>, work: F)
where
    Data: ToNapiValue + Send + 'static,
    F: FnOnce() -> Result<Data> + Send + 'static,
{
    // Park the deferred behind a slot so it can be recovered and REJECTED if
    // thread creation itself fails — `Builder::spawn` drops the closure on
    // error, and a dropped JsDeferred settles nothing (no Drop impl), which
    // would strand the Promise forever.
    let slot: DeferredSlot<Data> = Arc::new(Mutex::new(Some(deferred)));
    let worker_slot = slot.clone();
    let spawned = std::thread::Builder::new()
        .name("tiny-agent-run".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let deferred = worker_slot
                .lock()
                .unwrap()
                .take()
                .expect("deferred settled exactly once");
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
                Ok(Ok(v)) => deferred.resolve(Box::new(move |_env| Ok(v))),
                Ok(Err(e)) => deferred.reject(e),
                Err(_) => deferred.reject(err("agent worker thread panicked")),
            }
        });
    if let Err(e) = spawned {
        // Thread creation failed (resource exhaustion): settle the Promise
        // rather than stranding it.
        if let Some(d) = slot.lock().unwrap().take() {
            d.reject(err(format!("failed to spawn agent worker thread: {e}")));
        }
    }
}

// ── CancelToken ──────────────────────────────────────────────────────────────

/// A cancellation flag shared with the agent. Call `.cancel()` to interrupt a
/// running turn at the next boundary (streaming read or tool check), mirroring
/// Rust's `Arc<AtomicBool>` cancel flag. Must be passed to the agent via
/// `setCancel()` *before* `run()` (so the shared flag is installed); flipping
/// it from any thread then interrupts the run without contending the agent's
/// internal mutex.
#[napi]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

#[napi]
impl CancelToken {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Flip the flag; the running turn stops at the next boundary.
    #[napi]
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    #[napi]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

// ── Agent ────────────────────────────────────────────────────────────────────

/// Busy error for a sync method called while `run()` is in flight. The run
/// holds the agent mutex on its dedicated worker thread for the whole
/// duration; BLOCKING the JS main thread on that lock would freeze the event
/// loop the run itself depends on (TSFN event/tool/provider callbacks dispatch
/// there — a blocked loop means the run can never finish: a deadlock in all
/// but name). try_lock + an immediate, explanatory error is the only safe
/// shape. After the awaited run() resolves, the methods work again.
fn agent_busy() -> Error {
    err("Agent.run() is active: sync methods (pushUser, setCancel, \
        clearCancel, usage, contextBreakdown, messages, model, setHooks, \
        setModel/setReasoningEffort/setMaxTokens/setMaxContextTokens, \
        addMcpServer/removeMcpServer, \
        enableCode/disableCode/setCodeConfig) must \
        not be called until the awaited run() resolves — calling them while a run is \
        in flight would freeze the JS event loop the run depends on")
}

/// A multi-turn agent. Construct from a config file path, optionally with an
/// explicit API key; call `.run()` per turn.
#[napi]
pub struct Agent {
    inner: Arc<Mutex<ta::Agent>>,
}

/// Resolver shape for the async constructor (`Agent.create`): the finished
/// `Agent` is materialized as a class instance IN the resolver, which napi runs
/// on the JS thread — napi's class registry is thread-local, so the instance
/// cannot be built on the worker that assembled it.
type AgentBuildResolver = Resolver<Agent>;

/// The config-load + agent-assembly half shared by `new Agent(...)`,
/// `Agent.create(...)`, and `chat()`. Runs on whatever thread calls it — every
/// JS-thread-bound step (TSFN creation for custom tools / provider / hooks)
/// must already have happened, which is exactly what lets `create()` move this
/// work onto a worker thread.
fn assemble_agent(
    config_path: &str,
    api_key: Option<String>,
    provider: Option<Box<dyn ta::Provider>>,
    extra: Vec<Box<dyn ta::Tool>>,
    hooks: Option<Arc<dyn ta::Hooks>>,
) -> Result<ta::Agent> {
    let cfg = ta::Config::load(Path::new(config_path)).map_err(err)?;
    // A custom provider needs no API key for the main agent; only fall back
    // to cfg.api_key() when using the built-in provider.
    let api_key = match api_key {
        Some(k) => k,
        None => match &provider {
            Some(_) => String::new(),
            None => cfg.api_key().map_err(err)?,
        },
    };
    let client = ta::provider::new_client(cfg.limits.timeout_secs);
    let mut agent = match provider {
        Some(p) => ta::build_agent_with_provider(&cfg, p, Vec::new(), &client, extra, &api_key),
        None => ta::build_agent_with(&cfg, Vec::new(), &client, extra, &api_key),
    }
    .map_err(err)?;
    if let Some(h) = hooks {
        agent.set_hooks(h);
    }
    Ok(agent)
}

#[napi]
impl Agent {
    /// Synchronous constructor: config load + full assembly — including every
    /// configured MCP server's spawn + handshake — run on the JS main thread.
    /// With a slow MCP server that freezes the event loop for seconds. Prefer
    /// `Agent.create(...)` whenever the config lists MCP servers: it does the
    /// same work on a worker thread and resolves with the agent.
    #[napi(constructor)]
    pub fn new(
        env: Env,
        config_path: String,
        tools: Option<Vec<Object>>,
        provider: Option<Object>,
        api_key: Option<String>,
        hooks: Option<Object>,
    ) -> Result<Self> {
        let extra = build_extra_tools(env, tools)?;
        // The TSFNs must be created on the JS thread (env); build_provider /
        // build_hooks do that, then the trait objects are Send and safe to
        // thread into the agent.
        let prov = build_provider(env, provider)?;
        let hooks_layer = build_hooks(env, hooks)?;
        let agent = assemble_agent(&config_path, api_key, prov, extra, hooks_layer)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(agent)),
        })
    }

    /// Async constructor — same result as `new Agent(...)`, but the
    /// seconds-long part (config load + tool assembly, i.e. spawning and
    /// handshaking every configured MCP server) runs on a dedicated worker
    /// thread, so the JS event loop stays free while it happens. Only the
    /// cheap JS-thread-bound steps (creating the custom-tool / provider /
    /// hooks TSFNs from the passed objects) run synchronously; a malformed
    /// `tools` entry still throws at call time, while config / assembly
    /// errors reject the returned Promise.
    #[napi]
    pub fn create(
        env: Env,
        config_path: String,
        tools: Option<Vec<Object>>,
        provider: Option<Object>,
        api_key: Option<String>,
        hooks: Option<Object>,
    ) -> Result<JsObject> {
        let extra = build_extra_tools(env, tools)?;
        let prov = build_provider(env, provider)?;
        let hooks_layer = build_hooks(env, hooks)?;
        let (deferred, promise) = env.create_deferred::<Agent, AgentBuildResolver>()?;
        spawn_agent_work(deferred, move || {
            let agent = assemble_agent(&config_path, api_key, prov, extra, hooks_layer)?;
            Ok(Agent {
                inner: Arc::new(Mutex::new(agent)),
            })
        });
        Ok(promise)
    }

    /// Install the interception layer from a duck-typed hooks object with any
    /// of `beforeTool(tool, input, ctx)` (returns null / deny string /
    /// {modify: input}; a throwing or ill-formed gate FAILS CLOSED — the call
    /// is denied with the reason in the message) / `afterTool(tool, input,
    /// output, executed, ctx)` / `onChatRequest(request)` (observe-only, no
    /// return value). Methods are resolved at install time; a present-but-
    /// not-a-function property errors here, not mid-run. Busy-errors while
    /// `run()` is in flight. `task` sub-agents inherit the layer; attribution
    /// arrives via `ctx.subagent` (`null` = top-level). NOTE: a rebuilt agent
    /// (e.g. from a resumed session) does NOT carry hooks over — reinstall.
    #[napi]
    pub fn set_hooks(&self, env: Env, hooks: Object) -> Result<()> {
        // TSFN creation needs the JS thread; do it BEFORE taking the lock so
        // a busy agent isn't blocked by JS-object conversion.
        let layer = build_hooks(env, Some(hooks))?.expect("build_hooks(Some) returns Some");
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.set_hooks(layer);
        Ok(())
    }

    /// Append a user message to the conversation, then (typically) call
    /// `.run()`. Errors with a busy error while `run()` is in flight (see
    /// [`agent_busy`]).
    ///
    /// `content` accepts a string (one text block — the original shorthand), a
    /// single content-block object, or an array of those. Block objects come in
    /// either the tagged form (`{type: 'text', text}` /
    /// `{type: 'image'|'document', media_type, data: <base64>}` — note `type`,
    /// not the wrapper's usual camelCase, because it is the wire format) or the
    /// serde form `messages()` reports (`{text}` / `{image: {media_type, data}}`),
    /// so a history block can be pushed straight back. `tool_use` /
    /// `tool_result` are refused: their ids must pair with the model's own
    /// output.
    #[napi]
    pub fn push_user(&self, content: Value) -> Result<()> {
        let blocks = push_user_blocks(&content)?;
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.push_user(blocks);
        Ok(())
    }

    /// Arm a `CancelToken`; its flag interrupts the next turn boundary. Safe
    /// to call while a run is in flight? No — same busy contract as the other
    /// sync methods; flip `token.cancel()` instead (the token itself is
    /// lock-free by design, that's the supported mid-run cancel path).
    /// Contract (kernel, TinyWorker): the flag is level-triggered and `run()`
    /// NEVER clears it — after a cancelled run, call `clearCancel()` at the
    /// point you accept the next task (e.g. right before `pushUser`/`run` of
    /// a new message) or every following run resolves instantly cancelled.
    #[napi]
    pub fn set_cancel(&self, token: &CancelToken) -> Result<()> {
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.set_cancel(token.flag.clone());
        Ok(())
    }

    /// Clear the cancel flag — the acceptance-point half of the cancel
    /// contract (see `set_cancel`). Call this when accepting a NEW task, not
    /// during a run (a stop signalled mid-run must land). Busy-errors while
    /// `run()` is in flight (see [`agent_busy`]).
    #[napi]
    pub fn clear_cancel(&self) -> Result<()> {
        let a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.clear_cancel();
        Ok(())
    }

    /// Run one turn (until end_turn / max_turns / cancel / error). `onEvent` is
    /// called for each `AgentEvent` (text deltas, tool calls, usage, …) as a
    /// plain object. Resolves with the `RunOutcome` object
    /// (final_text, turns, stop_reason, tool_calls, *_tokens, elapsed).
    #[napi]
    pub fn run(
        &self,
        env: Env,
        on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
    ) -> Result<JsObject> {
        let (deferred, promise) = env.create_deferred::<Outcome, AgentResolver>()?;
        let agent = self.inner.clone();
        spawn_agent_work(deferred, move || {
            let mut a = agent.lock().map_err(|_| err("agent mutex poisoned"))?;
            // Forward each event to JS as a serde Value; napi turns it into a
            // plain object (the externally-tagged AgentEvent shape, e.g.
            // { TextDelta: ".." }).
            let outcome = a
                .run(&mut |ev: ta::AgentEvent| {
                    if let Some(tsfn) = on_event.as_ref() {
                        if let Ok(v) = serde_json::to_value(&ev) {
                            let _ = tsfn.call(v, ThreadsafeFunctionCallMode::NonBlocking);
                        }
                    }
                })
                .map_err(|e| run_error_to_napi(&e))?;
            serde_json::to_value(&outcome).map(Outcome).map_err(err)
        });
        Ok(promise)
    }

    /// Manually compress the conversation history NOW (the idle-time fold):
    /// summarize the middle via the LLM, fold it into the first message,
    /// rewrite the session file. `instructions` is an optional focus hint for
    /// the summarizer ("focus on the API changes"); `onEvent` receives the
    /// `Compressed`/`Warning` events. Resolves with `{dropped, kept, usage}`
    /// or `null` when nothing was compressible (history too short); rejects
    /// on summary-request failure. The summary's cost is NOT part of the next
    /// run's `usage()` — meter it from the returned `usage` yourself.
    #[napi]
    pub fn compact(
        &self,
        env: Env,
        instructions: Option<String>,
        on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
    ) -> Result<JsObject> {
        let (deferred, promise) = env.create_deferred::<Outcome, AgentResolver>()?;
        let agent = self.inner.clone();
        spawn_agent_work(deferred, move || {
            let mut a = agent.lock().map_err(|_| err("agent mutex poisoned"))?;
            let outcome = a
                .compact_with(
                    &mut |ev: ta::AgentEvent| {
                        if let Some(tsfn) = on_event.as_ref() {
                            if let Ok(v) = serde_json::to_value(&ev) {
                                let _ = tsfn.call(v, ThreadsafeFunctionCallMode::NonBlocking);
                            }
                        }
                    },
                    instructions.as_deref(),
                )
                .map_err(|e| {
                    // Same honest-cost envelope as run_error_to_napi: the
                    // summary cost a failed compact burned (CompressError::
                    // burned, riding the anyhow chain) surfaces as JSON the
                    // caller reads via JSON.parse(err.message).burned — null
                    // when the failure carried no usage.
                    let payload = serde_json::json!({
                        "error": e.to_string(),
                        "burned": e
                            .downcast_ref::<ta::CompressError>()
                            .and_then(|ce| ce.burned),
                    });
                    napi::Error::from_reason(payload.to_string())
                })?;
            match outcome {
                Some(o) => serde_json::to_value(o).map(Outcome).map_err(err),
                None => Ok(Outcome(Value::Null)),
            }
        });
        Ok(promise)
    }

    /// Cumulative token usage of the current/latest run (4 axes), as an object.
    /// Busy-errors while `run()` is in flight (see [`agent_busy`]).
    #[napi]
    pub fn usage(&self) -> Result<Value> {
        let a = self.inner.try_lock().map_err(|_| agent_busy())?;
        serde_json::to_value(a.usage()).map_err(err)
    }

    /// Estimated token composition of the NEXT request's context — system
    /// (env/base/skill_intro), tools (builtin/mcp/skill/custom, per tool),
    /// messages by kind, plus total & calibration. Every number is an
    /// ESTIMATE (chars/4 yardstick), not a provider measurement — `usage()`
    /// stays the billing authority; `calibration` is non-null only while a
    /// measured anchor is still valid. Read-only; same busy contract as
    /// `usage()`.
    #[napi]
    pub fn context_breakdown(&self) -> Result<Value> {
        let a = self.inner.try_lock().map_err(|_| agent_busy())?;
        serde_json::to_value(a.context_breakdown()).map_err(err)
    }

    /// Conversation history as a list of message objects. Busy-errors while
    /// `run()` is in flight (see [`agent_busy`]).
    #[napi]
    pub fn messages(&self) -> Result<Value> {
        let a = self.inner.try_lock().map_err(|_| agent_busy())?;
        serde_json::to_value(a.messages()).map_err(err)
    }

    /// The model name this agent sends with each request. Busy-errors while
    /// `run()` is in flight (see [`agent_busy`]).
    #[napi]
    pub fn model(&self) -> Result<String> {
        let a = self.inner.try_lock().map_err(|_| agent_busy())?;
        Ok(a.model().to_string())
    }

    // ── hot-update surface (between runs) ───────────────────────────────────
    //
    // Pure field swaps in the kernel: the next request uses the new value, and
    // MCP connections / tool assembly / history / the session file are all
    // untouched (rebuilding the agent would re-run every MCP handshake —
    // seconds on a slow server). Deliberate scope, shared with the kernel:
    // `task` sub-agents capture their config at construction and do NOT follow
    // these setters — swap before spawning if it must apply. Every method here
    // follows the same busy contract as `pushUser` (try_lock + explanatory
    // error while a run holds the agent).

    /// Swap the model for subsequent requests (same provider/endpoint).
    /// Validity is the caller's: an unknown model name fails at the next
    /// request, honestly, like a config typo.
    #[napi]
    pub fn set_model(&self, model: String) -> Result<()> {
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.set_model(&model);
        Ok(())
    }

    /// Swap the reasoning-effort hint forwarded to the provider verbatim
    /// (`null`/omitted = server default). No enum validation — vendor ladders
    /// differ and shift across generations.
    #[napi]
    pub fn set_reasoning_effort(&self, effort: Option<String>) -> Result<()> {
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.set_reasoning_effort(effort.as_deref());
        Ok(())
    }

    /// Swap the per-request output budget. Accepts an integer in
    /// `0..=u32::MAX`; 0 is clamped to 1 by the kernel (a persistent 0 would
    /// make every following request an instant provider 400). The parameter
    /// is `f64` so the check sees the ORIGINAL JS number: napi's integer
    /// conversions truncate fractions and wrap out-of-range values (`-5`
    /// became 4294967291), which silently armed a huge budget.
    #[napi]
    pub fn set_max_tokens(&self, max_tokens: f64) -> Result<()> {
        if !max_tokens.is_finite()
            || max_tokens.fract() != 0.0
            || !(0.0..=f64::from(u32::MAX)).contains(&max_tokens)
        {
            return Err(err(
                "setMaxTokens: maxTokens must be an integer in 0..=4294967295 \
                 (0 is clamped to 1)",
            ));
        }
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.set_max_tokens(max_tokens as u32);
        Ok(())
    }

    /// Update the context-window ceiling used by the preflight compression
    /// estimate (`0` = fall back to the char heuristic). Hosts learn the REAL
    /// window from their model catalog at runtime; a wrong ceiling fires the
    /// preflight far too early or never. Takes `f64` so fractional / negative /
    /// absurd values are rejected instead of silently truncated or wrapped.
    #[napi]
    pub fn set_max_context_tokens(&self, tokens: f64) -> Result<()> {
        if !tokens.is_finite() || tokens.fract() != 0.0 || tokens < 0.0 {
            return Err(err("setMaxContextTokens: tokens must be an integer >= 0 \
                 (0 = fall back to the char heuristic)"));
        }
        // 2^64 is the first f64 above u64::MAX; rejecting it avoids a
        // saturating cast off by one.
        if tokens >= 18_446_744_073_709_551_616.0 {
            return Err(err(
                "setMaxContextTokens: tokens must be <= 18446744073709551615",
            ));
        }
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.set_max_context_tokens(tokens as u64);
        Ok(())
    }

    /// Attach one MCP server at runtime (spawn + handshake, then the tools
    /// join the next request). `cfg` is an object shaped like one entry of the
    /// config file's `"mcp_servers"` list (`{name, command, args, env}` or
    /// `{name, url, headers}`); unknown keys are an error. This is a
    /// SYNCHRONOUS method: it returns the new tool display names, and the
    /// handshake blocks the JS main thread for as long as the server takes —
    /// prefer adding servers before user-facing work, or construct the agent
    /// from a config that already lists them. A name collision is a
    /// recoverable error with the agent left exactly as it was (no partial
    /// add). NOT propagated to `task` sub-agents or the runCode sandbox
    /// registry.
    #[napi]
    pub fn add_mcp_server(&self, cfg: Value) -> Result<Vec<String>> {
        let parsed: ta::McpServerConfig =
            serde_json::from_value(cfg).map_err(|e| err(format!("mcp server object: {e}")))?;
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.add_mcp_server(parsed).map_err(err)
    }

    /// Detach an MCP server by its CONFIG name (the `name` field, not the
    /// `mcp_<server>_<tool>` display names). Returns how many tools were
    /// removed; an unknown name is an error, not a silent no-op. The stdio
    /// child dies when its last holder drops (immediately if only this agent
    /// held it).
    #[napi]
    pub fn remove_mcp_server(&self, name: String) -> Result<u32> {
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        let removed = a.remove_mcp_server(&name).map_err(err)?;
        u32::try_from(removed).map_err(|_| err("tool count overflowed u32"))
    }

    /// Enable the sandboxed `runCode` tool for the NEXT request (runtime
    /// switch — the tool becomes visible to the model; zero schema cost
    /// while disabled). Same busy contract as the other sync methods.
    ///
    /// Requires this module built with its `code` cargo feature
    /// (`npm run build -- --features code`); otherwise a clear error.
    #[napi]
    pub fn enable_code(&self) -> Result<()> {
        self.code_switch(true)
    }

    /// Remove `runCode` from the model's toolset (runtime switch). Does not
    /// interrupt an in-flight execution — the next request just stops
    /// offering the tool. Same feature requirement as `enableCode`.
    #[napi]
    pub fn disable_code(&self) -> Result<()> {
        self.code_switch(false)
    }

    /// Atomically replace the WHOLE code config (switch + every limit /
    /// path knob) from an object shaped like the config file's `"code"`
    /// section: unknown keys error, missing keys reset to their DEFAULTS
    /// (they do not retain previous values — send the full object to keep
    /// a knob). Takes effect for the next execution; an in-flight one keeps
    /// its snapshot. Busy-errors while `run()` is in flight.
    ///
    /// The `llm` sub-object is pinned by the sandbox registry at its first
    /// execution: after that, changing `llm` here still succeeds but cannot
    /// reach guest `llm` calls, and the kernel emits a diag warning telling
    /// the host to rebuild the agent. Limits / paths / `enabled` stay hot.
    #[napi]
    pub fn set_code_config(&self, cfg: Value) -> Result<()> {
        #[cfg(feature = "code")]
        {
            let parsed: ta::CodeConfig =
                serde_json::from_value(cfg).map_err(|e| err(format!("code config object: {e}")))?;
            let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
            a.set_code_config(parsed).map_err(err)
        }
        #[cfg(not(feature = "code"))]
        {
            let _ = cfg;
            Err(code_feature_missing())
        }
    }
}

impl Agent {
    /// Shared body of `enable_code` / `disable_code` (same cfg split, so the
    /// feature-missing error text lives in exactly one place).
    fn code_switch(&self, enabled: bool) -> Result<()> {
        #[cfg(feature = "code")]
        {
            let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
            a.set_code_enabled(enabled).map_err(err)
        }
        #[cfg(not(feature = "code"))]
        {
            let _ = enabled;
            Err(code_feature_missing())
        }
    }
}

// ── chat(): one-shot convenience ─────────────────────────────────────────────

/// One-shot: build an agent from `configPath`, send `message`, run to
/// completion, and resolve with the `RunOutcome` object. Optional `onEvent`
/// streams events as plain objects; optional `tools` injects custom JS tools.
/// The config load + agent build happen on the worker thread, so a missing key
/// / unreadable config rejects the promise rather than throwing synchronously
/// at call time. (Malformed `tools`, by contrast, throw synchronously — the
/// tool `Function`s must be taken from JS before the worker thread runs.)
#[napi]
#[allow(clippy::too_many_arguments)] // flat positional surface mirrors the other entry points
pub fn chat(
    env: Env,
    config_path: String,
    message: String,
    on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
    tools: Option<Vec<Object>>,
    provider: Option<Object>,
    api_key: Option<String>,
    hooks: Option<Object>,
) -> Result<JsObject> {
    let extra = build_extra_tools(env, tools)?;
    // Build the provider / hooks TSFNs on the JS thread (env); the trait
    // objects are Send and thread safely into the worker thread.
    let prov = build_provider(env, provider)?;
    let hooks_layer = build_hooks(env, hooks)?;
    let (deferred, promise) = env.create_deferred::<Outcome, AgentResolver>()?;
    spawn_agent_work(deferred, move || {
        let mut agent = assemble_agent(&config_path, api_key, prov, extra, hooks_layer)?;
        agent.push_user(vec![ta::Block::Text(message)]);

        let outcome = agent
            .run(&mut |ev: ta::AgentEvent| {
                if let Some(tsfn) = on_event.as_ref() {
                    if let Ok(v) = serde_json::to_value(&ev) {
                        let _ = tsfn.call(v, ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
            })
            .map_err(|e| run_error_to_napi(&e))?;

        serde_json::to_value(&outcome).map(Outcome).map_err(err)
    });
    Ok(promise)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_ceiling_distinguishes_unbudgeted_from_expired() {
        // No deadline → the 120s backstop.
        assert_eq!(
            callback_wait_ceiling(None),
            Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS)
        );
        // Far-future deadline → capped by the backstop, not the remainder.
        assert_eq!(
            callback_wait_ceiling(Some(Instant::now() + Duration::from_secs(3_600))),
            Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS)
        );
        // Near deadline → the (sub-120s) remainder itself.
        let ceiling = callback_wait_ceiling(Some(Instant::now() + Duration::from_secs(2)));
        assert!(ceiling > Duration::from_secs(1) && ceiling <= Duration::from_secs(2));
        // EXPIRED deadline → zero (fail fast). checked_duration_since returns
        // None here; the old code mistook that None for "no deadline" and
        // granted a fresh 120s window (external review P2-4).
        assert_eq!(
            callback_wait_ceiling(Some(Instant::now() - Duration::from_secs(1))),
            Duration::ZERO
        );
    }

    #[test]
    fn recv_interruptible_honors_cancel_within_a_slice() {
        let (tx, rx) = mpsc::channel::<ToolResult>();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A never-resolving JS side, a 120s ceiling, and a cancel that flips
        // after 300ms: the wait must give up within ~one poll slice of the
        // flip, not block out the ceiling.
        let flip = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };
        let started = Instant::now();
        let out = recv_interruptible(
            &rx,
            Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS),
            Some(&cancel),
        );
        assert!(out.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancel must interrupt the wait, took {:?}",
            started.elapsed()
        );
        flip.join().unwrap();
        drop(tx);
    }

    #[test]
    fn recv_interruptible_returns_the_resolution() {
        let (tx, rx) = mpsc::channel();
        tx.send(ToolResult {
            content: "ok".into(),
            is_error: false,
        })
        .unwrap();
        let out = recv_interruptible(&rx, Duration::from_secs(5), None).unwrap();
        assert_eq!(out.content, "ok");
        assert!(!out.is_error);
    }
}

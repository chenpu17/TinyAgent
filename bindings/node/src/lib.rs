//! Node.js / TypeScript bindings for TinyAgent — `import { chat, Agent } from 'tiny-agent'`.
//!
//! Exposes the agent loop, streaming events, cancel, and honest token usage to
//! JavaScript. The blocking `run()` runs inside a napi `AsyncTask` on a libuv
//! worker thread (no tokio — the core stays synchronous, like the Python
//! binding's `py.allow_threads`); events cross back to the JS main thread via a
//! `ThreadsafeFunction`.
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

// ── AsyncTask return wrapper ─────────────────────────────────────────────────

/// Carries a serde `Value` through napi's `AsyncTask` return path. The task's
/// `JsValue` must implement both `ToNapiValue` *and* `TypeName`; `serde_json::
/// Value` has the former (via the `serde-json` feature) but not the latter, so
/// this thin wrapper supplies `TypeName` and delegates the JS materialization
/// unchanged. The JS side sees the raw outcome object — no extra nesting.
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
            // the wait is then the REMAINDER (capped by the backstop), so a
            // never-returning JS tool can't outlive the run budget. Same shape
            // as the custom-provider wait below.
            let wait = ctx
                .deadline
                .and_then(|d| d.checked_duration_since(Instant::now()))
                .filter(|d| *d < Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS))
                .unwrap_or(Duration::from_secs(JS_CALLBACK_TIMEOUT_SECS));
            // Round UP for the message: a sub-second budget remainder would
            // otherwise read as "within 0s".
            let wait_secs = wait.as_secs() + u64::from(wait.subsec_millis() > 0);
            match rx.recv_timeout(wait) {
                Ok(r) => Ok(ta::ToolOutput {
                    content: r.content,
                    is_error: r.is_error,
                    image: None,
                    usage: None,
                    meta: None,
                }),
                Err(_) => {
                    pending_tools().lock().unwrap().remove(&id);
                    Err(anyhow::anyhow!(
                        "tool \"{name}\": JS callback did not resolve within {wait_secs}s"
                    ))
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
        let (tx, rx) = mpsc::channel();
        pending_providers().lock().unwrap().insert(id, tx);

        let call = ProviderCall {
            id,
            request: provider_request_value(req)?,
        };
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

// ── run task: drives one agent.run() on a libuv worker thread ────────────────

/// The async unit of work for `Agent#run()`. `compute` runs off the JS thread
/// (so the event loop stays free to receive event callbacks); the outcome is
/// converted to a `serde_json::Value` and materialized as a JS object in
/// `resolve`. (`chat()` uses [`ChatRunTask`], the same shape with a config path
/// instead of a pre-built agent.)
pub struct RunTask {
    /// Cloned from the live `Agent`. `Arc<Mutex>` so the task can take `&mut`
    /// on the worker thread without moving the agent out of its JS wrapper.
    pub agent: Arc<Mutex<ta::Agent>>,
    /// Optional streaming-event forwarder; called for each `AgentEvent`.
    pub on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
}

impl Task for RunTask {
    type Output = Value;
    type JsValue = Outcome;

    fn compute(&mut self) -> Result<Self::Output> {
        let RunTask { agent, on_event } = self;
        let mut a = agent.lock().map_err(|_| err("agent mutex poisoned"))?;

        // Forward each event to JS as a serde Value; napi turns it into a plain
        // object (the externally-tagged AgentEvent shape, e.g. { TextDelta: ".." }).
        let outcome = a
            .run(&mut |ev: ta::AgentEvent| {
                if let Some(tsfn) = on_event.as_ref() {
                    if let Ok(v) = serde_json::to_value(&ev) {
                        let _ = tsfn.call(v, ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
            })
            .map_err(|e: ta::RunError| {
                // Surface the structured cost a failed run burned, not just the
                // Display message — the binding previously dropped usage /
                // tool_calls / turns / elapsed, contradicting the core library's
                // "honest cost" design (CLAUDE.md "重试诚实成本"). Serialize
                // RunError (manual Serialize impl) to a JSON string so JS callers
                // can JSON.parse(err.message) to read e.g. .usage.input_tokens;
                // the `error` field carries the human-readable message.
                match serde_json::to_string(&e) {
                    Ok(json) => Error::from_reason(json),
                    Err(_) => err(&e),
                }
            })?;

        serde_json::to_value(&outcome).map_err(err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(Outcome(output))
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
/// holds the agent mutex on its libuv worker thread for the whole duration;
/// BLOCKING the JS main thread on that lock would freeze the event loop the
/// run itself depends on (TSFN event/tool/provider callbacks dispatch there —
/// a blocked loop means the run can never finish: a deadlock in all but
/// name). try_lock + an immediate, explanatory error is the only safe shape.
/// After the awaited run() resolves, the methods work again.
fn agent_busy() -> Error {
    err(
        "Agent.run() is active: sync methods (pushUser, setCancel, usage, \
        messages, model, enableCode/disableCode/setCodeConfig) must not be \
        called until the awaited run() resolves — calling them while a run is \
        in flight would freeze the JS event loop the run depends on",
    )
}

/// A multi-turn agent. Construct from a config file path, optionally with an
/// explicit API key; call `.run()` per turn.
#[napi]
pub struct Agent {
    inner: Arc<Mutex<ta::Agent>>,
}

#[napi]
impl Agent {
    #[napi(constructor)]
    pub fn new(
        env: Env,
        config_path: String,
        tools: Option<Vec<Object>>,
        provider: Option<Object>,
        api_key: Option<String>,
    ) -> Result<Self> {
        let cfg = ta::Config::load(Path::new(&config_path)).map_err(err)?;
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
        let extra = build_extra_tools(env, tools)?;
        // The TSFN must be created on the JS thread (env); build_provider does
        // that, then the trait object is Send and safe to thread into the agent.
        let prov = build_provider(env, provider)?;
        let agent = match prov {
            Some(p) => ta::build_agent_with_provider(&cfg, p, Vec::new(), &client, extra, &api_key),
            None => ta::build_agent_with(&cfg, Vec::new(), &client, extra, &api_key),
        }
        .map_err(err)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(agent)),
        })
    }

    /// Append a user message (text) to the conversation, then (typically) call
    /// `.run()`. Errors with a busy error while `run()` is in flight (see
    /// [`agent_busy`]).
    #[napi]
    pub fn push_user(&self, text: String) -> Result<()> {
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.push_user(vec![ta::Block::Text(text)]);
        Ok(())
    }

    /// Arm a `CancelToken`; its flag interrupts the next turn boundary. Safe
    /// to call while a run is in flight? No — same busy contract as the other
    /// sync methods; flip `token.cancel()` instead (the token itself is
    /// lock-free by design, that's the supported mid-run cancel path).
    #[napi]
    pub fn set_cancel(&self, token: &CancelToken) -> Result<()> {
        let mut a = self.inner.try_lock().map_err(|_| agent_busy())?;
        a.set_cancel(token.flag.clone());
        Ok(())
    }

    /// Run one turn (until end_turn / max_turns / cancel / error). `onEvent` is
    /// called for each `AgentEvent` (text deltas, tool calls, usage, …) as a
    /// plain object. Resolves with the `RunOutcome` object
    /// (final_text, turns, stop_reason, tool_calls, *_tokens, elapsed).
    #[napi]
    pub fn run(
        &self,
        on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
    ) -> AsyncTask<RunTask> {
        AsyncTask::new(RunTask {
            agent: self.inner.clone(),
            on_event,
        })
    }

    /// Cumulative token usage of the current/latest run (4 axes), as an object.
    /// Busy-errors while `run()` is in flight (see [`agent_busy`]).
    #[napi]
    pub fn usage(&self) -> Result<Value> {
        let a = self.inner.try_lock().map_err(|_| agent_busy())?;
        serde_json::to_value(a.usage()).map_err(err)
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
pub fn chat(
    env: Env,
    config_path: String,
    message: String,
    on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
    tools: Option<Vec<Object>>,
    provider: Option<Object>,
    api_key: Option<String>,
) -> Result<AsyncTask<ChatRunTask>> {
    let extra = build_extra_tools(env, tools)?;
    // Build the provider TSFN on the JS thread (env); the trait object is Send
    // and threads safely into the worker task.
    let prov = build_provider(env, provider)?;
    Ok(AsyncTask::new(ChatRunTask {
        config_path,
        message,
        api_key,
        on_event,
        tools: extra,
        provider: prov,
    }))
}

/// chat() builds its agent on the worker thread (config + key resolved there),
/// so it uses its own task rather than [`RunTask`] (which assumes a pre-built
/// agent). Same Value-bridged outcome + event forwarding; carries any injected
/// tools through to the worker thread.
pub struct ChatRunTask {
    pub config_path: String,
    pub message: String,
    pub api_key: Option<String>,
    pub on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
    pub tools: Vec<Box<dyn ta::Tool>>,
    /// A custom provider (TSFN built on the JS thread), or None to build the
    /// configured openai/anthropic provider from the config on the worker.
    pub provider: Option<Box<dyn ta::Provider>>,
}

impl Task for ChatRunTask {
    type Output = Value;
    type JsValue = Outcome;

    fn compute(&mut self) -> Result<Self::Output> {
        let cfg = ta::Config::load(Path::new(&self.config_path)).map_err(err)?;
        let api_key = match &self.api_key {
            Some(k) => k.clone(),
            None => match &self.provider {
                Some(_) => String::new(),
                None => cfg.api_key().map_err(err)?,
            },
        };
        let client = ta::provider::new_client(cfg.limits.timeout_secs);
        // Move the tools + provider out of `&mut self` (the builders take them
        // by value); they're spent once the agent owns them.
        let tools = std::mem::take(&mut self.tools);
        let provider = self.provider.take();
        let mut agent = match provider {
            Some(p) => ta::build_agent_with_provider(&cfg, p, Vec::new(), &client, tools, &api_key),
            None => ta::build_agent_with(&cfg, Vec::new(), &client, tools, &api_key),
        }
        .map_err(err)?;
        agent.push_user(vec![ta::Block::Text(self.message.clone())]);

        let on_event = &self.on_event;
        let outcome = agent
            .run(&mut |ev: ta::AgentEvent| {
                if let Some(tsfn) = on_event.as_ref() {
                    if let Ok(v) = serde_json::to_value(&ev) {
                        let _ = tsfn.call(v, ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
            })
            .map_err(|e: ta::RunError| {
                // Surface the structured cost a failed run burned, not just the
                // Display message — the binding previously dropped usage /
                // tool_calls / turns / elapsed, contradicting the core library's
                // "honest cost" design (CLAUDE.md "重试诚实成本"). Serialize
                // RunError (manual Serialize impl) to a JSON string so JS callers
                // can JSON.parse(err.message) to read e.g. .usage.input_tokens;
                // the `error` field carries the human-readable message.
                match serde_json::to_string(&e) {
                    Ok(json) => Error::from_reason(json),
                    Err(_) => err(&e),
                }
            })?;

        serde_json::to_value(&outcome).map_err(err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(Outcome(output))
    }
}

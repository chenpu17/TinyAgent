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

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use serde_json::Value;

use ta_core as ta;

// ── error helper ─────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
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
            .map_err(err)?;

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
        Self { flag: Arc::new(AtomicBool::new(false)) }
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

/// A multi-turn agent. Construct from a config file path, optionally with an
/// explicit API key; call `.run()` per turn.
#[napi]
pub struct Agent {
    inner: Arc<Mutex<ta::Agent>>,
}

#[napi]
impl Agent {
    #[napi(constructor)]
    pub fn new(config_path: String, api_key: Option<String>) -> Result<Self> {
        let cfg = ta::Config::load(Path::new(&config_path)).map_err(err)?;
        let api_key = match api_key {
            Some(k) => k,
            None => cfg.api_key().map_err(err)?,
        };
        let client = ta::provider::new_client(cfg.limits.timeout_secs);
        let agent = ta::build_agent_with(&cfg, Vec::new(), &client, Vec::new(), &api_key)
            .map_err(err)?;
        Ok(Self { inner: Arc::new(Mutex::new(agent)) })
    }

    /// Append a user message (text) to the conversation, then (typically) call
    /// `.run()`.
    #[napi]
    pub fn push_user(&self, text: String) -> Result<()> {
        self.inner
            .lock()
            .map_err(|_| err("agent mutex poisoned"))?
            .push_user(vec![ta::Block::Text(text)]);
        Ok(())
    }

    /// Arm a `CancelToken`; its flag interrupts the next turn boundary.
    #[napi]
    pub fn set_cancel(&self, token: &CancelToken) -> Result<()> {
        self.inner
            .lock()
            .map_err(|_| err("agent mutex poisoned"))?
            .set_cancel(token.flag.clone());
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
    #[napi]
    pub fn usage(&self) -> Result<Value> {
        let a = self.inner.lock().map_err(|_| err("agent mutex poisoned"))?;
        serde_json::to_value(a.usage()).map_err(err)
    }

    /// Conversation history as a list of message objects.
    #[napi]
    pub fn messages(&self) -> Result<Value> {
        let a = self.inner.lock().map_err(|_| err("agent mutex poisoned"))?;
        serde_json::to_value(a.messages()).map_err(err)
    }

    /// The model name this agent sends with each request.
    #[napi]
    pub fn model(&self) -> Result<String> {
        Ok(self.inner.lock().map_err(|_| err("agent mutex poisoned"))?.model().to_string())
    }
}

// ── chat(): one-shot convenience ─────────────────────────────────────────────

/// One-shot: build an agent from `configPath`, send `message`, run to
/// completion, and resolve with the `RunOutcome` object. Optional `onEvent`
/// streams events as plain objects. The config load + agent build happen on the
/// worker thread, so a missing key / unreadable config rejects the promise
/// rather than throwing synchronously at call time.
#[napi]
pub fn chat(
    config_path: String,
    message: String,
    on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
    api_key: Option<String>,
) -> AsyncTask<ChatRunTask> {
    AsyncTask::new(ChatRunTask {
        config_path,
        message,
        api_key,
        on_event,
    })
}

/// chat() builds its agent on the worker thread (config + key resolved there),
/// so it uses its own task rather than [`RunTask`] (which assumes a pre-built
/// agent). Same Value-bridged outcome + event forwarding.
pub struct ChatRunTask {
    pub config_path: String,
    pub message: String,
    pub api_key: Option<String>,
    pub on_event: Option<ThreadsafeFunction<Value, ErrorStrategy::Fatal>>,
}

impl Task for ChatRunTask {
    type Output = Value;
    type JsValue = Outcome;

    fn compute(&mut self) -> Result<Self::Output> {
        let cfg = ta::Config::load(Path::new(&self.config_path)).map_err(err)?;
        let api_key = match &self.api_key {
            Some(k) => k.clone(),
            None => cfg.api_key().map_err(err)?,
        };
        let client = ta::provider::new_client(cfg.limits.timeout_secs);
        let mut agent =
            ta::build_agent_with(&cfg, Vec::new(), &client, Vec::new(), &api_key).map_err(err)?;
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
            .map_err(err)?;

        serde_json::to_value(&outcome).map_err(err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(Outcome(output))
    }
}

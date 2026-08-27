//! Python bindings for TinyAgent — `import tiny_agent`.
//!
//! Exposes the agent loop, custom tools, streaming events, and honest token
//! accounting to Python. The blocking `run()` runs with the GIL released
//! (`py.allow_threads`) so other Python threads make progress during the
//! network-bound run; the event callback re-acquires the GIL per event via
//! `Python::with_gil`.
//!
//! `ta` is the Rust core library `tiny_agent` (package `tiny-agent`, aliased
//! here because this crate's own lib name — the Python import — is *also*
//! `tiny_agent`).

// pyo3 0.22's `#[pymethods]` / `#[pyfunction]` expansions insert a conversion
// shim around `PyResult<PyObject>` returns that clippy 1.93 flags as
// `useless_conversion` ("to the same type: pyo3::PyErr"). The flagged bodies
// are already minimal — `pythonize(...)?.unbind()` with nothing to remove — so
// this is a known macro/lig interaction, not a real redundancy. Scoped to this
// file (pure pyo3 glue) so a genuine useless conversion elsewhere still trips.
#![allow(clippy::useless_conversion)]

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pythonize::{depythonize, pythonize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// The core library: package `tiny-agent` (crate `tiny_agent`), imported under a
// distinct dep key so it doesn't clash with this crate's own lib name.
use ta_core as ta;

// ── error / conversion helpers ──────────────────────────────────────────────

fn to_pyerr(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Build a [`ta::Config`] from either a filesystem path (`str`) or an inline
/// `dict` (the same shape as a config JSON file).
fn load_config(obj: &Bound<'_, PyAny>) -> PyResult<ta::Config> {
    if let Ok(path) = obj.extract::<String>() {
        ta::Config::load(std::path::Path::new(&path)).map_err(to_pyerr)
    } else {
        depythonize::<ta::Config>(obj).map_err(to_pyerr)
    }
}

/// Shape a Python tool callback's return value accepts: a plain `str`, or a
/// dict with `content` and optional `is_error`.
#[derive(serde::Deserialize)]
struct PyToolResult {
    content: String,
    #[serde(default)]
    is_error: bool,
}

fn python_ret_to_tooloutput(bound: &Bound<'_, PyAny>) -> anyhow::Result<ta::ToolOutput> {
    if let Ok(s) = bound.extract::<String>() {
        return Ok(ta::ToolOutput::text(s));
    }
    let r: PyToolResult = depythonize(bound)
        .map_err(|e| anyhow::anyhow!("tool return must be a str or a dict with 'content': {e}"))?;
    Ok(ta::ToolOutput {
        content: r.content,
        is_error: r.is_error,
        image: None,
        usage: None,
        meta: None,
    })
}

/// Wrap each Python `Tool` (name + schema + callable) into a [`ta::CallbackTool`]
/// so the agent dispatches it through the same `Tool` impl the TypeScript
/// binding will reuse. The closure re-acquires the GIL to call back into Python
/// — tool execution happens on the agent's run thread, which already released
/// the GIL via `allow_threads`.
fn build_extra_tools(
    tools: Option<Vec<Py<PyTool>>>,
    py: Python<'_>,
) -> PyResult<Vec<Box<dyn ta::Tool>>> {
    let mut out: Vec<Box<dyn ta::Tool>> = Vec::new();
    if let Some(ts) = tools {
        for t in ts {
            let borrowed = t.bind(py).borrow();
            let def = borrowed.def.clone();
            let cb = borrowed.callback.clone();
            let tool = ta::CallbackTool::new(def, move |input: &serde_json::Value, _ctx: &ta::ToolCtx| {
                Python::with_gil(|py| -> anyhow::Result<ta::ToolOutput> {
                    let args = pythonize(py, input).map_err(|e| anyhow::anyhow!("{e}"))?;
                    let ret = cb.call1(py, (args,)).map_err(|e| anyhow::anyhow!("{e}"))?;
                    python_ret_to_tooloutput(ret.bind(py))
                })
            });
            out.push(Box::new(tool));
        }
    }
    Ok(out)
}

/// Drive one `agent.run()` with the GIL released, forwarding each event to the
/// optional Python callback, and return the outcome as a Python dict.
fn run_agent(
    agent: &mut ta::Agent,
    on_event: Option<PyObject>,
    py: Python<'_>,
) -> PyResult<PyObject> {
    let result = py.allow_threads(|| {
        agent.run(&mut |ev: ta::AgentEvent| {
            if let Some(cb) = on_event.as_ref() {
                // Re-acquire the GIL only long enough to hand the event to the
                // Python callback, then release it again for the network-bound
                // run. Errors in the callback are swallowed: a buggy on_event
                // must not poison the agent run.
                Python::with_gil(|py| {
                    if let Ok(py_ev) = pythonize(py, &ev) {
                        let _ = cb.call1(py, (py_ev,));
                    }
                });
            }
        })
    });
    match result {
        Ok(o) => {
            let bound = pythonize(py, &o)?;
            Ok(bound.unbind())
        }
        Err(e) => {
            // Surface the structured cost (usage / tool_calls / turns / elapsed)
            // a failed run burned, not just the Display message — the bindings
            // previously dropped these, contradicting the core library's
            // "honest cost" design (CLAUDE.md "重试诚实成本"). Serialize
            // RunError (manual Serialize impl) to a JSON string and carry it as
            // the RuntimeError message so callers can `json.loads(e.args[0])`
            // to read e.g. .usage.input_tokens; the `error` field carries the
            // human-readable message (mirrors the Node binding).
            match serde_json::to_string(&e) {
                Ok(json) => Err(PyRuntimeError::new_err(json)),
                Err(_) => Err(to_pyerr(&e)),
            }
        }
    }
}

// ── CancelToken ─────────────────────────────────────────────────────────────

/// A cancellation flag shared with the agent. Call `.cancel()` to interrupt a
/// running turn at the next boundary (streaming read or tool check), mirroring
/// Rust's `Arc<AtomicBool>` cancel flag.
#[pyclass]
#[derive(Clone)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

#[pymethods]
impl CancelToken {
    #[new]
    fn new() -> Self {
        Self { flag: Arc::new(AtomicBool::new(false)) }
    }

    /// Flip the flag; the running turn stops at the next boundary.
    fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

// ── Tool: a custom tool from a Python callable ──────────────────────────────

/// A custom tool backed by a Python callable. Pass instances to
/// `Agent(..., tools=[...])`. The model's JSON arguments arrive as a `dict`;
/// return a `str` (content) or a `dict` with `content` and optional `is_error`.
#[pyclass(name = "Tool")]
pub struct PyTool {
    def: ta::ToolDef,
    callback: PyObject,
}

#[pymethods]
impl PyTool {
    #[new]
    fn new(
        name: String,
        description: String,
        schema: &Bound<'_, PyAny>,
        callback: PyObject,
    ) -> PyResult<Self> {
        let schema: serde_json::Value = depythonize(schema).map_err(to_pyerr)?;
        Ok(Self {
            def: ta::ToolDef { name, description, schema },
            callback,
        })
    }
}

// ── Provider: a custom provider from a Python callable ──────────────────────

/// The request shape handed to a Python custom provider: the conversation +
/// model params the host function needs to produce a response. Excludes the
/// `cancel` flag / per-request timeout / cache hint — those are agent-internal
/// plumbing the host function shouldn't see (a non-streaming custom provider
/// runs to completion; cancel lands at the next turn boundary).
#[derive(serde::Serialize)]
struct ProviderRequest<'a> {
    system: Option<&'a str>,
    messages: &'a [ta::Message],
    tools: &'a [ta::ToolDef],
    model: &'a str,
    max_tokens: u32,
}

/// Accept a Python provider's return value: a plain `str` (shorthand for just
/// the text) or a `dict` with `text` and optional `stop_reason` / `usage`.
fn py_to_simple(bound: &Bound<'_, PyAny>) -> anyhow::Result<ta::SimpleChatResponse> {
    if let Ok(s) = bound.extract::<String>() {
        return Ok(ta::SimpleChatResponse { text: s, stop_reason: None, usage: None });
    }
    depythonize::<ta::SimpleChatResponse>(bound).map_err(|e| {
        anyhow::anyhow!("provider must return str or dict {{text, stop_reason?, usage?}}: {e}")
    })
}

/// A custom provider backed by a Python callable `chat(request) -> str | dict`.
///
/// `chat` runs on the agent's worker thread — the run loop released the GIL via
/// `allow_threads`, so `Python::with_gil` re-acquires it for the duration of the
/// host call (blocking, like any provider's network round-trip). The host
/// returns the **whole** text at once (non-streaming v1); it is emitted as a
/// single `TextDelta` via [`ta::ChatResponse::from_simple`], so the agent's
/// event stream / display still sees the assistant message.
///
/// Cancellation/timeout contract (deliberate v1 limitation): the call is
/// synchronous inside the GIL, so `limits.timeout_secs` and the cancel token
/// do NOT interrupt it — the agent waits for the host function to return, for
/// however long it takes. A host `chat` that can run long MUST bound itself
/// (e.g. its own timeout around the work it does); a raised exception fails
/// the turn honestly. The run budget still applies at the turn boundaries
/// around the call.
struct PyProvider {
    chat_fn: Py<PyAny>,
}

impl ta::Provider for PyProvider {
    fn chat(
        &self,
        req: &ta::ChatRequest,
        on_event: &mut dyn FnMut(ta::provider::ChatEvent),
    ) -> anyhow::Result<ta::ChatResponse> {
        Python::with_gil(|py| -> anyhow::Result<ta::ChatResponse> {
            let req_view = ProviderRequest {
                system: req.system,
                messages: req.messages,
                tools: req.tools,
                model: req.model,
                max_tokens: req.max_tokens,
            };
            let req_dict = pythonize(py, &req_view).map_err(|e| anyhow::anyhow!("{e}"))?;
            let ret = self
                .chat_fn
                .call1(py, (req_dict,))
                .map_err(|e| anyhow::anyhow!("provider callback raised: {e}"))?;
            let simple = py_to_simple(ret.bind(py))?;
            Ok(ta::ChatResponse::from_simple(simple, on_event))
        })
    }
}



/// A multi-turn agent. Construct from a config (path or dict), optionally with
/// custom tools and an explicit API key; call `.run()` per turn.
#[pyclass]
pub struct Agent {
    inner: Mutex<ta::Agent>,
}

#[pymethods]
impl Agent {
    #[new]
    #[pyo3(signature = (config, *, tools=None, provider=None, api_key=None))]
    fn new(
        config: &Bound<'_, PyAny>,
        tools: Option<Vec<Py<PyTool>>>,
        provider: Option<Py<PyAny>>,
        api_key: Option<String>,
        py: Python<'_>,
    ) -> PyResult<Self> {
        let cfg = load_config(config)?;
        // A custom provider needs no API key for the main agent; only fall back
        // to cfg.api_key() (which reads the env var) when using the built-in
        // provider. `task` sub-agents, if enabled, still build their own
        // built-in provider and would need a real key — documented limitation.
        let api_key = match api_key {
            Some(k) => k,
            None => match &provider {
                Some(_) => String::new(),
                None => cfg.api_key().map_err(to_pyerr)?,
            },
        };
        let client = ta::provider::new_client(cfg.limits.timeout_secs);
        let extra = build_extra_tools(tools, py)?;
        let agent = match provider {
            Some(chat_fn) => {
                let prov: Box<dyn ta::Provider> = Box::new(PyProvider { chat_fn });
                ta::build_agent_with_provider(&cfg, prov, Vec::new(), &client, extra, &api_key)
            }
            None => ta::build_agent_with(&cfg, Vec::new(), &client, extra, &api_key),
        }
        .map_err(to_pyerr)?;
        Ok(Self { inner: Mutex::new(agent) })
    }

    /// Append a user message (text) to the conversation, then `.run()`.
    fn push_user(&self, text: &str) {
        self.inner
            .lock()
            .expect("agent mutex poisoned")
            .push_user(vec![ta::Block::Text(text.into())]);
    }

    /// Arm a `CancelToken`; its flag interrupts the next turn boundary.
    fn set_cancel(&self, token: &CancelToken) {
        self.inner
            .lock()
            .expect("agent mutex poisoned")
            .set_cancel(token.flag.clone());
    }

    /// Run one turn (until end_turn / max_turns / cancel / error). `on_event`
    /// is called for each `AgentEvent` (text deltas, tool calls, usage, …) with
    /// the event as a dict. Returns the `RunOutcome` as a dict
    /// (final_text, turns, stop_reason, tool_calls, *_tokens, elapsed_secs).
    #[pyo3(signature = (on_event=None))]
    fn run(&self, on_event: Option<PyObject>, py: Python<'_>) -> PyResult<PyObject> {
        let mut agent = self.inner.lock().expect("agent mutex poisoned");
        run_agent(&mut agent, on_event, py)
    }

    /// Cumulative token usage of the current/latest run (4 axes).
    fn usage(&self, py: Python<'_>) -> PyResult<PyObject> {
        let bound = pythonize(py, &self.inner.lock().expect("agent mutex poisoned").usage())?;
        Ok(bound.unbind())
    }

    /// Conversation history as a list of message dicts.
    fn messages(&self, py: Python<'_>) -> PyResult<PyObject> {
        let bound = pythonize(py, self.inner.lock().expect("agent mutex poisoned").messages())?;
        Ok(bound.unbind())
    }

    /// The model name this agent sends with each request.
    fn model(&self) -> String {
        self.inner.lock().expect("agent mutex poisoned").model().to_string()
    }
}

// ── chat(): one-shot convenience ─────────────────────────────────────────────

/// One-shot: build an agent from `config`, send `message`, run to completion,
/// and return the `RunOutcome` dict. Optional `on_event` streams events.
#[pyfunction]
#[pyo3(signature = (config, message, *, on_event=None, tools=None, provider=None, api_key=None))]
fn chat(
    config: &Bound<'_, PyAny>,
    message: &str,
    on_event: Option<PyObject>,
    tools: Option<Vec<Py<PyTool>>>,
    provider: Option<Py<PyAny>>,
    api_key: Option<String>,
    py: Python<'_>,
) -> PyResult<PyObject> {
    let cfg = load_config(config)?;
    let api_key = match api_key {
        Some(k) => k,
        None => match &provider {
            Some(_) => String::new(),
            None => cfg.api_key().map_err(to_pyerr)?,
        },
    };
    let client = ta::provider::new_client(cfg.limits.timeout_secs);
    let extra = build_extra_tools(tools, py)?;
    let mut agent = match provider {
        Some(chat_fn) => {
            let prov: Box<dyn ta::Provider> = Box::new(PyProvider { chat_fn });
            ta::build_agent_with_provider(&cfg, prov, Vec::new(), &client, extra, &api_key)
        }
        None => ta::build_agent_with(&cfg, Vec::new(), &client, extra, &api_key),
    }
    .map_err(to_pyerr)?;
    agent.push_user(vec![ta::Block::Text(message.into())]);
    run_agent(&mut agent, on_event, py)
}

// ── module ──────────────────────────────────────────────────────────────────

#[pymodule]
fn tiny_agent(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Agent>()?;
    m.add_class::<CancelToken>()?;
    m.add_class::<PyTool>()?;
    m.add_function(wrap_pyfunction!(chat, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

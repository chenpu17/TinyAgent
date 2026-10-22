//! Python bindings for TinyAgent — `import tiny_agent`.
//!
//! Exposes the agent loop, custom tools, streaming events, and honest token
//! accounting to Python. The blocking `run()` runs with the GIL released
//! (`py.allow_threads`) so other Python threads make progress during the
//! network-bound run; the event callback re-acquires the GIL per event via
//! `Python::with_gil`.
//!
//! Lock model (`Agent` wraps the core agent in a `Mutex` because pyclasses
//! methods take `&self`): every sync method acquires that lock with the GIL
//! RELEASED (`with_agent` below), so a thread blocked on the lock never
//! holds the GIL — holding both forms a GIL↔mutex deadlock against the run
//! thread's `with_gil` event callbacks. Reentrant calls from those callbacks
//! (same thread, whose run already holds the lock) are converted into an
//! explanatory RuntimeError by a thread-local in-run flag instead of a
//! silent `std::sync::Mutex` self-deadlock.
//!
//! Callback / GIL caveat (no watchdog): every event / tool / provider
//! callback re-acquires the GIL, and the run thread waits for it to return
//! — a host callback that BLOCKS on something which itself needs the GIL
//! (e.g. `Thread.join()` on a thread running Python code, or acquiring a
//! lock the GIL-bound side holds) deadhangs the process with no timeout
//! (the Node binding's TSFN path has a 120s backstop; Python has none).
//! Keep callbacks non-blocking; never join GIL-dependent threads from one.
//!
//! Scope note: the `set_model` / limits hot-update family and MCP hot
//! add/remove are deliberately NOT exposed here (minimal-surface v0.3 decision
//! — Rust hosts get them via the core crate). The hooks interception layer IS
//! exposed (`Agent.set_hooks` / `hooks=` kwarg): a Python object with any of
//! `before_tool` / `after_tool` / `on_chat_request`; missing methods are
//! no-ops, and the core `after_tool` signature already carries `&ToolCtx`.
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
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// The core library: package `tiny-agent` (crate `tiny_agent`), imported under a
// distinct dep key so it doesn't clash with this crate's own lib name.
use ta_core as ta;

use serde_json::Value;

// ── error / conversion helpers ──────────────────────────────────────────────

fn to_pyerr(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Build a [`ta::Config`] from either a filesystem path (`str`) or an inline
/// `dict` (the same shape as a config JSON file). Both paths go through
/// `validate_and_normalize` — an inline dict used to skip workdir
/// canonicalization/existence checks and eager MCP validation, silently
/// diverging from the file path's semantics.
fn load_config(obj: &Bound<'_, PyAny>) -> PyResult<ta::Config> {
    let mut cfg = if let Ok(path) = obj.extract::<String>() {
        ta::Config::load(std::path::Path::new(&path)).map_err(to_pyerr)?
    } else {
        depythonize::<ta::Config>(obj).map_err(to_pyerr)?
    };
    // No-op for the file path (already normalized); the real work for dicts.
    cfg.validate_and_normalize().map_err(to_pyerr)?;
    Ok(cfg)
}

/// Error for the run_code methods when this module was built without its
/// `code` cargo feature (design §8.6: off by default so standard wheels stay
/// ~10MB lighter) — a clear rebuild hint instead of a silent no-op.
#[cfg(not(feature = "code"))]
fn code_feature_missing() -> PyErr {
    pyo3::exceptions::PyRuntimeError::new_err(
        "this tiny_agent build lacks the `code` feature — rebuild with \
         `maturin develop --features code` to use enable_code()/\
         disable_code()/set_code_config()",
    )
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
        usage_in_shared_budget: false,
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
            let tool =
                ta::CallbackTool::new(def, move |input: &serde_json::Value, _ctx: &ta::ToolCtx| {
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

/// Forward one event to the optional Python callback. Executed on the agent
/// run thread while the GIL is released (the whole run sits inside
/// `allow_threads`), so it re-acquires the GIL via `with_gil` just for the
/// call. Errors in the callback are swallowed: a buggy on_event must not
/// poison the agent run.
fn emit_event(on_event: &Option<PyObject>, ev: ta::AgentEvent) {
    if let Some(cb) = on_event.as_ref() {
        Python::with_gil(|py| {
            if let Ok(py_ev) = pythonize(py, &ev) {
                let _ = cb.call1(py, (py_ev,));
            }
        });
    }
}

/// Convert a run result into the Python return: the serialized outcome, or a
/// RuntimeError. Shared by `Agent::run` and the one-shot `chat()`.
fn outcome_to_py<T, E>(result: Result<T, E>, py: Python<'_>) -> PyResult<PyObject>
where
    T: serde::Serialize,
    E: serde::Serialize + std::fmt::Display,
{
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

thread_local! {
    /// Set while this thread is inside an `Agent::run` loop. Event and tool
    /// callbacks execute on this same thread, and the run holds the agent
    /// mutex for its whole duration — a callback calling a sync method
    /// (`push_user`, `usage`, …) would re-lock the very mutex its own run
    /// holds: a guaranteed self-deadlock on `std::sync::Mutex`. The flag
    /// converts that into an immediate, explanatory RuntimeError.
    static IN_RUN: Cell<bool> = const { Cell::new(false) };
}

/// RAII clear for [`IN_RUN`] — also on panic unwind, so the thread stays
/// usable after a failed run.
struct RunFlag;

impl Drop for RunFlag {
    fn drop(&mut self) {
        IN_RUN.with(|c| c.set(false));
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
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
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
            def: ta::ToolDef {
                name,
                description,
                schema,
            },
            callback,
        })
    }
}

// ── Hooks: the interception layer as a duck-typed Python object ─────────────

/// The execution context projected to `before_tool` / `after_tool`. Deliberate
/// minimal surface: what a policy needs for attribution and path decisions —
/// the full `ToolCtx` carries agent plumbing (cancel flag, budget gate, the
/// hooks Arc itself) a Python host must not touch.
#[derive(serde::Serialize)]
struct HookCtx<'a> {
    workdir: &'a str,
    /// `None` on top-level calls; the `task` child's name (or `"__code__"`
    /// for calls originating inside the run_code sandbox).
    subagent: Option<&'a str>,
}

/// The finished-call view projected to `after_tool`. `image` / `usage` /
/// `usage_in_shared_budget` stay binding-internal: usage surfaces through the
/// `Usage` event, an image blob has no host-policy meaning, and the budget
/// flag is core accounting.
#[derive(serde::Serialize)]
struct HookToolOutput<'a> {
    content: &'a str,
    is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<&'a serde_json::Value>,
}

/// The outgoing-request view projected to `on_chat_request` — same shape the
/// custom provider receives (host-relevant conversation + params), with the
/// agent-internal plumbing (`cancel`, `per_request_timeout`) withheld.
#[derive(serde::Serialize)]
struct HookChatRequest<'a> {
    system: Option<&'a str>,
    messages: &'a [ta::Message],
    tools: &'a [ta::ToolDef],
    model: &'a str,
    max_tokens: u32,
    cache: bool,
}

/// Look up an optional duck-typed hook method. AttributeError (i.e. the host
/// object doesn't define it) → None = core's default no-op; any other lookup
/// error is a real problem and surfaces as a constructor error.
fn optional_method(obj: &Bound<'_, PyAny>, name: &str) -> PyResult<Option<PyObject>> {
    match obj.getattr(name) {
        Ok(m) => Ok(Some(m.unbind())),
        Err(e) => {
            if e.is_instance_of::<pyo3::exceptions::PyAttributeError>(obj.py()) {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// A host hooks object wrapped into [`ta::Hooks`]. Methods are resolved at
/// `set_hooks`/construction time (getattr once, not per call — per-call
/// getattr would silently swallow typos as no-ops).
struct PyHooks {
    before_tool: Option<PyObject>,
    after_tool: Option<PyObject>,
    on_chat_request: Option<PyObject>,
}

impl PyHooks {
    fn from_obj(obj: Py<PyAny>, py: Python<'_>) -> PyResult<Self> {
        let b = obj.bind(py);
        Ok(Self {
            before_tool: optional_method(b, "before_tool")?,
            after_tool: optional_method(b, "after_tool")?,
            on_chat_request: optional_method(b, "on_chat_request")?,
        })
    }
}

/// Parse a `before_tool` return value into a [`ta::ToolDecision`]:
/// `None` → Allow; a `str` → Deny(message) (the model sees it as the
/// tool_result); a single-key dict `{"deny": msg}` or `{"modify": input}`
/// → the matching decision. Anything else is an error → the caller
/// fail-closes with a Deny that names the problem.
fn decision_from_py(bound: &Bound<'_, PyAny>) -> anyhow::Result<ta::ToolDecision> {
    if bound.is_none() {
        return Ok(ta::ToolDecision::Allow);
    }
    if let Ok(s) = bound.extract::<String>() {
        return Ok(ta::ToolDecision::Deny(s));
    }
    let v: serde_json::Value = depythonize(bound)
        .map_err(|e| anyhow::anyhow!("expected None, a deny string, or {{deny|modify: …}}: {e}"))?;
    match v {
        serde_json::Value::Object(ref m) if m.len() == 1 => {
            if let Some(serde_json::Value::String(d)) = m.get("deny") {
                Ok(ta::ToolDecision::Deny(d.clone()))
            } else if let Some(modv) = m.get("modify") {
                Ok(ta::ToolDecision::Modify(modv.clone()))
            } else {
                Err(anyhow::anyhow!(
                    "single-key dict must use 'deny' (string) or 'modify' (input)"
                ))
            }
        }
        _ => Err(anyhow::anyhow!(
            "expected None, a deny string, or a single-key {{'deny': …}} / {{'modify': …}} dict"
        )),
    }
}

/// The fail-closed deny for a misbehaving gate: a hook that raises or returns
/// garbage must not silently degrade into "allow everything" — the tool call
/// is refused with a message the model sees (hooks doc: return Deny on bad
/// input rather than panicking; the binding enforces that for the host).
fn deny_fail_closed(tool: &str, why: String) -> ta::ToolDecision {
    ta::ToolDecision::Deny(format!(
        "[hooks] before_tool('{tool}') refused the call: {why}"
    ))
}

impl ta::Hooks for PyHooks {
    fn before_tool(&self, tool: &str, input: &Value, ctx: &ta::ToolCtx) -> ta::ToolDecision {
        let Some(cb) = self.before_tool.as_ref() else {
            return ta::ToolDecision::Allow;
        };
        Python::with_gil(|py| {
            let proj = HookCtx {
                workdir: &ctx.workdir.to_string_lossy(),
                subagent: ctx.subagent.as_deref(),
            };
            let Ok(py_ctx) = pythonize(py, &proj) else {
                return deny_fail_closed(tool, "ctx serialization failed".into());
            };
            let Ok(py_in) = pythonize(py, input) else {
                return deny_fail_closed(tool, "input serialization failed".into());
            };
            match cb.call1(py, (tool, py_in, py_ctx)) {
                Ok(ret) => decision_from_py(ret.bind(py))
                    .unwrap_or_else(|e| deny_fail_closed(tool, e.to_string())),
                Err(e) => deny_fail_closed(tool, format!("hook raised: {e}")),
            }
        })
    }

    fn after_tool(
        &self,
        tool: &str,
        input: &Value,
        output: &ta::ToolOutput,
        executed: bool,
        ctx: &ta::ToolCtx,
    ) {
        let Some(cb) = self.after_tool.as_ref() else {
            return;
        };
        // Observe-only: a buggy after_tool must not poison the run (same
        // swallow contract as the on_event callback).
        let _ = Python::with_gil(|py| -> pyo3::PyResult<()> {
            let proj = HookCtx {
                workdir: &ctx.workdir.to_string_lossy(),
                subagent: ctx.subagent.as_deref(),
            };
            let out = HookToolOutput {
                content: &output.content,
                is_error: output.is_error,
                meta: output.meta.as_ref(),
            };
            cb.call1(
                py,
                (
                    tool,
                    pythonize(py, input)?,
                    pythonize(py, &out)?,
                    executed,
                    pythonize(py, &proj)?,
                ),
            )?;
            Ok(())
        });
    }

    fn on_chat_request(&self, req: &ta::ChatRequest) {
        let Some(cb) = self.on_chat_request.as_ref() else {
            return;
        };
        let _ = Python::with_gil(|py| -> pyo3::PyResult<()> {
            let proj = HookChatRequest {
                system: req.system,
                messages: req.messages,
                tools: req.tools,
                model: req.model,
                max_tokens: req.max_tokens,
                cache: req.cache,
            };
            cb.call1(py, (pythonize(py, &proj)?,))?;
            Ok(())
        });
    }
}

/// Validate + install a host hooks object on the freshly built agent (shared
/// by `Agent.__init__` and `chat()`).
fn install_hooks(agent: &mut ta::Agent, hooks: Option<Py<PyAny>>, py: Python<'_>) -> PyResult<()> {
    if let Some(obj) = hooks {
        let h = PyHooks::from_obj(obj, py)?;
        agent.set_hooks(std::sync::Arc::new(h));
    }
    Ok(())
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
        return Ok(ta::SimpleChatResponse {
            text: s,
            stop_reason: None,
            usage: None,
        });
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

impl Agent {
    /// Shared body of `enable_code` / `disable_code` (same cfg split, so the
    /// feature-missing error text lives in exactly one place).
    fn code_switch(&self, py: Python<'_>, enabled: bool) -> PyResult<()> {
        #[cfg(feature = "code")]
        {
            self.with_agent(py, |a| a.set_code_enabled(enabled))
                .and_then(|r| r.map_err(to_pyerr))
        }
        #[cfg(not(feature = "code"))]
        {
            let _ = (py, enabled);
            Err(code_feature_missing())
        }
    }

    /// Run `f` on the inner agent with (a) the same-thread reentry guard and
    /// (b) the GIL released while blocked on the lock.
    ///
    /// (b) is the deadlock fix: a thread waiting on the mutex while HOLDING
    /// the GIL deadlocks against the running agent's `with_gil` event
    /// callbacks (classic GIL↔mutex ABBA) — with the GIL released around the
    /// wait, the run thread's callbacks proceed and this thread proceeds once
    /// the run finishes. `MutexGuard` is `!Send` so it can't cross out of
    /// `allow_threads`; the whole lock+work happens inside, which also forces
    /// `f` to be Python-free (it runs GIL-less) — every caller only
    /// reads/copies Rust data.
    fn with_agent<T: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut ta::Agent) -> T + Send,
    ) -> PyResult<T> {
        if IN_RUN.with(Cell::get) {
            return Err(PyRuntimeError::new_err(
                "Agent is running on this thread: calling Agent methods from an \
                 on_event/tool callback during run() would deadlock (the run \
                 already holds the agent lock). Collect what you need in the \
                 callback and call Agent methods after run() returns.",
            ));
        }
        Ok(py.allow_threads(|| {
            let mut guard = self.inner.lock().expect("agent mutex poisoned");
            f(&mut guard)
        }))
    }
}

#[pymethods]
impl Agent {
    #[new]
    #[pyo3(signature = (config, *, tools=None, provider=None, api_key=None, hooks=None))]
    fn new(
        config: &Bound<'_, PyAny>,
        tools: Option<Vec<Py<PyTool>>>,
        provider: Option<Py<PyAny>>,
        api_key: Option<String>,
        hooks: Option<Py<PyAny>>,
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
        let mut agent = match provider {
            Some(chat_fn) => {
                let prov: Box<dyn ta::Provider> = Box::new(PyProvider { chat_fn });
                ta::build_agent_with_provider(&cfg, prov, Vec::new(), &client, extra, &api_key)
            }
            None => ta::build_agent_with(&cfg, Vec::new(), &client, extra, &api_key),
        }
        .map_err(to_pyerr)?;
        install_hooks(&mut agent, hooks, py)?;
        Ok(Self {
            inner: Mutex::new(agent),
        })
    }

    /// Install the interception layer from a Python object with any of
    /// `before_tool(tool, input, ctx)` / `after_tool(tool, input, output,
    /// executed, ctx)` / `on_chat_request(request)` — missing methods are
    /// no-ops. `before_tool` returns `None` (allow), a deny string (the model
    /// sees it as the error tool_result), or `{"modify": input}` (run with
    /// replaced input; the audit keeps the original). A raising or
    /// ill-formed `before_tool` FAILS CLOSED (the call is denied, with the
    /// reason in the message); the observe-only hooks swallow errors.
    /// Call between runs (same busy contract as the other sync methods).
    /// `task` sub-agents inherit the layer; attribution arrives via
    /// `ctx["subagent"]` (`None` = top-level).
    fn set_hooks(&self, hooks: Py<PyAny>, py: Python<'_>) -> PyResult<()> {
        // Resolve + validate the methods with the GIL held, then install
        // lock-only (from_obj / set_hooks touch no Python).
        let h = PyHooks::from_obj(hooks, py)?;
        self.with_agent(py, |a| a.set_hooks(std::sync::Arc::new(h)))
    }

    /// Append a user message (text) to the conversation, then `.run()`.
    fn push_user(&self, text: &str, py: Python<'_>) -> PyResult<()> {
        // The returned session-save warning is dropped (same as before the
        // lock-model fix — the headless binary surfaces it, bindings don't).
        self.with_agent(py, |a| {
            a.push_user(vec![ta::Block::Text(text.into())]);
        })
    }

    /// Arm a `CancelToken`; its flag interrupts the next turn boundary.
    /// Contract (kernel, TinyWorker): the flag is level-triggered and `run()`
    /// NEVER clears it — after a cancelled run, call `clear_cancel()` at the
    /// point you accept the next task (e.g. right before `push_user`/`run` of
    /// a new message) or every following run returns instantly cancelled.
    fn set_cancel(&self, token: &CancelToken, py: Python<'_>) -> PyResult<()> {
        self.with_agent(py, |a| a.set_cancel(token.flag.clone()))
    }

    /// Clear the cancel flag — the acceptance-point half of the cancel
    /// contract (see `set_cancel`). Call this when accepting a NEW task, not
    /// during a run (a stop signalled mid-run must land).
    fn clear_cancel(&self, py: Python<'_>) -> PyResult<()> {
        self.with_agent(py, |a| a.clear_cancel())
    }

    /// Run one turn (until end_turn / max_turns / cancel / error). `on_event`
    /// is called for each `AgentEvent` (text deltas, tool calls, usage, …) with
    /// the event as a dict. Returns the `RunOutcome` as a dict
    /// (final_text, turns, stop_reason, tool_calls, *_tokens, elapsed).
    ///
    /// The lock is acquired and the whole run happens with the GIL released;
    /// callbacks re-acquire it per call (`emit_event` / tool closures). A
    /// same-thread reentrant `run()` (from inside a callback) is a
    /// RuntimeError, not a self-deadlock.
    #[pyo3(signature = (on_event=None))]
    fn run(&self, on_event: Option<PyObject>, py: Python<'_>) -> PyResult<PyObject> {
        if IN_RUN.with(Cell::get) {
            return Err(PyRuntimeError::new_err(
                "Agent.run() cannot be called reentrantly from an on_event/tool callback",
            ));
        }
        let result = py.allow_threads(|| {
            let mut agent = self.inner.lock().expect("agent mutex poisoned");
            IN_RUN.with(|c| c.set(true));
            let _flag = RunFlag; // RAII clear, panic-safe
            agent.run(&mut |ev| emit_event(&on_event, ev))
        });
        outcome_to_py(result, py)
    }

    /// Cumulative token usage of the current/latest run (4 axes).
    fn usage(&self, py: Python<'_>) -> PyResult<PyObject> {
        let u = self.with_agent(py, |a| a.usage())?;
        let bound = pythonize(py, &u)?;
        Ok(bound.unbind())
    }

    /// Estimated token composition of the NEXT request's context:
    /// `{"system": {"env", "base", "skill_intro", "tokens"},
    ///    "tools": {"builtin"/"mcp"/"skill"/"custom": {"tokens", "tools":
    ///      [{"name", "tokens"}]}, "tokens"},
    ///    "messages": {"tokens", "by_kind": {"user_text", …}},
    ///    "total_tokens", "calibration": {"measured_prompt_tokens", "scale"}
    ///      | None}`. Every number is an ESTIMATE (chars/4 yardstick), not a
    /// provider measurement — `usage()` stays the billing authority;
    /// `calibration` is non-None only while a measured anchor is still valid.
    /// Read-only; calling during `run()` waits until the run ends (other
    /// thread) or errors on same-thread reentry (same as `usage`).
    fn context_breakdown(&self, py: Python<'_>) -> PyResult<PyObject> {
        let bd = self.with_agent(py, |a| a.context_breakdown())?;
        let bound = pythonize(py, &bd)?;
        Ok(bound.unbind())
    }

    /// Manually compress the conversation history NOW (the idle-time fold):
    /// summarize the middle via the LLM, fold it into the first message,
    /// rewrite the session file. `instructions` is an optional focus hint for
    /// the summarizer ("focus on the API changes"); `on_event` receives the
    /// `Compressed`/`Warning` events. Returns the outcome dict
    /// `{dropped, kept, usage}` or `None` when nothing was compressible
    /// (history too short); raises on summary-request failure. Call between
    /// runs (same reentrancy rules as `run`). The summary's cost is NOT part
    /// of the next run's `usage()` — meter it from the returned `usage`.
    #[pyo3(signature = (instructions=None, on_event=None))]
    fn compact(
        &self,
        instructions: Option<&str>,
        on_event: Option<PyObject>,
        py: Python<'_>,
    ) -> PyResult<PyObject> {
        if IN_RUN.with(Cell::get) {
            return Err(PyRuntimeError::new_err(
                "Agent.compact() cannot be called reentrantly from an on_event/tool callback",
            ));
        }
        let result = py.allow_threads(|| {
            let mut agent = self.inner.lock().expect("agent mutex poisoned");
            IN_RUN.with(|c| c.set(true));
            let _flag = RunFlag; // RAII clear, panic-safe
            agent.compact_with(&mut |ev| emit_event(&on_event, ev), instructions)
        });
        match result {
            Ok(Some(outcome)) => {
                let bound = pythonize(py, &outcome)?;
                Ok(bound.unbind())
            }
            Ok(None) => Ok(py.None()),
            Err(e) => {
                // Same honest-cost envelope as the run path (outcome_to_py):
                // the summary cost a failed compact burned rides the error
                // chain (CompressError::burned) instead of hitting the floor.
                // JSON-string message — callers `json.loads(e.args[0])` and
                // meter .burned exactly like a failed run's .usage. `burned`
                // is null when the failure carried no usage (e.g. a host
                // provider callback that simply raised).
                let payload = serde_json::json!({
                    "error": e.to_string(),
                    "burned": e
                        .downcast_ref::<ta::CompressError>()
                        .and_then(|ce| ce.burned),
                });
                Err(PyRuntimeError::new_err(payload.to_string()))
            }
        }
    }

    /// Conversation history as a list of message dicts.
    fn messages(&self, py: Python<'_>) -> PyResult<PyObject> {
        let msgs = self.with_agent(py, |a| a.messages().to_vec())?;
        let bound = pythonize(py, &msgs)?;
        Ok(bound.unbind())
    }

    /// The model name this agent sends with each request.
    fn model(&self, py: Python<'_>) -> PyResult<String> {
        self.with_agent(py, |a| a.model().to_string())
    }

    /// Enable the sandboxed `run_code` tool for the NEXT request (runtime
    /// switch — the tool becomes visible to the model; zero schema cost
    /// while disabled). Same busy contract as the other sync methods:
    /// calling during `run()` deadlocks-waits (other thread) or errors
    /// (same thread).
    ///
    /// Requires the module built with its `code` cargo feature
    /// (`maturin develop --features code`); otherwise a clear error.
    fn enable_code(&self, py: Python<'_>) -> PyResult<()> {
        self.code_switch(py, true)
    }

    /// Remove `run_code` from the model's toolset (runtime switch). Does not
    /// interrupt an in-flight execution — the next request just stops
    /// offering the tool. Same feature requirement as `enable_code`.
    fn disable_code(&self, py: Python<'_>) -> PyResult<()> {
        self.code_switch(py, false)
    }

    /// Atomically replace the WHOLE code config (switch + every limit / path
    /// knob) from a dict shaped like the config file's `"code"` section:
    /// unknown keys error, missing keys reset to their DEFAULTS (they do not
    /// retain previous values — send the full dict to keep a knob). Takes
    /// effect for the next execution; an in-flight one keeps its snapshot.
    ///
    /// The `llm` sub-object is pinned by the sandbox registry at its first
    /// execution: after that, changing `llm` here still succeeds but cannot
    /// reach guest `llm` calls, and the kernel emits a diag warning telling
    /// the host to rebuild the agent. Limits / paths / `enabled` stay hot.
    fn set_code_config(&self, cfg: &Bound<'_, PyAny>, py: Python<'_>) -> PyResult<()> {
        #[cfg(feature = "code")]
        {
            let parsed: ta::CodeConfig = depythonize(cfg)
                .map_err(|e| anyhow::anyhow!("code config dict: {e}"))
                .map_err(to_pyerr)?;
            self.with_agent(py, |a| a.set_code_config(parsed))
                .and_then(|r| r.map_err(to_pyerr))
        }
        #[cfg(not(feature = "code"))]
        {
            let _ = (cfg, py);
            Err(code_feature_missing())
        }
    }
}

// ── chat(): one-shot convenience ─────────────────────────────────────────────

/// One-shot: build an agent from `config`, send `message`, run to completion,
/// and return the `RunOutcome` dict. Optional `on_event` streams events.
#[pyfunction]
#[pyo3(signature = (config, message, *, on_event=None, tools=None, provider=None, api_key=None, hooks=None))]
#[allow(clippy::too_many_arguments)] // flat kwarg surface mirrors the other entry points
fn chat(
    config: &Bound<'_, PyAny>,
    message: &str,
    on_event: Option<PyObject>,
    tools: Option<Vec<Py<PyTool>>>,
    provider: Option<Py<PyAny>>,
    api_key: Option<String>,
    hooks: Option<Py<PyAny>>,
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
    install_hooks(&mut agent, hooks, py)?;
    agent.push_user(vec![ta::Block::Text(message.into())]);
    // Local agent (no shared mutex): the run itself still releases the GIL so
    // other Python threads progress during the network-bound loop.
    let result = py.allow_threads(|| agent.run(&mut |ev| emit_event(&on_event, ev)));
    outcome_to_py(result, py)
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

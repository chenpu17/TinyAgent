//! `run_code`: LLM-written JavaScript executed in an in-process wasm sandbox.
//!
//! The guest is quickjs-ng compiled to `wasm32-wasi` WITHOUT quickjs-libc and
//! with every WASI symbol stubbed at link time, so the module's ONLY imports
//! are `env.host_call_tool` / `env.host_log` — capability surface is the two
//! functions, nothing ambient (docs/run_code_design.md §5). The host embeds it
//! via wasmtime: one Engine/Module/Linker per process, one Store per tool
//! execution, fuel + linear-memory caps per execution (§6).
//!
//! Guest↔host contract (§5.1): `alloc`/`dealloc` for buffer exchange,
//! `eval_code(code)` runs the code as a function body (top-level `return`
//! works), `take_result`/`take_last_error` hand back packed
//! `(ptr << 32) | len` slots. Each inner tool call goes through the SAME
//! gate → run → after chain as model-direct calls (FR-3): the hooks gate
//! fires first (unknown names included — matching the top-level order), then
//! the deny_paths / fs_write_dirs interception layer judges the
//! post-`Modify` EFFECTIVE input (§7), then the run. The ToolCall event is
//! emitted before the gate and the ToolResult right after the call returns
//! (H1: result emission belongs to the caller; the outer loop only assembles
//! results for ITS calls). Every call lands in the `meta` audit trace with
//! dual truncated-sha256 input hashes (§8.4) — original vs. effective — plus
//! call count / limit / fuel usage.
//!
//! The embedded asset is built by `examples/build-quickjs.sh` (source
//! provenance + sha256 recorded there). Everything through milestone M3 is
//! live: registry (whitelist + shared MCP tools), interception layer, audit
//! meta, call limit, the log cap + CodeLog events, the enable/config switch
//! face (§8.6), and the epoch-delivered wall clock / cancel.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::Digest;
use wasmtime::{Caller, Engine, Extern, Linker, Memory, Module, ResourceLimiter, Store, TypedFunc};

use crate::agent::{ctx_cancelled, gate_tool_call, notify_tool_after, run_tool, AgentEvent};
use crate::config::{CodeConfig, Config};
use crate::hooks::ToolDecision;
use crate::provider::{ToolDef, Usage};
use crate::tools::{mcp, SharedTool, Tool, ToolConcurrency, ToolCtx, ToolOrigin, ToolOutput};

// ── module layout ──────────────────────────────────────────────────────────
//
// Move-only split of the former single ~12k-line file: this file stays the
// module ROOT (docs + imports + facade); each sibling submodule owns one
// cohesive concern. Items are glob-re-imported below so submodule code keeps
// referring to each other by bare name, exactly as the single file did:
//
//   contract   guest-visible contract text: reserved-name constants, prelude
//              sources, the embedded skill md, description overlays
//   sandbox    wasmtime engine/module/linker, HostCtx, memory limiter, wall clock
//   guest_api  env.host_call_tool dispatch, reserved-name routing, envelopes,
//              guest memory I/O, audit folds (llm-primitive tests included)
//   fs_policy  deny_paths / fs_write_dirs interception, path normalization, glob
//   lib_index  the lib family: INDEX.md grammar, admission, saveLib host halves
//   batch      callBatch: __call_batch dispatch, wave engine, output admission
//   precheck   FR-2 static precheck (JS lexer + rejection copy)
//   tool       RunCodeTool and its Tool impl
//   test_util  shared test helpers (cfg(test))

mod batch;
mod contract;
mod fs_policy;
mod guest_api;
mod lib_index;
mod precheck;
mod sandbox;
mod tool;

#[cfg(test)]
mod test_util;

// Private imports: pull every submodule's items into THIS module's namespace
// so the submodules (each doing `use super::*`) see each other's items — the
// pre-split single-file name resolution, one level deeper. `contract` and
// `tool` are named lists / test-gated globs: their facade re-exports below
// must stay the ONLY prod bindings of those names (a glob alongside a named
// re-export of the same item makes the re-export look unused to rustc).
use batch::*;
use contract::{
    prelude_line, BATCH_DESC_SUFFIX, CODE_MARKER, GUEST_OUTPUT_CHARS, LIB_DESC_SUFFIX,
    LLM_DESC_SUFFIX, MAX_CODE_BYTES,
};
use fs_policy::*;
use guest_api::*;
use lib_index::*;
use precheck::*;
use sandbox::*;

#[cfg(test)]
use test_util::*;

// Facade: crate-facing paths that predate the split keep resolving unchanged.
pub use contract::run_code_skill_md;
pub use tool::RunCodeTool;
// The MD constant's only in-crate path consumer (skill.rs's byte-identity
// test) is itself `#[cfg(feature = "code")] #[test]` — in a plain lib build
// the re-export has no users, which the lint would flag.
#[allow(unused_imports)]
pub(crate) use contract::RUN_CODE_SKILL_MD;

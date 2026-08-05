'use strict';

// User-facing JS layer over the raw napi addon (./index.js). It hides the
// custom-tool round-trip ceremony: the native side hands each JS tool callback
// a `(err, req)` CalleeHandled TSFN call and expects it to call back
// `resolveTool(req.id, result)`; here we wrap the user's idiomatic
// `run(args)` into that shape, so users just write `run(args) => 'result'`.
//
// Also presents an options-object API (new Agent(path, { tools, apiKey }) and
// chat(path, message, { onEvent, tools, apiKey })) instead of the raw positional
// one.

const native = require('./index.js');

/** Normalize a tool callback's return into { content, isError }: a plain string
 *  is shorthand for { content: str }; an object carries content + isError. */
function normalizeResult(r) {
  if (r == null) return { content: '', isError: false };
  if (typeof r === 'string') return { content: r, isError: false };
  return {
    content: r.content == null ? '' : String(r.content),
    isError: !!r.isError,
  };
}

/** Wrap each user `run(args)` into the TSFN callback shape the native side
 *  expects: receives (err, req) — err is always null since Rust only calls
 *  `Ok(req)` — runs the user fn on req.args, then fulfills the round-trip via
 *  resolveTool(req.id, result). Errors in the user fn become error tool
 *  results rather than crashing the agent. */
function wrapTools(tools) {
  if (!Array.isArray(tools)) return undefined;
  return tools.map((t) => ({
    name: t.name,
    description: t.description,
    schema: t.schema,
    run: async function (_err, req) {
      try {
        const r = await t.run(req.args);
        native.resolveTool(req.id, normalizeResult(r));
      } catch (e) {
        native.resolveTool(req.id, {
          content: String((e && e.message) || e),
          isError: true,
        });
      }
    },
  }));
}

class Agent extends native.Agent {
  constructor(configPath, opts = {}) {
    super(configPath, wrapTools(opts.tools), opts.apiKey);
  }
}

async function chat(configPath, message, opts = {}) {
  return native.chat(configPath, message, opts.onEvent, wrapTools(opts.tools), opts.apiKey);
}

module.exports = {
  Agent,
  chat,
  CancelToken: native.CancelToken,
};

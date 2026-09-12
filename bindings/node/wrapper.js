'use strict';

// User-facing JS layer over the raw napi addon (./index.js). It hides the
// custom-tool AND custom-provider round-trip ceremony:
//
//   tool:     native hands the wrapped JS `run` a `(err, req)` CalleeHandled
//             TSFN call and expects `resolveTool(req.id, result)` back; here we
//             wrap the user's idiomatic `run(args)` into that shape.
//   provider: same pattern — native hands the wrapped provider `(err, call)`
//             and expects `resolveProvider(call.id, result)` back; here we wrap
//             the user's `provider(request)` (a function or `{ chat }` object)
//             into that shape.
//
// Also presents an options-object API (new Agent(path, { tools, provider,
// apiKey }) and chat(path, message, { onEvent, tools, provider, apiKey }))
// instead of the raw positional one.

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

/** Normalize a custom provider's return into the SimpleChatResponse shape the
 *  Rust side parses: a plain string is shorthand for { text: str }; an object
 *  carries text + optional stop_reason / usage. */
function normalizeProviderResult(r) {
  if (r == null) return { text: '' };
  if (typeof r === 'string') return { text: r };
  return {
    text: r.text == null ? '' : String(r.text),
    stop_reason: r.stop_reason == null ? undefined : String(r.stop_reason),
    usage: r.usage == null ? undefined : r.usage,
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

/** Wrap the user's provider (a function or `{ chat }` object) into the TSFN
 *  callback shape the native side expects: `{ chat: async (err, call) => … }`.
 *  Runs the user fn on call.request, then fulfills via
 *  resolveProvider(call.id, result). A thrown provider resolves with a
 *  `{__error}` marker so the run fails honestly instead of hanging on the
 *  120s timeout (an unhandled TSFN promise rejection can't reach Rust). */
function wrapProvider(provider) {
  if (provider == null) return undefined;
  const fn = typeof provider === 'function' ? provider : provider.chat;
  if (typeof fn !== 'function') {
    throw new TypeError("provider must be a function or an object with a 'chat' method");
  }
  return {
    chat: async function (_err, call) {
      try {
        const r = await fn(call.request);
        native.resolveProvider(call.id, normalizeProviderResult(r));
      } catch (e) {
        native.resolveProvider(call.id, { __error: String((e && e.message) || e) });
      }
    },
  };
}

/** Wrap the user's onEvent callback so a throw can't kill the process. The
 *  native side uses ErrorStrategy::Fatal for event TSFNs (the event is
 *  fire-and-forget — there's no Rust side to hand an error to), so an uncaught
 *  throw in the callback escalates to napi_fatal_exception: an UNCATCHABLE
 *  process exit. Swallow instead (log to stderr), matching the Python
 *  binding's semantics (a buggy onEvent must not poison the run) and the
 *  tools/provider wrappers above. Covers sync throws and async rejections. */
function wrapOnEvent(onEvent) {
  if (typeof onEvent !== 'function') return undefined;
  return function onNativeEvent(ev) {
    try {
      Promise.resolve(onEvent(ev)).catch((e) => {
        console.error('[tiny-agent] onEvent callback threw (ignored):', e);
      });
    } catch (e) {
      console.error('[tiny-agent] onEvent callback threw (ignored):', e);
    }
  };
}

class Agent extends native.Agent {
  constructor(configPath, opts = {}) {
    super(configPath, wrapTools(opts.tools), wrapProvider(opts.provider), opts.apiKey);
  }

  run(onEvent) {
    return super.run(wrapOnEvent(onEvent));
  }

  // compact takes (instructions, onEvent) natively. WITHOUT this override the
  // user's onEvent reached the event TSFN raw — and that TSFN is
  // ErrorStrategy::Fatal, so a throw in the callback was an UNCATCHABLE
  // process abort, the exact failure wrapOnEvent exists to prevent (#5 of
  // the 2026-09 review: run was wrapped, compact missed).
  compact(instructions, onEvent) {
    return super.compact(instructions ?? null, wrapOnEvent(onEvent));
  }
}

async function chat(configPath, message, opts = {}) {
  return native.chat(
    configPath,
    message,
    wrapOnEvent(opts.onEvent),
    wrapTools(opts.tools),
    wrapProvider(opts.provider),
    opts.apiKey,
  );
}

module.exports = {
  Agent,
  chat,
  CancelToken: native.CancelToken,
};

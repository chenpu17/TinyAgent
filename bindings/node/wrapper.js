'use strict';

// User-facing JS layer over the raw napi addon (./index.js). It hides the
// custom-tool, custom-provider, AND hooks round-trip ceremony:
//
//   tool:     native hands the wrapped JS `run` a `(err, req)` CalleeHandled
//             TSFN call and expects `resolveTool(req.id, result)` back; here we
//             wrap the user's idiomatic `run(args)` into that shape.
//   provider: same pattern — native hands the wrapped provider `(err, call)`
//             and expects `resolveProvider(call.id, result)` back; here we wrap
//             the user's `provider(request)` (a function or `{ chat }` object)
//             into that shape.
//   hooks:    beforeTool round-trips the same way (resolveHooksDecision);
//             afterTool / onChatRequest are fire-and-forget and get the
//             swallow guard wrapOnEvent uses (their TSFNs are Fatal strategy).
//
// Also presents an options-object API (new Agent(path, { tools, provider,
// hooks, apiKey }) and chat(path, message, { onEvent, tools, provider, hooks,
// apiKey })) instead of the raw positional one.

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

/** Wrap the user's duck-typed hooks object into the TSFN callback shapes the
 *  native side expects. Method resolution happens HERE (once, at install
 *  time) — a hooks object without a method simply omits it (no-op), matching
 *  the Python binding.
 *
 *  beforeTool must return a decision, so it round-trips: native hands us
 *  `(err, call)` with { id, tool, input, ctx } and blocks; we run the user's
 *  fn, normalize the decision (undefined → null = allow; string = deny;
 *  {deny|modify: …}), and fulfill via resolveHooksDecision(call.id, result).
 *  A user fn that THROWS must still resolve the channel — it resolves with a
 *  deny carrying the error (fail-closed), so a broken policy can't hang the
 *  worker for 120s; the native side fail-closes the remaining paths (timeout,
 *  ill-formed decision) itself.
 *
 *  afterTool / onChatRequest are observe-only: the native TSFNs are
 *  ErrorStrategy::Fatal, so a raw throw would be an UNCATCHABLE process
 *  abort — same guard as wrapOnEvent (swallow + stderr). */
function wrapHooks(hooks) {
  if (hooks == null) return undefined;
  const out = {};
  if (typeof hooks.beforeTool === 'function') {
    out.beforeTool = async function (_err, call) {
      let result;
      try {
        result = await hooks.beforeTool(call.tool, call.input, call.ctx);
        if (result === undefined) result = null;
      } catch (e) {
        result = {
          deny: `[hooks] beforeTool('${call.tool}') refused the call: hook threw ${
            String((e && e.message) || e)
          }`,
        };
      }
      native.resolveHooksDecision(call.id, result);
    };
  }
  // NB: the observe-only TSFNs are ErrorStrategy::Fatal — napi invokes them
  // with the payload as the SINGLE argument (no leading `err`, unlike the
  // CalleeHandled tool/provider/gate callbacks).
  if (typeof hooks.afterTool === 'function') {
    const fn = hooks.afterTool;
    out.afterTool = function (payload) {
      try {
        Promise.resolve(fn(payload)).catch((e) => {
          console.error('[tiny-agent] afterTool hook threw (ignored):', e);
        });
      } catch (e) {
        console.error('[tiny-agent] afterTool hook threw (ignored):', e);
      }
    };
  }
  if (typeof hooks.onChatRequest === 'function') {
    const fn = hooks.onChatRequest;
    out.onChatRequest = function (payload) {
      try {
        Promise.resolve(fn(payload)).catch((e) => {
          console.error('[tiny-agent] onChatRequest hook threw (ignored):', e);
        });
      } catch (e) {
        console.error('[tiny-agent] onChatRequest hook threw (ignored):', e);
      }
    };
  }
  return out;
}

class Agent extends native.Agent {
  constructor(configPath, opts = {}) {
    super(
      configPath,
      wrapTools(opts.tools),
      wrapProvider(opts.provider),
      opts.apiKey,
      wrapHooks(opts.hooks),
    );
  }

  // The native setHooks takes the (already wrapped) hooks object; the TSFNs
  // must be created on the JS thread, which this override guarantees before
  // native touches the agent lock.
  setHooks(hooks) {
    return super.setHooks(wrapHooks(hooks));
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
    wrapHooks(opts.hooks),
  );
}

module.exports = {
  Agent,
  chat,
  CancelToken: native.CancelToken,
};

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
 *  results rather than crashing the agent.
 *
 *  Shape validation happens HERE, synchronously. Passing the array through
 *  unchecked used to fail three different ways: a non-array silently disabled
 *  every custom tool, a non-function `run` killed the process inside the TSFN
 *  constructor (napi assert, uncatchable), and a non-object `schema` was sent
 *  to the provider verbatim. One clear TypeError at call time is the contract. */
function wrapTools(tools) {
  if (tools == null) return undefined;
  if (!Array.isArray(tools)) {
    throw new TypeError('tools must be an array of { name, description, schema, run }');
  }
  return tools.map((t, i) => {
    if (t == null || typeof t !== 'object' || Array.isArray(t)) {
      throw new TypeError(`tools[${i}] must be an object`);
    }
    if (typeof t.name !== 'string' || t.name === '') {
      throw new TypeError(`tools[${i}].name must be a non-empty string`);
    }
    if (typeof t.description !== 'string') {
      throw new TypeError(`tools[${i}].description must be a string`);
    }
    if (t.schema == null || typeof t.schema !== 'object' || Array.isArray(t.schema)) {
      throw new TypeError(`tools[${i}].schema must be a JSON Schema object`);
    }
    if (typeof t.run !== 'function') {
      throw new TypeError(`tools[${i}].run must be a function`);
    }
    return {
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
    };
  });
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
 *  A PRESENT method that is not a function is rejected with a TypeError: the
 *  native TSFN constructor asserts on a non-function and aborts the process
 *  (uncatchable), so silently dropping the property would both hide the typo
 *  and diverge from the native contract. `null` / `undefined` count as absent.
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
  if (typeof hooks !== 'object') {
    throw new TypeError(
      'hooks must be an object with any of beforeTool / afterTool / onChatRequest',
    );
  }
  const method = (name) => {
    const fn = hooks[name];
    if (fn == null) return undefined;
    if (typeof fn !== 'function') {
      throw new TypeError(`hooks.${name} must be a function when present`);
    }
    return fn;
  };
  const beforeTool = method('beforeTool');
  const afterTool = method('afterTool');
  const onChatRequest = method('onChatRequest');
  const out = {};
  if (beforeTool) {
    out.beforeTool = async function (_err, call) {
      let result;
      try {
        result = await beforeTool(call.tool, call.input, call.ctx);
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
  if (afterTool) {
    out.afterTool = function (payload) {
      try {
        Promise.resolve(afterTool(payload)).catch((e) => {
          console.error('[tiny-agent] afterTool hook threw (ignored):', e);
        });
      } catch (e) {
        console.error('[tiny-agent] afterTool hook threw (ignored):', e);
      }
    };
  }
  if (onChatRequest) {
    out.onChatRequest = function (payload) {
      try {
        Promise.resolve(onChatRequest(payload)).catch((e) => {
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

  // Async constructor. Same work as `new Agent(...)`, but native does the
  // config load + tool assembly (spawning and handshaking every MCP server —
  // seconds with a slow one) on a worker thread, so the JS event loop keeps
  // ticking. The worker can't materialize a class instance (napi's class
  // registry is thread-local), so native resolves the instance ON the JS
  // thread as a plain `native.Agent`; re-pointing its prototype at this class
  // is what makes the wrapper overrides below (wrapOnEvent / wrapHooks) apply.
  //
  // NOT an `async` method: the option-shape checks (wrapTools / wrapProvider /
  // wrapHooks) must throw SYNCHRONOUSLY, before any work starts. `await`ing
  // inside an async wrapper would turn those TypeErrors into Promise
  // rejections, contradicting the documented "malformed options throw".
  // `this.prototype` (not `Agent.prototype`) keeps subclassing intact.
  static create(configPath, opts = {}) {
    const AgentClass = this;
    const pending = native.Agent.create(
      configPath,
      wrapTools(opts.tools),
      wrapProvider(opts.provider),
      opts.apiKey,
      wrapHooks(opts.hooks),
    );
    return pending.then((agent) => {
      Object.setPrototypeOf(agent, AgentClass.prototype);
      return agent;
    });
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

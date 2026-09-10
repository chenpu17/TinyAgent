// End-to-end Node binding tests for tiny-agent.
// Runs fully offline against the in-process SSE mock in ./mock.js.
//
// Run:  cd bindings/node && npm test
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const { chat, Agent, CancelToken } = require('..');
const { ScriptedMock, toolCallSse, textSse, respondSse, hang } = require('./mock');

const MOCK_KEY = 'mock-key';

// Write a config JSON pointing at `mockUrl` into a fresh tmp workdir; clean up
// after the test. Returns { cfgPath, workdir }.
function writeConfig(t, mockUrl, opts = {}) {
  const workdir = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-node-'));
  t.after(() => {
    try { fs.rmSync(workdir, { recursive: true, force: true }); } catch { /* tmp cleanup best-effort */ }
  });
  const cfg = {
    provider: 'openai',
    api_key_env: 'TA_NODE_UNUSED_KEY', // never read — the key is passed explicitly
    base_url: mockUrl,
    model: 'mock-model',
    max_tokens: 1024,
    tools: opts.tools ?? ['write_file', 'read_file'],
    workdir,
    limits: {
      max_turns: opts.maxTurns ?? 6,
      timeout_secs: opts.timeoutSecs ?? 30,
      max_output_chars: 20000,
      stream_idle_secs: opts.streamIdleSecs ?? 30,
      // Undefined entries are dropped by JSON.stringify — opt-in only.
      keep_recent_messages: opts.keepRecentMessages,
    },
  };
  const cfgPath = path.join(workdir, 'config.json');
  fs.writeFileSync(cfgPath, JSON.stringify(cfg));
  return { cfgPath, workdir };
}

// ── 1. chat(): built-in write_file tool round-trip ──────────────────────────

test('chat one-shot: built-in write_file tool round-trip + side effect', async (t) => {
  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, toolCallSse('c1', 'write_file', { path: 'out.txt', content: 'mock ok\n' })),
    (_body, res) => respondSse(res, textSse('Task complete: wrote out.txt.')),
  ]);
  t.after(() => mock.stop());

  const { cfgPath, workdir } = writeConfig(t, mock.url);
  const out = await chat(cfgPath, 'write the file', { apiKey: MOCK_KEY });

  assert.ok(out.final_text.includes('Task complete'), out.final_text);
  assert.strictEqual(out.tool_calls[0].name, 'write_file');
  // The side effect landed in the configured workdir.
  assert.strictEqual(fs.readFileSync(path.join(workdir, 'out.txt'), 'utf8'), 'mock ok\n');
  assert.ok(out.turns >= 2);
});

// ── 2. Agent multi-turn: events stream + getters ────────────────────────────

test('Agent multi-turn: events stream, getters reflect state', async (t) => {
  const events = [];
  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, toolCallSse('c1', 'write_file', { path: 'a.txt', content: 'x' })),
    (_body, res) => respondSse(res, textSse('all done')),
  ]);
  t.after(() => mock.stop());

  const { cfgPath } = writeConfig(t, mock.url);
  const a = new Agent(cfgPath, { apiKey: MOCK_KEY });
  assert.strictEqual(a.model(), 'mock-model');

  a.pushUser('write a.txt');
  const out = await a.run((ev) => events.push(ev));

  // Events arrived as plain objects (externally-tagged AgentEvent shape).
  assert.ok(events.some((e) => 'TextDelta' in e), `expected a TextDelta event, got: ${JSON.stringify(events)}`);
  assert.ok(events.some((e) => 'Done' in e), 'expected a Done event');

  assert.ok(out.final_text.includes('all done'));
  assert.strictEqual(out.stop_reason, 'end_turn'); // OpenAI "stop" → "end_turn"

  // Getters surface the agent's internal state.
  assert.ok(Array.isArray(a.messages()));
  assert.ok(a.messages().length >= 2);
  const u = a.usage();
  for (const k of ['input_tokens', 'output_tokens', 'cache_creation_input_tokens', 'cache_read_input_tokens']) {
    assert.ok(k in u, `usage missing ${k}`);
  }
});

// ── 3. cancel interrupts the run ─────────────────────────────────────────────

test('cancel interrupts the in-flight run', async (t) => {
  const mock = await ScriptedMock.start([(_body, res, m) => hang(res, m)]);
  t.after(() => mock.stop());

  const { cfgPath } = writeConfig(t, mock.url, { tools: [], streamIdleSecs: 30 });
  const a = new Agent(cfgPath, { apiKey: MOCK_KEY });
  a.pushUser('hi');
  const tok = new CancelToken();
  a.setCancel(tok);

  // Flip cancel from a timer (the JS main thread is free during the awaited
  // run, which executes on a libuv worker thread).
  setTimeout(() => tok.cancel(), 300);

  const out = await a.run();
  assert.strictEqual(tok.isCancelled(), true);
  assert.strictEqual(out.stop_reason, 'cancelled');
});

test('stale cancel persists until clearCancel (acceptance contract)', async (t) => {
  // Kernel cancel-ownership contract (TinyWorker): run() NEVER clears the
  // flag. A stop left over from a cancelled run makes every following run
  // return instantly cancelled (turns=0, zero requests) until the HOST
  // clears it at its acceptance point — clearCancel().
  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, textSse('done')),
  ]);
  t.after(() => mock.stop());

  const { cfgPath } = writeConfig(t, mock.url, { tools: [] });
  const a = new Agent(cfgPath, { apiKey: MOCK_KEY });
  a.pushUser('hi');
  const tok = new CancelToken();
  a.setCancel(tok);

  tok.cancel(); // stop signalled before the run (the embedder's acceptance window)
  const out = await a.run();
  assert.strictEqual(out.stop_reason, 'cancelled');
  assert.strictEqual(out.turns, 0);

  // No clear in between: cancelled again, deterministically (not a race).
  const out2 = await a.run();
  assert.strictEqual(out2.stop_reason, 'cancelled');
  assert.strictEqual(out2.turns, 0);

  // Acceptance point: clear, then the follow-up runs to completion.
  a.clearCancel();
  const out3 = await a.run();
  assert.strictEqual(out3.stop_reason, 'end_turn');
  assert.strictEqual(out3.turns, 1);
});

// ── 4. honest 4-axis token usage ─────────────────────────────────────────────

test('usage four axes are reported', async (t) => {
  const usage = {
    prompt_tokens: 42,
    completion_tokens: 9,
    total_tokens: 51,
    prompt_tokens_details: { cached_tokens: 30 },
  };
  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, textSse('done', usage)),
  ]);
  t.after(() => mock.stop());

  const { cfgPath } = writeConfig(t, mock.url, { tools: [] });
  const out = await chat(cfgPath, 'hi', { apiKey: MOCK_KEY });
  assert.strictEqual(out.input_tokens, 12);               // 42 − 30 cached
  assert.strictEqual(out.output_tokens, 9);
  assert.strictEqual(out.cache_read_input_tokens, 30);
  assert.strictEqual(out.cache_creation_input_tokens, 0);  // OpenAI has no creation axis
});

// ── 5. custom JS tool is invoked (the headline capability) ───────────────────

test('custom JS tool (sync return) is invoked and recorded', async (t) => {
  const seen = [];
  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, toolCallSse('c1', 'get_weather', { city: 'Boston' })),
    (_body, res) => respondSse(res, textSse('The weather in Boston is sunny.')),
  ]);
  t.after(() => mock.stop());

  const { cfgPath } = writeConfig(t, mock.url, { tools: [] });
  const out = await chat(cfgPath, 'weather in Boston?', {
    apiKey: MOCK_KEY,
    tools: [{
      name: 'get_weather',
      description: 'Look up the weather for a city.',
      schema: {
        type: 'object',
        properties: { city: { type: 'string' } },
        required: ['city'],
      },
      run: (args) => {
        seen.push(args);
        return { content: 'sunny, 72F' };
      },
    }],
  });

  // The user's run ran once, with the model's parsed args.
  assert.deepStrictEqual(seen, [{ city: 'Boston' }]);
  // The call is recorded with its input and the tool's textual output.
  assert.strictEqual(out.tool_calls[0].name, 'get_weather');
  assert.strictEqual(out.tool_calls[0].input.city, 'Boston');
  assert.ok(out.tool_calls[0].output.includes('sunny'));
  assert.ok(out.final_text.includes('sunny'));
});

// ── 6. custom async JS tool + str return shorthand ──────────────────────────

test('custom async JS tool, str-return shorthand', async (t) => {
  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, toolCallSse('c1', 'echo', { msg: 'hi' })),
    (_body, res) => respondSse(res, textSse('done')),
  ]);
  t.after(() => mock.stop());

  const { cfgPath } = writeConfig(t, mock.url, { tools: [] });
  const out = await chat(cfgPath, 'echo hi', {
    apiKey: MOCK_KEY,
    tools: [{
      name: 'echo',
      description: 'echo back',
      schema: { type: 'object', properties: { msg: { type: 'string' } }, required: ['msg'] },
      // async + plain string return (shorthand for { content: str }).
      run: async (args) => `echo:${args.msg}`,
    }],
  });
  assert.strictEqual(out.tool_calls[0].output, 'echo:hi');
});

// ── 7. custom JS provider (no HTTP server at all) ───────────────────────────

// A config for custom-provider tests: base_url is never hit (the provider
// replaces the HTTP call entirely), so a dummy url suffices.
function providerConfig(t, opts = {}) {
  return writeConfig(t, 'http://unused-no-http-here', {
    tools: opts.tools ?? [],
    maxTurns: opts.maxTurns ?? 6,
    timeoutSecs: opts.timeoutSecs,
    keepRecentMessages: opts.keepRecentMessages,
  });
}

test('custom provider returns text + honest usage', async (t) => {
  const seen = [];
  const { cfgPath } = providerConfig(t);

  const a = new Agent(cfgPath, {
    provider: (request) => {
      seen.push(request);
      return {
        text: 'hello from custom provider',
        stop_reason: 'end_turn',
        usage: { input_tokens: 7, output_tokens: 4 },
      };
    },
  });
  a.pushUser('hi');
  const out = await a.run();

  assert.strictEqual(out.final_text, 'hello from custom provider');
  assert.strictEqual(out.stop_reason, 'end_turn');
  assert.strictEqual(out.input_tokens, 7);
  assert.strictEqual(out.output_tokens, 4);

  // The provider saw the request shape: model + the user message.
  assert.strictEqual(seen.length, 1);
  assert.strictEqual(seen[0].model, 'mock-model');
  assert.strictEqual(seen[0].max_tokens, 1024);
  assert.ok(JSON.stringify(seen[0].messages).includes('hi'));
});

test('custom provider, plain-string return shorthand', async (t) => {
  const { cfgPath } = providerConfig(t);
  const out = await chat(cfgPath, 'hi', {
    provider: () => 'just a string',
  });
  assert.strictEqual(out.final_text, 'just a string');
  assert.strictEqual(out.stop_reason, 'end_turn');
});

test('custom provider as { chat } object', async (t) => {
  // The object form (provider = { chat(request) }) is accepted too.
  const { cfgPath } = providerConfig(t);
  const out = await chat(cfgPath, 'hi', {
    provider: { chat: () => ({ text: 'object form', stop_reason: 'end_turn' }) },
  });
  assert.strictEqual(out.final_text, 'object form');
});

test('custom provider sees growing history across turns', async (t) => {
  const seen = [];
  const { cfgPath } = providerConfig(t, { maxTurns: 3 });

  const a = new Agent(cfgPath, {
    provider: (request) => {
      const n = request.messages.filter((m) => m.role === 'assistant').length;
      seen.push(n);
      return { text: `turn-${n}`, stop_reason: 'end_turn' };
    },
  });
  a.pushUser('first');
  await a.run();
  a.pushUser('second');
  await a.run();

  assert.deepStrictEqual(seen, [0, 1]);
});

test('compact(instructions?, onEvent?) folds the history between runs', async (t) => {
  // Host-initiated fold: one summary call folds the middle, the outcome
  // reports {dropped, kept, usage}, the Compressed event fires with trigger
  // "Manual", and the optional focus instructions thread through. A history
  // too short for keepRecentMessages resolves null.
  const events = [];
  const { cfgPath } = providerConfig(t, { keepRecentMessages: 2 });

  const a = new Agent(cfgPath, {
    provider: () => ({ text: 'SUMMARY', stop_reason: 'end_turn' }),
  });
  for (const msg of ['t1', 't2', 't3']) {
    a.pushUser(msg);
    await a.run();
  }
  // history: u1 a1 u2 a2 u3 a3 — 6 messages, safe cut at the trailing a3.
  assert.strictEqual(a.messages().length, 6);

  const outcome = await a.compact('focus on the API changes', (e) => events.push(e));
  assert.strictEqual(outcome.dropped, 4);
  assert.strictEqual(outcome.kept, 2);
  assert.strictEqual(a.messages().length, 2);
  const compressed = events.filter((e) => e && 'Compressed' in e).map((e) => e.Compressed);
  assert.ok(compressed.length >= 1, 'Compressed event forwarded');
  assert.strictEqual(compressed[0].trigger, 'Manual');

  // Too short to fold: null, no summary call.
  const b = new Agent(cfgPath, {
    provider: () => ({ text: 'x', stop_reason: 'end_turn' }),
  });
  b.pushUser('one');
  await b.run();
  assert.strictEqual(await b.compact(), null);
});

test('custom provider emits the whole text as one delta', async (t) => {
  // Non-streaming v1: the full text arrives as a single TextDelta event.
  const events = [];
  const { cfgPath } = providerConfig(t);
  await chat(cfgPath, 'hi', {
    provider: () => ({ text: 'whole text at once', stop_reason: 'end_turn' }),
    onEvent: (e) => events.push(e),
  });
  const deltas = events.filter((e) => 'TextDelta' in e).map((e) => e.TextDelta);
  assert.deepStrictEqual(deltas, ['whole text at once']);
});

test('custom provider that throws fails the run honestly', async (t) => {
  // A thrown provider rejects the run (the {__error} marker), rather than
  // silently producing an empty turn or hanging on the 120s timeout.
  const { cfgPath } = providerConfig(t);
  await assert.rejects(
    () => chat(cfgPath, 'hi', { provider: () => { throw new Error('boom'); } }),
    /boom/,
  );
});

test('custom provider empty text terminates cleanly', async (t) => {
  // Empty text must not loop forever or persist a bad assistant message:
  // from_simple yields zero blocks, the loop breaks at the empty-turn guard.
  const { cfgPath } = providerConfig(t);
  const out = await chat(cfgPath, 'hi', { provider: () => ({ text: '', stop_reason: 'end_turn' }) });
  assert.strictEqual(out.final_text, '');
  assert.strictEqual(out.stop_reason, 'end_turn');
});

// ── 8. custom provider: depth — cancel / concurrency / recovery / shape ─────

test('custom provider cancel takes effect after the call', async (t) => {
  // cancel flipped DURING the provider call: the JS provider (on the main
  // thread) can't be interrupted, and the worker blocks on the TSFN round-trip
  // — so cancel lands only after the provider resolves, at the turn boundary.
  // The run reports 'cancelled', but only once the provider call completes.
  const { cfgPath } = providerConfig(t);
  const a = new Agent(cfgPath, {
    provider: async () => {
      await new Promise((r) => setTimeout(r, 400));
      return { text: 'done', stop_reason: 'end_turn' };
    },
  });
  const tok = new CancelToken();
  a.setCancel(tok);
  a.pushUser('hi');
  setTimeout(() => tok.cancel(), 100);

  const start = Date.now();
  const out = await a.run();
  const elapsed = Date.now() - start;

  assert.strictEqual(out.stop_reason, 'cancelled');
  assert.strictEqual(tok.isCancelled(), true);
  // The provider ran to completion (≈400ms) — cancel did NOT cut it short.
  assert.ok(elapsed >= 350, `provider should run to completion, cancelled at ${elapsed}ms`);
});

test('custom provider: async providers run concurrently via the event loop', async (t) => {
  // Two agents running simultaneously: their run() calls overlap on libuv
  // worker threads, and — because the provider is ASYNC — its callback yields
  // the main thread at the first `await` (the TSFN call returns a promise napi
  // doesn't block on). So both sleeps run concurrently in the event loop, just
  // like the Python binding's GIL-released sleeping provider. The property
  // under test: no TSFN/worker deadlock under concurrent use, AND the two
  // async providers overlap in time (total ≈ one sleep, not two). A
  // *synchronous* provider WOULD serialize on the main thread — not tested
  // here, but documented.
  const { cfgPath } = providerConfig(t);
  const make = () => new Agent(cfgPath, {
    provider: async () => {
      await new Promise((r) => setTimeout(r, 120));
      return { text: 'ok', stop_reason: 'end_turn' };
    },
  });
  const runOne = async () => {
    const a = make();
    a.pushUser('hi');
    return (await a.run()).final_text;
  };

  const start = Date.now();
  const results = await Promise.all([runOne(), runOne()]);
  const elapsed = Date.now() - start;

  assert.deepStrictEqual(results, ['ok', 'ok']);
  // Concurrent: 2×120ms serialized would be ≈240ms; overlapped ≈120ms.
  // 200ms separates them with margin for timer coarseness / worker handoff.
  assert.ok(elapsed < 200, `expected concurrent (<200ms), got ${elapsed}ms — serialized?`);
});

test('custom provider recovers after an error', async (t) => {
  // A provider that throws on turn 1 rejects the run (the {__error} marker),
  // but the agent's state isn't corrupted: turn 2 runs fine after the provider
  // stops failing, and the failed turn left no partial assistant message.
  const state = { fail: true };
  const { cfgPath } = providerConfig(t);
  const a = new Agent(cfgPath, {
    provider: () => {
      if (state.fail) throw new Error('boom');
      return { text: 'recovered', stop_reason: 'end_turn' };
    },
  });
  a.pushUser('first');
  await assert.rejects(() => a.run(), /boom/);
  state.fail = false;
  a.pushUser('second');
  const out = await a.run();
  assert.strictEqual(out.final_text, 'recovered');
});

test('custom provider request messages shape', async (t) => {
  // Lock down the JSON shape handed to the provider: messages is a list of
  // {id?, role, content: [blocks]}, a text block is the externally-tagged
  // {text: str}. id is the agent-minted anchor (present on ingested
  // messages; hosts read it back via messages()).
  let seen;
  const { cfgPath } = providerConfig(t);
  const a = new Agent(cfgPath, {
    provider: (req) => { seen = req; return { text: 'ok', stop_reason: 'end_turn' }; },
  });
  a.pushUser('hello world');
  await a.run();
  assert.deepStrictEqual(seen.messages[0], { id: 'm1', role: 'user', content: [{ text: 'hello world' }] });
});

test('custom provider passes through non-end-turn stop_reason', async (t) => {
  // A non-end_turn stop_reason flows through unchanged. 'max_tokens'
  // exercises the loop's abort path (strips partial tool_use, keeps text +
  // the original stop_reason) — text-only still reports it.
  const { cfgPath } = providerConfig(t);
  const out = await chat(cfgPath, 'hi', {
    provider: () => ({ text: 'cut off', stop_reason: 'max_tokens', usage: { input_tokens: 1, output_tokens: 1 } }),
  });
  assert.strictEqual(out.stop_reason, 'max_tokens');
  assert.strictEqual(out.final_text, 'cut off');
});

test('custom provider wait is bounded by the run budget, not just the 120s backstop', async (t) => {
  // A provider that never resolves + timeout_secs=1: the request carries the
  // run's remaining budget as its per-request timeout, and the worker's wait
  // for the JS callback must honor it — run() rejects in ~1s instead of
  // hanging until the 120s backstop.
  const { cfgPath } = providerConfig(t, { timeoutSecs: 1 });
  const a = new Agent(cfgPath, {
    provider: () => new Promise(() => {}), // never settles
  });
  a.pushUser('hi');
  const started = Date.now();
  await assert.rejects(a.run(), /did not resolve within 1s/);
  const elapsed = Date.now() - started;
  assert.ok(elapsed < 10_000, `failed fast (~1s), took ${elapsed}ms`);
});

// ── 12. robustness: throwing onEvent, busy sync methods, budget-bounded waits ─

test('throwing onEvent does not kill the process; run still completes', async (t) => {
  // P1-3 regression: the native event TSFNs use ErrorStrategy::Fatal, so an
  // uncaught throw inside onEvent escalates to napi_fatal_exception — an
  // UNCATCHABLE process exit (the whole test runner would die). The wrapper
  // swallows the throw (Python-binding semantics); the run must complete.
  const { cfgPath } = providerConfig(t);
  const a = new Agent(cfgPath, {
    provider: () => ({ text: 'done', stop_reason: 'end_turn' }),
  });
  a.pushUser('hi');
  const out = await a.run((ev) => {
    throw new Error('buggy onEvent: ' + JSON.stringify(ev));
  });
  assert.strictEqual(out.stop_reason, 'end_turn');

  // Same guarantee for the one-shot chat() path (a rejected async callback).
  const out2 = await chat(cfgPath, 'hi', {
    provider: () => ({ text: 'ok', stop_reason: 'end_turn' }),
    onEvent: async () => {
      throw new Error('async throw');
    },
  });
  assert.strictEqual(out2.stop_reason, 'end_turn');
});

test('sync methods busy-error while run() is in flight, work again after', async (t) => {
  // P2-12 regression: a blocking lock on the JS main thread would freeze the
  // event loop the run's own TSFN round-trips depend on (deadlock in all but
  // name). The methods must fail fast with a busy error instead.
  const { cfgPath } = providerConfig(t);
  const a = new Agent(cfgPath, {
    provider: async () => {
      await new Promise((r) => setTimeout(r, 400));
      return { text: 'done', stop_reason: 'end_turn' };
    },
  });
  a.pushUser('hi');
  const p = a.run(); // NOT awaited yet — it holds the agent lock in flight
  await new Promise((r) => setTimeout(r, 100));
  for (const busy of [() => a.pushUser('x'), () => a.usage(), () => a.messages(), () => a.model()]) {
    assert.throws(busy, /active/, 'sync method must busy-error during run()');
  }
  const out = await p;
  assert.strictEqual(out.stop_reason, 'end_turn');
  // After the awaited run resolves, the methods work again.
  assert.strictEqual(a.model(), 'mock-model');
  assert.ok(Array.isArray(a.messages()));
  assert.strictEqual(a.usage().input_tokens, 0); // provider reported no usage
});

test('never-resolving JS tool wait is bounded by the run budget, not 120s', async (t) => {
  // P2-11 regression: the tool round-trip used to wait a fixed 120s backstop;
  // with limits.timeout_secs set it must wait only the REMAINING budget (the
  // same shape as the custom-provider wait), so a stuck tool fails the run in
  // ~2s here instead of 120s. (The simple custom-provider shape can't emit
  // tool calls, so the tool_use turn comes from the SSE mock.)
  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, toolCallSse('c1', 'hang', {})),
    (_body, res) => respondSse(res, textSse('late but bounded')),
  ]);
  t.after(() => mock.stop());
  const { cfgPath } = writeConfig(t, mock.url, { tools: [], timeoutSecs: 2 });
  const started = Date.now();
  const out = await chat(cfgPath, 'call the hanging tool', {
    apiKey: MOCK_KEY,
    tools: [{
      name: 'hang',
      description: 'never resolves',
      schema: { type: 'object', properties: {} },
      run: () => new Promise(() => {}), // never settles → never calls resolveTool
    }],
  });
  const elapsed = Date.now() - started;
  assert.ok(
    out.tool_calls[0].output.includes('did not resolve within'),
    `tool error should name the bounded wait: ${out.tool_calls[0].output}`,
  );
  assert.ok(elapsed < 60_000, `run must not wait the full 120s backstop (${elapsed}ms)`);
});

// ── N. runCode runtime switch (design §8.6; binding's `code` feature) ───────
//
// The binding's `code` cargo feature is OFF by default (the standard npm
// artifact stays ~10MB lighter), so these tests adapt to whichever build they
// run against: the default build must reject the three methods with the
// rebuild hint (a clear error, never a silent no-op); a
// `npm run build -- --features code` build must actually flip the tool
// table / apply the config object.

// Probe once at module scope (throwaway agent, custom provider → no network):
// disableCode succeeds on a code build and leaves the switch in its default
// OFF state; the default build raises the rebuild hint.
const CODE_BUILT = (() => {
  const workdir = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-node-code-probe-'));
  const cfgPath = path.join(workdir, 'config.json');
  fs.writeFileSync(cfgPath, JSON.stringify({
    provider: 'openai',
    api_key_env: 'TA_NODE_UNUSED_KEY',
    base_url: 'http://unused',
    model: 'mock-model',
    max_tokens: 256,
    tools: [],
    workdir,
  }));
  try {
    new Agent(cfgPath, { provider: () => ({ text: 'x', stop_reason: 'end_turn' }) }).disableCode();
    return true;
  } catch {
    return false;
  } finally {
    fs.rmSync(workdir, { recursive: true, force: true });
  }
})();

test('runCode: default build rejects the switch methods with a rebuild hint', { skip: CODE_BUILT }, async (t) => {
  const { cfgPath } = providerConfig(t);
  const a = new Agent(cfgPath, { provider: () => ({ text: 'x', stop_reason: 'end_turn' }) });
  assert.throws(() => a.enableCode(), /code` feature/);
  assert.throws(() => a.disableCode(), /code` feature/);
  assert.throws(() => a.setCodeConfig({ enabled: true }), /code` feature/);
});

test('runCode: enable/disable flips the model-visible tool table', { skip: !CODE_BUILT }, async (t) => {
  const seen = [];
  const { cfgPath } = providerConfig(t);
  const a = new Agent(cfgPath, {
    provider: (request) => {
      seen.push(request.tools.map((tool) => tool.name));
      return { text: 'done', stop_reason: 'end_turn' };
    },
  });
  a.disableCode(); // deterministic OFF start (the probe left it off already)
  a.pushUser('hi');
  await a.run();
  assert.ok(!seen[0].includes('run_code'), 'off by default (D15)');
  a.enableCode();
  a.pushUser('hi');
  await a.run();
  assert.ok(seen[1].includes('run_code'), 'enableCode surfaces the tool');
  a.disableCode();
  a.pushUser('hi');
  await a.run();
  assert.ok(!seen[2].includes('run_code'), 'disableCode removes it again');
});

test('runCode: setCodeConfig applies the dict; unknown keys error', { skip: !CODE_BUILT }, async (t) => {
  const seen = [];
  const { cfgPath } = providerConfig(t);
  const a = new Agent(cfgPath, {
    provider: (request) => {
      seen.push(request.tools.map((tool) => tool.name));
      return { text: 'done', stop_reason: 'end_turn' };
    },
  });
  a.disableCode();
  // The object goes through the same serde shape as the config file's "code"
  // section — a typo'd key errors instead of being silently ignored.
  assert.throws(() => a.setCodeConfig({ enabled: true, timeout_sec: 5 }), /unknown field/);
  a.setCodeConfig({ enabled: true });
  a.pushUser('hi');
  await a.run();
  assert.ok(seen[0].includes('run_code'), 'setCodeConfig drives the switch too');
});

test('runCode: busy error when called during run(); works again after (A-10c)', { skip: !CODE_BUILT }, async (t) => {
  // The code methods ride the same try_lock path as the other sync methods:
  // called from the provider callback (which runs on the JS thread while the
  // run holds the agent lock on its worker thread) they must throw the busy
  // error — blocking would freeze the event loop the round-trip depends on.
  const { cfgPath } = providerConfig(t);
  let busyErr = null;
  const a = new Agent(cfgPath, {
    provider: () => {
      try { a.enableCode(); } catch (e) { busyErr = e.message; }
      return { text: 'done', stop_reason: 'end_turn' };
    },
  });
  a.disableCode();
  a.pushUser('hi');
  await a.run();
  assert.ok(busyErr && busyErr.includes('Agent.run() is active'), busyErr);
  // After the awaited run resolves, the method works again.
  a.enableCode();
});

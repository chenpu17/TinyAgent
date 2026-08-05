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
      timeout_secs: 30,
      max_output_chars: 20000,
      stream_idle_secs: opts.streamIdleSecs ?? 30,
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
  const out = await chat(cfgPath, 'write the file', null, MOCK_KEY);

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
  const a = new Agent(cfgPath, MOCK_KEY);
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
  const a = new Agent(cfgPath, MOCK_KEY);
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
  const out = await chat(cfgPath, 'hi', null, MOCK_KEY);
  assert.strictEqual(out.input_tokens, 12);               // 42 − 30 cached
  assert.strictEqual(out.output_tokens, 9);
  assert.strictEqual(out.cache_read_input_tokens, 30);
  assert.strictEqual(out.cache_creation_input_tokens, 0);  // OpenAI has no creation axis
});

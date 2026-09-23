// Hooks interception layer exposed to Node (setHooks / hooks option).
//
// Covers the same decision surface as the Python binding's test_hooks.py,
// offline against ScriptedMock:
//   - Deny: the call never executes, the model sees the deny message as the
//     error tool_result, the run completes
//   - Modify: the REPLACED input executes while the audit keeps the ORIGINAL
//   - observe: afterTool receives (tool, original input, output view,
//     executed, ctx) with ctx attribution; onChatRequest fires per main-turn
//     request
//   - fail-closed: a throwing / ill-formed beforeTool DENIES (never silently
//     allows); observe-only hooks swallow throws (Fatal-strategy TSFNs would
//     otherwise abort the process)
//   - async beforeTool decisions
//   - chat() one-shot entry point accepts hooks too
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const { chat, Agent } = require('..');
const { ScriptedMock, toolCallSse, textSse, respondSse } = require('./mock');

const MOCK_KEY = 'mock-key';

// Two-request script: shell tool_call, then a final text.
const shellRound = (command) => [
  (_body, res) => respondSse(res, toolCallSse('c1', 'shell', { command })),
  (_body, res) => respondSse(res, textSse('ok done')),
];

async function withAgent(t, mockUrl, hooks, { useSetHooks = false } = {}) {
  const workdir = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-hooks-'));
  const cfgPath = path.join(workdir, 'config.json');
  fs.writeFileSync(cfgPath, JSON.stringify({
    provider: 'openai',
    api_key_env: 'TA_NODE_UNUSED_KEY',
    base_url: mockUrl,
    model: 'mock-model',
    max_tokens: 1024,
    tools: ['shell'],
    workdir,
    limits: { max_turns: 6, timeout_secs: 30, max_output_chars: 20000, stream_idle_secs: 30 },
  }));
  const a = new Agent(cfgPath, { apiKey: MOCK_KEY, ...(useSetHooks ? {} : { hooks }) });
  if (useSetHooks) a.setHooks(hooks);
  return { a, workdir, cleanup: () => fs.rmSync(workdir, { recursive: true, force: true }) };
}

// Make a recording hooks object with configurable behavior.
function makeHooks({ deny = null, modify = null, throwBefore = false, badReturn = false, async = false } = {}) {
  const h = {
    before: [], after: [], requests: [],
    deny, modify, throwBefore, badReturn,
  };
  h.beforeTool = async (tool, input, ctx) => {
    h.before.push([tool, input, ctx]);
    if (throwBefore) throw new Error('host policy exploded');
    if (badReturn) return 12345;
    if (deny != null) return deny;
    if (modify != null) return { modify };
    return null;
  };
  h.afterTool = (payload) => { h.after.push(payload); };
  h.onChatRequest = (req) => { h.requests.push(req); };
  return h;
}

test('hooks deny: call refused, model sees the message, run completes', async (t) => {
  const mock = await ScriptedMock.start(shellRound('echo original'));
  t.after(() => mock.stop());
  const hooks = makeHooks({ deny: 'shell is not allowed here' });
  const { a, workdir, cleanup } = await withAgent(t, mock.url, hooks);
  t.after(cleanup);

  a.pushUser('run it');
  const out = await a.run();

  assert.strictEqual(hooks.before[0][0], 'shell');
  assert.deepStrictEqual(hooks.before[0][1], { command: 'echo original' });
  assert.strictEqual(hooks.before[0][2].subagent, null);
  assert.ok(hooks.before[0][2].workdir.endsWith(path.basename(workdir)));

  const rec = out.tool_calls[0];
  assert.strictEqual(rec.is_error, true);
  assert.ok(rec.output.includes('shell is not allowed here'), rec.output);
  assert.ok(!rec.output.includes('original'));
  assert.strictEqual(hooks.after[0].executed, false);
  assert.strictEqual(out.final_text, 'ok done');
  assert.strictEqual(out.stop_reason, 'end_turn');
});

test('hooks modify: replacement executes, audit keeps the original', async (t) => {
  const mock = await ScriptedMock.start(shellRound('echo original'));
  t.after(() => mock.stop());
  const hooks = makeHooks({ modify: { command: 'echo replaced-input' } });
  const { a, cleanup } = await withAgent(t, mock.url, hooks);
  t.after(cleanup);

  a.pushUser('run it');
  const out = await a.run();

  const rec = out.tool_calls[0];
  assert.ok(rec.output.includes('replaced-input'), rec.output);
  assert.ok(!rec.output.includes('original'));
  assert.deepStrictEqual(rec.input, { command: 'echo original' });
  assert.strictEqual(rec.is_error, false);
  assert.strictEqual(hooks.after[0].executed, true);
});

test('hooks observe: afterTool payload + onChatRequest fire per request', async (t) => {
  const mock = await ScriptedMock.start(shellRound('echo observed'));
  t.after(() => mock.stop());
  const hooks = makeHooks();
  const { a, cleanup } = await withAgent(t, mock.url, hooks);
  t.after(cleanup);

  a.pushUser('run it');
  await a.run();

  const after = hooks.after[0];
  assert.strictEqual(after.tool, 'shell');
  assert.deepStrictEqual(after.input, { command: 'echo observed' });
  assert.ok(after.output.content.includes('observed'), after.output.content);
  assert.strictEqual(after.output.is_error, false);
  assert.strictEqual(after.executed, true);
  assert.strictEqual(after.ctx.subagent, null);

  // Two main-turn requests (tool round + final text), projected without
  // agent-internal plumbing.
  assert.strictEqual(hooks.requests.length, 2);
  for (const req of hooks.requests) {
    assert.strictEqual(req.model, 'mock-model');
    assert.strictEqual(req.max_tokens, 1024);
    assert.ok(Array.isArray(req.messages));
    assert.ok(req.tools.some((x) => x.name === 'shell'));
  }
});

test('hooks beforeTool that throws FAILS CLOSED', async (t) => {
  const mock = await ScriptedMock.start(shellRound('echo boom'));
  t.after(() => mock.stop());
  const hooks = makeHooks({ throwBefore: true });
  const { a, cleanup } = await withAgent(t, mock.url, hooks);
  t.after(cleanup);

  a.pushUser('run it');
  const out = await a.run();

  const rec = out.tool_calls[0];
  assert.strictEqual(rec.is_error, true);
  assert.ok(rec.output.includes('hook threw'), rec.output);
  assert.ok(rec.output.includes('host policy exploded'), rec.output);
  assert.ok(!rec.output.includes('boom'));
});

test('hooks ill-formed return FAILS CLOSED', async (t) => {
  const mock = await ScriptedMock.start(shellRound('echo bad'));
  t.after(() => mock.stop());
  const hooks = makeHooks({ badReturn: true });
  const { a, cleanup } = await withAgent(t, mock.url, hooks);
  t.after(cleanup);

  a.pushUser('run it');
  const out = await a.run();

  const rec = out.tool_calls[0];
  assert.strictEqual(rec.is_error, true);
  assert.ok(rec.output.includes("beforeTool('shell') refused"), rec.output);
});

test('hooks: async beforeTool decision is awaited (deny via setHooks)', async (t) => {
  const mock = await ScriptedMock.start(shellRound('echo async'));
  t.after(() => mock.stop());
  const hooks = makeHooks({ deny: 'denied after await' });
  const { a, cleanup } = await withAgent(t, mock.url, hooks, { useSetHooks: true });
  t.after(cleanup);

  a.pushUser('run it');
  const out = await a.run();

  const rec = out.tool_calls[0];
  assert.strictEqual(rec.is_error, true);
  assert.ok(rec.output.includes('denied after await'), rec.output);
});

test('chat one-shot accepts hooks', async (t) => {
  const mock = await ScriptedMock.start(shellRound('echo one-shot'));
  t.after(() => mock.stop());

  const hooks = makeHooks({ deny: 'denied by one-shot policy' });
  const workdir = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-hooks-'));
  t.after(() => fs.rmSync(workdir, { recursive: true, force: true }));
  const cfgPath = path.join(workdir, 'config.json');
  fs.writeFileSync(cfgPath, JSON.stringify({
    provider: 'openai',
    api_key_env: 'TA_NODE_UNUSED_KEY',
    base_url: mock.url,
    model: 'mock-model',
    max_tokens: 1024,
    tools: ['shell'],
    workdir,
    limits: { max_turns: 6, timeout_secs: 30, max_output_chars: 20000, stream_idle_secs: 30 },
  }));

  const out = await chat(cfgPath, 'run it', { apiKey: MOCK_KEY, hooks });
  assert.strictEqual(out.tool_calls[0].is_error, true);
  assert.ok(out.tool_calls[0].output.includes('denied by one-shot policy'));
});

test('hooks object without methods is a no-op (allow all)', async (t) => {
  const mock = await ScriptedMock.start(shellRound('echo passthrough'));
  t.after(() => mock.stop());

  const workdir = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-hooks-'));
  t.after(() => fs.rmSync(workdir, { recursive: true, force: true }));
  const cfgPath = path.join(workdir, 'config.json');
  fs.writeFileSync(cfgPath, JSON.stringify({
    provider: 'openai',
    api_key_env: 'TA_NODE_UNUSED_KEY',
    base_url: mock.url,
    model: 'mock-model',
    max_tokens: 1024,
    tools: ['shell'],
    workdir,
    limits: { max_turns: 6, timeout_secs: 30, max_output_chars: 20000, stream_idle_secs: 30 },
  }));

  const out = await chat(cfgPath, 'go', { apiKey: MOCK_KEY, hooks: {} });
  assert.strictEqual(out.tool_calls[0].is_error, false);
  assert.ok(out.tool_calls[0].output.includes('passthrough'));
});

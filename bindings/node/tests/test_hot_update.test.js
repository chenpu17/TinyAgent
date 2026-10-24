// Off-thread construction (Agent.create) + the hot-update surface
// (setModel family, MCP add/remove). Fully offline: a custom provider answers
// the turns, and the MCP server is a tiny Node script written to a tmp dir.
//
// Run: cd bindings/node && npm test
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const { Agent } = require('..');
const native = require('../index.js');
const { ScriptedMock, toolCallSse, textSse, respondSse } = require('./mock');

// Minimal MCP stdio server (NDJSON JSON-RPC 2.0) exposing one `echo` tool.
// `argv[2]` delays the initialize reply (ms) — the slow server that proves
// Agent.create does its assembly off the JS thread.
const MCP_SERVER = String.raw`
'use strict';
let buf = '';
const delay = Number(process.argv[2] || 0);
function reply(id, result) {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\n');
}
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buf += chunk;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    const msg = JSON.parse(line);
    const send = () => reply(msg.id, msg.method === 'initialize'
      ? { protocolVersion: '2025-03-26', capabilities: { tools: {} },
          serverInfo: { name: 'test-mcp', version: '0.1.0' } }
      : { tools: [{
          name: 'echo',
          description: 'Echo back the given text.',
          inputSchema: { type: 'object',
            properties: { text: { type: 'string' } }, required: ['text'] },
        }] });
    if (msg.method === 'initialize') setTimeout(send, delay); else send();
  }
});
`;

/** Write the MCP server script into a self-cleaning tmp dir; return its path. */
function writeMcpServer(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-mcp-'));
  t.after(() => {
    try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* best effort */ }
  });
  const file = path.join(dir, 'mcp_server.js');
  fs.writeFileSync(file, MCP_SERVER);
  return file;
}

function writeConfig(t, opts = {}) {
  const workdir = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-node-'));
  t.after(() => {
    try { fs.rmSync(workdir, { recursive: true, force: true }); } catch { /* best effort */ }
  });
  const cfg = {
    provider: 'openai',
    api_key_env: 'TA_NODE_UNUSED_KEY',
    model: 'mock-model',
    tools: [],
    workdir,
    mcp_servers: opts.mcpServers ?? [],
    base_url: opts.baseUrl,
    limits: { max_turns: 4, timeout_secs: 30 },
  };
  const cfgPath = path.join(workdir, 'config.json');
  fs.writeFileSync(cfgPath, JSON.stringify(cfg));
  return { cfgPath, workdir };
}

/** A provider that answers each turn with plain text and records the request. */
function recordingProvider(seen, text = 'ok') {
  return (req) => {
    seen.push(req);
    return { text, stop_reason: 'end_turn' };
  };
}

// ── 1. Agent.create: assembly happens off the JS thread ─────────────────────

test('Agent.create: slow MCP handshake does not freeze the event loop', async (t) => {
  const mcpScript = writeMcpServer(t);
  const { cfgPath } = writeConfig(t, {
    mcpServers: [{ name: 'slow', command: process.execPath, args: [mcpScript, '1200'] }],
  });

  let ticks = 0;
  const interval = setInterval(() => ticks++, 10);
  t.after(() => clearInterval(interval));
  const t0 = Date.now();
  let firstTimerAt = null;
  setTimeout(() => { firstTimerAt = Date.now() - t0; }, 50);

  const agent = await Agent.create(cfgPath, { provider: () => 'ok' });
  const elapsed = Date.now() - t0;
  clearInterval(interval);

  // Sanity: the handshake really was slow, otherwise this test proves nothing.
  assert.ok(elapsed >= 1000, `expected a >=1s handshake, got ${elapsed}ms`);
  // A 10ms interval is due ~100 times across that span; a blocked loop fires
  // it once on resume. 20 is far below the free-loop count and far above the
  // blocked one.
  assert.ok(
    ticks >= 20,
    `event loop was frozen during construction: only ${ticks} timer ticks in ${elapsed}ms`,
  );
  assert.ok(
    firstTimerAt !== null && firstTimerAt < elapsed - 200,
    `50ms timer fired at ${firstTimerAt}ms, agent resolved at ${elapsed}ms`,
  );
  assert.ok(agent instanceof Agent);
});

test('Agent.create: resolves with a wrapped Agent that runs end-to-end', async (t) => {
  const { cfgPath } = writeConfig(t);
  const a = await Agent.create(cfgPath, { provider: () => 'created ok' });

  // The wrapper's prototype must be in place — that is what keeps the
  // onEvent guard on run()/compact() (Fatal-strategy event TSFNs).
  assert.ok(a instanceof Agent);
  assert.strictEqual(Object.getPrototypeOf(a), Agent.prototype);
  assert.strictEqual(a.run, Agent.prototype.run);
  assert.strictEqual(a.compact, Agent.prototype.compact);

  a.pushUser('hello');
  const out = await a.run(() => { throw new Error('onEvent throw must be swallowed'); });
  assert.strictEqual(out.final_text, 'created ok');
  assert.strictEqual(out.stop_reason, 'end_turn');
});

test('Agent.create: config errors reject the promise (no synchronous throw)', async (t) => {
  const missing = path.join(os.tmpdir(), `ta-missing-config-${process.pid}.json`);
  const p = Agent.create(missing);
  assert.ok(p instanceof Promise);
  await assert.rejects(p, /cannot read config file/);
});

test('Agent.create: custom tools + hooks cross the worker assembly intact', async (t) => {
  // The TSFNs are built on the JS thread, then the trait objects are moved
  // into the worker that assembles the agent. This is the path the plain
  // constructor never exercises, so prove an actual tool round-trip and an
  // observe-only hook still fire.
  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, toolCallSse('c1', 'greet', { name: 'Ann' })),
    (_body, res) => respondSse(res, textSse('done')),
  ]);
  t.after(() => mock.stop());
  const { cfgPath } = writeConfig(t, { baseUrl: mock.url });
  const chatRequests = [];

  const a = await Agent.create(cfgPath, {
    apiKey: 'test-key',
    tools: [{
      name: 'greet',
      description: 'Greet someone.',
      schema: { type: 'object', properties: { name: { type: 'string' } }, required: ['name'] },
      run: (args) => `hi ${args.name}`,
    }],
    hooks: { onChatRequest: (req) => chatRequests.push(req.model) },
  });

  a.pushUser('go');
  const out = await a.run();
  assert.strictEqual(out.final_text, 'done');
  assert.strictEqual(out.tool_calls[0].name, 'greet');
  assert.strictEqual(out.tool_calls[0].output, 'hi Ann');
  assert.ok(chatRequests.length >= 1, 'onChatRequest must reach the wrapper hook');
});

test('Agent.create: returns the subclass instance for a subclass constructor', async (t) => {
  const { cfgPath } = writeConfig(t);
  class SubAgent extends Agent {}
  const a = await SubAgent.create(cfgPath, { provider: () => 'ok' });
  assert.ok(a instanceof SubAgent, 'prototype fix-up must use this.prototype');
  a.pushUser('hi');
  assert.strictEqual((await a.run()).final_text, 'ok');
});

// ── 1b. input-shape validation (fail loud, never abort) ────────────────────

test('hooks: a present non-function member throws instead of aborting', (t) => {
  const { cfgPath } = writeConfig(t);
  // Before the TSFN shape check this was a process-level abort
  // (`Assertion failed: (func) != nullptr`) on every entry point.
  for (const [key, hooks] of [
    ['beforeTool', { beforeTool: 1 }],
    ['afterTool', { afterTool: 1 }],
    ['onChatRequest', { onChatRequest: 1 }],
  ]) {
    assert.throws(
      () => new Agent(cfgPath, { apiKey: 'test-key', hooks }),
      new RegExp(`hooks\\.${key} must be a function when present`),
    );
    assert.throws(
      () => new native.Agent(cfgPath, undefined, undefined, 'test-key', hooks),
      new RegExp(`hooks\\.${key} must be a function when present`),
      'the native constructor must reject the shape too (no shell wrapper needed)',
    );
  }

  // null counts as absent, and setHooks is validated as well.
  const a = new Agent(cfgPath, { apiKey: 'test-key', hooks: { beforeTool: null } });
  assert.throws(() => a.setHooks({ afterTool: 1 }), /hooks\.afterTool must be a function/);
  a.setHooks({ afterTool: null, onChatRequest: () => {} });
});

test('wrapper: malformed tools throw synchronously (constructor and create)', async (t) => {
  const { cfgPath } = writeConfig(t);
  const cases = [
    ['nope', /tools must be an array/],
    [{}, /tools must be an array/],
    [[{ name: 't', description: 'd', schema: {}, run: 42 }], /tools\[0\]\.run must be a function/],
    [[{ name: 't', description: 'd', schema: 'nope', run: () => '' }], /tools\[0\]\.schema must be a JSON Schema object/],
    [[{ name: 't', description: 'd', run: () => '' }], /tools\[0\]\.schema must be a JSON Schema object/],
    [[{ name: '', description: 'd', schema: {}, run: () => '' }], /tools\[0\]\.name must be a non-empty string/],
  ];
  for (const [tools, re] of cases) {
    assert.throws(() => new Agent(cfgPath, { apiKey: 'test-key', tools }), re);
    // Synchronous throw, NOT a rejected Promise: the option check runs before
    // any work starts.
    assert.throws(() => Agent.create(cfgPath, { apiKey: 'test-key', tools }), re);
  }
});

// ── 2. setModel / limits hot updates ───────────────────────────────────────

test('hot update: setModel/setMaxTokens reach the next request', async (t) => {
  const seen = [];
  const { cfgPath } = writeConfig(t);
  const a = new Agent(cfgPath, { provider: recordingProvider(seen) });
  assert.strictEqual(a.model(), 'mock-model');

  a.setModel('other-model');
  assert.strictEqual(a.model(), 'other-model');
  a.setReasoningEffort('high');
  a.setReasoningEffort(null);
  a.setMaxTokens(0); // clamped to 1
  a.setMaxContextTokens(4000);
  a.setMaxContextTokens(0); // back to the char heuristic
  assert.throws(() => a.setMaxContextTokens(-1), /tokens must be an integer >= 0/);

  a.pushUser('hi');
  await a.run();
  assert.strictEqual(seen[0].model, 'other-model');
  assert.strictEqual(seen[0].max_tokens, 1);
});

test('hot update: numeric args reject fractional / negative / out-of-range values', (t) => {
  // napi's u32 conversion used to truncate and wrap: -5 became 4294967291,
  // 2**40 became 1, 1.7 became 1. The wrapper now sees the original f64.
  const { cfgPath } = writeConfig(t);
  const a = new Agent(cfgPath, { apiKey: 'test-key' });

  for (const bad of [-5, 1.7, 2 ** 40, NaN, Infinity]) {
    assert.throws(
      () => a.setMaxTokens(bad),
      /setMaxTokens: maxTokens must be an integer in 0/,
      `setMaxTokens(${bad})`,
    );
  }
  for (const bad of [-1, 1.5, NaN, Infinity]) {
    assert.throws(
      () => a.setMaxContextTokens(bad),
      /setMaxContextTokens: tokens must be an integer >= 0/,
      `setMaxContextTokens(${bad})`,
    );
  }
  // Boundaries remain valid; 0 is the kernel's "clamp to 1 / char heuristic".
  a.setMaxTokens(0);
  a.setMaxTokens(4294967295);
  a.setMaxContextTokens(0);
  a.setMaxContextTokens(2 ** 53);
});

test('hot update: sync methods busy-error while a run is in flight', async (t) => {
  const { cfgPath } = writeConfig(t);
  const a = new Agent(cfgPath, {
    provider: async () => {
      await new Promise((r) => setTimeout(r, 400));
      return { text: 'done', stop_reason: 'end_turn' };
    },
  });
  a.pushUser('hi');
  const runP = a.run();
  await new Promise((r) => setTimeout(r, 100));

  const busy = [
    () => a.setModel('x'),
    () => a.setReasoningEffort(null),
    () => a.setMaxTokens(10),
    () => a.setMaxContextTokens(0),
    () => a.setHooks({}),
    () => a.addMcpServer({ name: 'x', command: process.execPath, args: ['-e', ''] }),
    () => a.removeMcpServer('x'),
  ];
  for (const fn of busy) assert.throws(fn, /Agent\.run\(\) is active/);

  const out = await runP;
  assert.strictEqual(out.stop_reason, 'end_turn');
  // After the awaited run resolves the lock is free again: a real hot update
  // succeeds, and removeMcpServer reaches the agent (its "not attached" error
  // is the kernel's, not the busy error).
  a.setModel('after-run');
  assert.strictEqual(a.model(), 'after-run');
  assert.throws(() => a.removeMcpServer('x'), /no MCP server 'x'/);
});

// ── 3. MCP hot add/remove ─────────────────────────────────────────────────

test('hot update: MCP add/remove changes the next request toolset', async (t) => {
  const mcpScript = writeMcpServer(t);
  const seen = [];
  const { cfgPath } = writeConfig(t);
  const a = new Agent(cfgPath, { provider: recordingProvider(seen) });

  const added = a.addMcpServer({ name: 'echo', command: process.execPath, args: [mcpScript] });
  assert.deepStrictEqual(added, ['mcp_echo_echo']);

  a.pushUser('first');
  await a.run();
  assert.ok(
    seen[0].tools.some((tool) => tool.name === 'mcp_echo_echo'),
    `next request must carry the hot-added tool: ${JSON.stringify(seen[0].tools.map((t) => t.name))}`,
  );

  assert.strictEqual(a.removeMcpServer('echo'), 1);
  a.pushUser('second');
  await a.run();
  assert.ok(
    !seen[1].tools.some((tool) => tool.name === 'mcp_echo_echo'),
    'removed tool must be gone from the next request',
  );

  // A second remove (or a typo) is an error, not a silent no-op.
  assert.throws(() => a.removeMcpServer('echo'), /no MCP server 'echo'/);
});

test('hot update: MCP name collision is a recoverable error and leaves the agent unchanged', async (t) => {
  const mcpScript = writeMcpServer(t);
  const seen = [];
  const { cfgPath } = writeConfig(t);
  const a = new Agent(cfgPath, { provider: recordingProvider(seen) });

  a.addMcpServer({ name: 'echo', command: process.execPath, args: [mcpScript] });
  assert.throws(
    () => a.addMcpServer({ name: 'echo', command: process.execPath, args: [mcpScript] }),
    /collision/,
  );

  a.pushUser('hi');
  await a.run();
  const echoes = seen[0].tools.filter((tool) => tool.name === 'mcp_echo_echo');
  assert.strictEqual(echoes.length, 1, 'failed add must not leave a duplicate tool behind');
});

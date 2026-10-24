// pushUser content blocks: the str shorthand, the Anthropic-style tagged
// form, the messages() serde form (round-trip), sequences, and rejections.
//
// Run: cd bindings/node && npm test
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const { Agent } = require('..');

function newAgent(t) {
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
    limits: { max_turns: 4, timeout_secs: 30 },
  };
  const cfgPath = path.join(workdir, 'config.json');
  fs.writeFileSync(cfgPath, JSON.stringify(cfg));
  return new Agent(cfgPath, { apiKey: 'test-key' });
}

test('pushUser: str shorthand is one text block', (t) => {
  const a = newAgent(t);
  a.pushUser('plain text');
  const msgs = a.messages();
  assert.strictEqual(msgs.length, 1);
  assert.strictEqual(msgs[0].role, 'user');
  assert.deepStrictEqual(msgs[0].content, [{ text: 'plain text' }]);
});

test('pushUser: tagged image block + sequence preserve order', (t) => {
  const a = newAgent(t);
  a.pushUser([
    'caption',
    { type: 'image', media_type: 'image/png', data: 'aGVsbG8=' },
    { type: 'document', media_type: 'application/pdf', data: 'JVBERi0=' },
  ]);
  assert.deepStrictEqual(a.messages()[0].content, [
    { text: 'caption' },
    { image: { media_type: 'image/png', data: 'aGVsbG8=' } },
    { document: { media_type: 'application/pdf', data: 'JVBERi0=' } },
  ]);
});

test('pushUser: a block read from messages() can be pushed straight back', (t) => {
  const a = newAgent(t);
  a.pushUser({ type: 'image', media_type: 'image/jpeg', data: '/9j/4AAQ' });
  const block = a.messages()[0].content[0];

  const b = newAgent(t);
  b.pushUser(block); // serde shape: { image: { … } }
  assert.deepStrictEqual(b.messages()[0].content[0], block);
});

test('pushUser: a single block object is one block', (t) => {
  const a = newAgent(t);
  a.pushUser({ type: 'text', text: 'single' });
  assert.deepStrictEqual(a.messages()[0].content, [{ text: 'single' }]);
});

test('pushUser: bad input throws and appends nothing', (t) => {
  const a = newAgent(t);
  const cases = [
    [[], /at least one content block/],
    [[{ type: 'tool_use', id: 'x', name: 't', input: {} }], /agent-loop internal/],
    [[{ tool_result: { tool_use_id: 'x', content: 'r' } }], /agent-loop internal/],
    [[{ type: 'audio', data: 'x' }], /unknown content block type/],
    [[{ type: 'image', media_type: 'image/png' }], /needs a string "data" field/],
    [[{ nope: 1 }], /needs a string "type" field/],
    [123, /expects a string/],
    // One object must carry one shape: silently keeping the first would drop
    // the host's other content block.
    [[{ text: 'a', image: { media_type: 'image/png', data: 'x' } }], /mixes multiple message shapes/],
    [[{ type: 'text', text: 'a', image: { media_type: 'image/png', data: 'x' } }], /also carries the "image" shape key/],
    // Wrong value types get a precise message, not a fall-through shape error.
    [[{ text: 123 }], /needs a string "text" field/],
    [[{ image: 'nope' }], /needs an object "image" field/],
  ];
  for (const [bad, re] of cases) {
    assert.throws(() => a.pushUser(bad), re, `pushUser(${JSON.stringify(bad)})`);
  }
  assert.deepStrictEqual(a.messages(), [], 'a rejected push must not append anything');
});

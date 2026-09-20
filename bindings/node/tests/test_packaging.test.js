// Packaging acceptance test: `npm pack` → install into an isolated
// directory → require + one offline mock round-trip through the INSTALLED
// artifact. Guards the "files" whitelist in package.json: it must ship
// everything the generated loader needs. Before "*.node" was added, the
// tarball carried no native binary and no platform dependency packages are
// published either — installing it yielded MODULE_NOT_FOUND:
// tiny-agent-<platform> (external re-review P2; source-tree tests can't
// see this because they require('..') next to the build output).
//
// NOTE: this file deliberately does NOT require('..') — loading the
// installed copy is the point, and a second native-module instance in one
// process is exactly what we avoid.
//
// Run:  cd bindings/node && npm run build:debug && npm test
const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { execFileSync } = require('node:child_process');

const { ScriptedMock, textSse, respondSse } = require('./mock');

const PKG_DIR = path.join(__dirname, '..');

// npm ships as npm.cmd on Windows: CreateProcess can't exec a .cmd directly
// and Node refuses to spawn one without a shell outright (EINVAL, since the
// 2024 batch-injection fix) — so shell mode is required there. Shell mode
// joins args unquoted, hence the manual quoting for spaced paths (mkdtemp
// under a spaced TMPDIR). POSIX keeps the direct exec; nothing changes on CI.
function npm(args, opts) {
  const win = process.platform === 'win32';
  return execFileSync('npm', win
    ? args.map((a) => (/\s/.test(a) ? `"${a}"` : a))
    : args, { ...opts, shell: win, encoding: 'utf8' });
}

test('npm pack → clean install → load + mock round-trip', { timeout: 60000 }, async (t) => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-pack-'));
  t.after(() => {
    try { fs.rmSync(tmp, { recursive: true, force: true }); } catch { /* best-effort */ }
  });

  // 1. Pack from the source dir (same precondition as the rest of the
  //    suite: a locally built *.node must exist, or the tarball has no
  //    binary to ship and the install below fails — which is the bug this
  //    test exists to catch).
  const packOut = npm(
    ['pack', '--json', '--silent', `--pack-destination=${tmp}`],
    { cwd: PKG_DIR },
  );
  const tarball = path.join(tmp, JSON.parse(packOut)[0].filename);

  // 2. Install into a consumer package that has never seen the source tree.
  const consumer = path.join(tmp, 'consumer');
  fs.mkdirSync(consumer);
  fs.writeFileSync(path.join(consumer, 'package.json'),
    JSON.stringify({ name: 'ta-pack-consumer', private: true }));
  npm(
    ['install', '--no-audit', '--no-fund', '--no-save', tarball],
    { cwd: consumer },
  );

  // 3. Load the INSTALLED artifact and run one offline round-trip through
  //    it (config + mock server, same shape as test_agent.test.js).
  const installed = require(path.join(consumer, 'node_modules', 'tiny-agent'));
  assert.strictEqual(typeof installed.chat, 'function', 'installed package exports chat');

  const mock = await ScriptedMock.start([
    (_body, res) => respondSse(res, textSse('installed ok')),
  ]);
  t.after(() => mock.stop());

  const cfgPath = path.join(consumer, 'config.json');
  fs.writeFileSync(cfgPath, JSON.stringify({
    provider: 'openai',
    api_key_env: 'TA_NODE_UNUSED_KEY', // never read — the key is passed explicitly
    base_url: mock.url,
    model: 'mock-model',
    max_tokens: 64,
    tools: [],
    workdir: consumer,
    limits: {
      max_turns: 2,
      timeout_secs: 15,
      max_output_chars: 20000,
      stream_idle_secs: 15,
    },
  }));
  const out = await installed.chat(cfgPath, 'ping', { apiKey: 'mock-key' });
  assert.ok(out.final_text.includes('installed ok'), out.final_text);
});

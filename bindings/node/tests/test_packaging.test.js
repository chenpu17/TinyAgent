// Packaging acceptance test: `npm pack` → install into an isolated
// directory → require + one offline mock round-trip through the INSTALLED
// artifact.
//
// Distribution contract (cross-platform npm split, 0.3.x): the main tarball
// carries NO native binary — `*.node` ships in per-platform subpackages
// (tiny-agent-<platform> via optionalDependencies). The napi-generated
// loader (index.js) resolves a local ./.node file first (dev builds) and
// falls back to require('tiny-agent-<platform>'). This suite guards both
// halves of that contract:
//
//   1. tarball whitelist leg — no *.node inside, loader + types present
//      (before the split, the tarball shipped the packing machine's binary;
//      after removing it, the regression to guard is a whitelist that
//      accidentally drops index.js/index.d.ts or re-adds *.node).
//   2. subpackage resolution leg — a consumer that has only the installed
//      main package + a platform subpackage loads and runs offline.
//      The subpackage is staged locally (same layout CI publishes); a real
//      registry install is out of scope for offline tests.
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

// Mirror the napi loader's platform → subpackage name / artifact name mapping
// for the platforms this repo publishes (release.yml npm matrix). Unknown
// combos return null and the resolution leg skips itself.
function hostSubpackage() {
  const { platform, arch } = process;
  if (platform === 'darwin') {
    const a = arch === 'arm64' ? 'arm64' : arch === 'x64' ? 'x64' : null;
    return a && { pkg: `tiny-agent-darwin-${a}`, file: `tiny-agent.darwin-${a}.node` };
  }
  if (platform === 'win32') {
    return arch === 'x64'
      ? { pkg: 'tiny-agent-win32-x64-msvc', file: 'tiny-agent.win32-x64-msvc.node' }
      : null;
  }
  if (platform === 'linux' && arch === 'x64') {
    let musl = false;
    try {
      musl = require('child_process').execSync('which ldd')
        .toString().trim().length === 0;
    } catch { /* ldd missing → assume gnu (matches index.js) */ }
    const suffix = musl ? 'musl' : 'gnu';
    return { pkg: `tiny-agent-linux-x64-${suffix}`, file: `tiny-agent.linux-x64-${suffix}.node` };
  }
  return null;
}

function findLocalNode() {
  // dev builds emit either the plain `--platform` name or a --target suffixed
  // name; accept either so the suite runs from any local workflow
  const dir = PKG_DIR;
  const candidates = fs.readdirSync(dir).filter((f) => f.endsWith('.node'));
  return candidates[0] || null;
}

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

test('main tarball whitelist: no *.node, loader + types shipped', { timeout: 60000 }, (t) => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-pack-wl-'));
  t.after(() => { try { fs.rmSync(tmp, { recursive: true, force: true }); } catch { /* best-effort */ } });

  const packOut = npm(
    ['pack', '--json', '--silent', `--pack-destination=${tmp}`],
    { cwd: PKG_DIR },
  );
  const tarball = path.join(tmp, JSON.parse(packOut)[0].filename);
  const files = execFileSync('tar', ['-tzf', tarball], { encoding: 'utf8' })
    .trim().split('\n').map((f) => f.replace(/^package\//, ''));

  assert.ok(files.includes('index.js'), 'generated loader shipped');
  assert.ok(files.includes('index.d.ts'), 'generated types shipped');
  assert.ok(files.includes('wrapper.js'), 'wrapper shipped');
  assert.ok(files.includes('wrapper.d.ts'), 'wrapper types shipped');
  const nodes = files.filter((f) => f.endsWith('.node'));
  assert.deepStrictEqual(nodes, [],
    'main tarball must NOT carry a native binary (ships via platform subpackages)');
});

test('npm pack → install + platform subpackage → load + mock round-trip', { timeout: 60000 }, async (t) => {
  const sub = hostSubpackage();
  const nodeFile = findLocalNode();
  if (!sub || !nodeFile) {
    t.skip(`no local publish mapping (${sub?.pkg ?? 'unknown'}) or built binary (${nodeFile ?? 'none'})`);
    return;
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'ta-pack-'));
  t.after(() => {
    try { fs.rmSync(tmp, { recursive: true, force: true }); } catch { /* best-effort */ }
  });

  // 1. Pack the main package (no binary inside — asserted by the whitelist leg).
  const packOut = npm(
    ['pack', '--json', '--silent', `--pack-destination=${tmp}`],
    { cwd: PKG_DIR },
  );
  const tarball = path.join(tmp, JSON.parse(packOut)[0].filename);

  // 2. Install into a consumer package that has never seen the source tree.
  //    optionalDependencies point at registry names that may not exist yet;
  //    npm treats unresolvable optionals as warnings, which is precisely the
  //    pre-publish state this leg simulates.
  const consumer = path.join(tmp, 'consumer');
  fs.mkdirSync(consumer);
  fs.writeFileSync(path.join(consumer, 'package.json'),
    JSON.stringify({ name: 'ta-pack-consumer', private: true }));
  npm(
    ['install', '--no-audit', '--no-fund', '--no-save', '--ignore-scripts', tarball],
    { cwd: consumer },
  );

  // 3. Stage the platform subpackage exactly as CI builds and publishes it:
  //    package.json (matching name) + the native artifact at the loader's
  //    expected file name.
  const subDir = path.join(consumer, 'node_modules', sub.pkg);
  fs.mkdirSync(subDir, { recursive: true });
  fs.writeFileSync(path.join(subDir, 'package.json'), JSON.stringify({
    name: sub.pkg,
    version: JSON.parse(fs.readFileSync(path.join(PKG_DIR, 'package.json'), 'utf8')).version,
    description: `TinyAgent native binding (${sub.pkg})`,
    license: 'MIT',
    main: sub.file,
    files: [sub.file],
  }));
  fs.copyFileSync(path.join(PKG_DIR, nodeFile), path.join(subDir, sub.file));

  // 4. Load the INSTALLED artifact (subpackage resolution path) and run one
  //    offline round-trip through it.
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

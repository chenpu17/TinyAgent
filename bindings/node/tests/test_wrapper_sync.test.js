// Wrapper-sync guard (#6 of the 2026-09 review): native Agent methods were
// added (clearCancel / compact / enableCode / disableCode / setCodeConfig)
// without wrapper.d.ts — or wrapper.js — following, so TypeScript users
// couldn't discover them and compact() reached the Fatal-strategy event TSFN
// unwrapped. This test takes the NATIVE PROTOTYPE as the source of truth and
// fails when either user-facing file drifts from it.
//
// Run: cd bindings/node && npm test

const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

const native = require('../index.js');
const wrapper = require('../wrapper.js');

/** The method names the native Agent class actually exposes (napi
 *  snake_case → camelCase), excluding the constructor. */
function nativeAgentMethods() {
  return Object.getOwnPropertyNames(native.Agent.prototype).filter(
    (k) => k !== 'constructor' && typeof native.Agent.prototype[k] === 'function',
  );
}

/** Method names declared inside `export declare class Agent { … }`. */
function declaredAgentMethods() {
  const dts = fs.readFileSync(path.join(__dirname, '..', 'wrapper.d.ts'), 'utf8');
  const m = dts.match(/export declare class Agent \{([\s\S]*?)\n\}/);
  assert.ok(m, 'wrapper.d.ts must contain `export declare class Agent { … }`');
  return [...m[1].matchAll(/^\s{2}([A-Za-z_$][\w$]*)\s*[<(]/gm)]
    .map((x) => x[1])
    .filter((k) => k !== 'constructor'); // the ctor line matches too; both sides skip it
}

test('wrapper.d.ts declares every native Agent method (no prototype drift)', () => {
  const declared = new Set(declaredAgentMethods());
  const missing = nativeAgentMethods().filter((k) => !declared.has(k));
  assert.deepStrictEqual(
    missing,
    [],
    `wrapper.d.ts is missing native Agent method(s): ${missing}. ` +
      'Adding a native method requires updating wrapper.d.ts (and wrapper.js ' +
      'when it takes an onEvent callback) — see the binding checklist in CLAUDE.md.',
  );
});

test('wrapper.d.ts declares no method the native class lacks (no stale entries)', () => {
  const proto = new Set(nativeAgentMethods());
  const stale = declaredAgentMethods().filter((k) => !proto.has(k));
  assert.deepStrictEqual(stale, [], `wrapper.d.ts declares removed/renamed method(s): ${stale}`);
});

test('wrapper.js wraps every method that takes onEvent (Fatal TSFN protection)', () => {
  // run() and compact() hand a user callback to an ErrorStrategy::Fatal
  // event TSFN — the wrapper MUST override them (wrapOnEvent) so a throwing
  // callback degrades to a stderr log instead of an uncatchable abort.
  for (const k of ['run', 'compact']) {
    assert.strictEqual(typeof wrapper.Agent.prototype[k], 'function', `wrapper.Agent.${k} exists`);
    assert.notStrictEqual(
      wrapper.Agent.prototype[k],
      native.Agent.prototype[k],
      `wrapper.Agent.${k} must be an OVERRIDE (wrapOnEvent), not the raw native method`,
    );
  }
});

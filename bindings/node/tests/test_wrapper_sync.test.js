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

/** The declared Agent methods whose signature takes an `onEvent` callback.
 *  Derived from wrapper.d.ts on every run — NOT hardcoded — so adding a
 *  third onEvent-bearing native method fails the wrap test below until
 *  wrapper.js actually wraps it. A hardcoded list was exactly the blind
 *  spot: nothing forced a new entry onto it (external review #7). */
function methodsTakingOnEvent() {
  const dts = fs.readFileSync(path.join(__dirname, '..', 'wrapper.d.ts'), 'utf8');
  const m = dts.match(/export declare class Agent \{([\s\S]*?)\n\}/);
  assert.ok(m, 'wrapper.d.ts must contain `export declare class Agent { … }`');
  const body = m[1];
  const starts = [...body.matchAll(/^ {2}([A-Za-z_$][\w$]*)\s*[<(]/gm)].filter(
    (x) => x[1] !== 'constructor',
  );
  return starts
    .map((s) => {
      // A signature may span lines (compact(…)); it ends at the first line
      // starting a new member or a doc comment at class-member indent
      // (exactly two spaces) — continuation params are indented deeper, and
      // stopping at doc comments keeps the NEXT method's prose (which may
      // mention onEvent) out of this signature's span.
      const rest = body.slice(s.index);
      const end = rest.search(/\n {2}(?:\/\*\*|[A-Za-z_$])/);
      return { name: s[1], sig: end === -1 ? rest : rest.slice(0, end) };
    })
    .filter((x) => /\bonEvent\b/.test(x.sig))
    .map((x) => x.name);
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
  // The list is DERIVED from wrapper.d.ts (methodsTakingOnEvent), not
  // hardcoded: a new onEvent-bearing method must fail here until wrapped.
  const expected = methodsTakingOnEvent();
  assert.ok(
    expected.includes('run') && expected.includes('compact'),
    `derivation regression: expected at least run+compact, got [${expected}]`,
  );
  for (const k of expected) {
    assert.strictEqual(typeof wrapper.Agent.prototype[k], 'function', `wrapper.Agent.${k} exists`);
    assert.notStrictEqual(
      wrapper.Agent.prototype[k],
      native.Agent.prototype[k],
      `wrapper.Agent.${k} must be an OVERRIDE (wrapOnEvent), not the raw native method`,
    );
  }
});

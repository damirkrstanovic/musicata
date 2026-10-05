// SPDX-License-Identifier: AGPL-3.0-or-later
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { test } from 'node:test';
import assert from 'node:assert/strict';
const require = createRequire(new URL('../../crates/musicata-server/web/package.json', import.meta.url));
const ts = require('typescript');
const source = readFileSync(new URL('../../crates/musicata-server/web/src/lib/audio.ts', import.meta.url), 'utf8');
const js = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText;
const exports = {};
new Function('exports', js)(exports);
const { BrowserAudio } = exports;
const profile = { id: 'same-room', name: 'Room', preampDb: 0, bands: [], roomIr: { sampleRate: 48000 } };
function driver() {
  globalThis.document = { addEventListener() {} };
  const audio = new BrowserAudio({ addEventListener() {} });
  audio.ctx = { sampleRate: 48000, decodeAudioData: async bytes => bytes };
  audio.rebuildChain = () => {};
  return audio;
}
test('a late IR load for the same profile cannot replace the latest revision', async () => {
  const audio = driver();
  let resolveOld;
  const old = new Promise(resolve => { resolveOld = resolve; });
  const latest = new ArrayBuffer(2);
  let calls = 0;
  globalThis.fetch = async () => ({ ok: true, arrayBuffer: () => ++calls === 1 ? old : Promise.resolve(latest) });
  const first = audio.applyOutputEq(profile);
  await Promise.resolve();
  await audio.applyOutputEq({ ...profile, preampDb: -6 });
  resolveOld(new ArrayBuffer(1));
  await first;
  assert.equal(audio.convBuffer, latest);
  assert.equal(audio.profile.preampDb, -6);
});
test('bypass and a PEQ-only profile remove a previously decoded room IR', async () => {
  for (const next of [null, { ...profile, roomIr: undefined }]) {
    const audio = driver();
    globalThis.fetch = async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(1) });
    await audio.applyOutputEq(profile);
    assert.ok(audio.convBuffer);
    await audio.applyOutputEq(next);
    assert.equal(audio.convBuffer, null);
  }
});

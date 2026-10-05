// SPDX-License-Identifier: AGPL-3.0-or-later
import {readFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {test} from 'node:test';
import assert from 'node:assert/strict';
const ts = createRequire(new URL('../../crates/musicata-server/web/package.json', import.meta.url))('typescript');
const source = readFileSync(new URL('../../crates/musicata-server/web/src/lib/radio.svelte.ts', import.meta.url), 'utf8');
function store(api) {
  const js=ts.transpileModule(source, {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}}).outputText;
  const exports={};
  new Function('exports','require','$state',js)(exports, () => ({api}), value=>value);
  return exports.radio;
}
for (const mutation of ['save', 'remove']) test(`a stale station load cannot undo ${mutation}`, async () => {
  let resolve;
  const pending = new Promise(done=>resolve=done);
  const station={id:'radio',name:'FM',stream_url:'http://example.test/live'};
  const radio=store({radio:()=>pending,createRadio:async()=>station,deleteRadio:async()=>null});
  if (mutation==='remove') radio.stations=[station];
  const load=radio.load();
  if (mutation==='save') await radio.save(station); else await radio.remove(station.id);
  resolve(mutation==='save' ? [] : [station]);
  await load;
  assert.deepEqual(radio.stations, mutation==='save' ? [station] : []);
});
test('radio transport reports a rejected play command', async () => {
  const source=readFileSync(new URL('../../crates/musicata-server/web/src/lib/playback.ts', import.meta.url),'utf8');
  const js=ts.transpileModule(source,{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}}).outputText;
  const exports={};
  new Function('exports','require',js)(exports, () => ({
    api:{}, sendCommand:async()=>false, player:{target:{kind:'player',id:'offline'},isBrowserOutput:false},radioMix:{cancel(){}},nav:{}
  }));
  assert.equal(await exports.playStream('/api/radio/test/stream','FM'),false);
});

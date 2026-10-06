// SPDX-License-Identifier: AGPL-3.0-or-later
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { test } from 'node:test';
import assert from 'node:assert/strict';
const require = createRequire(new URL('../../crates/musicata-server/web/package.json', import.meta.url));
const ts = require('typescript');
const cache = new Map();
function load(name) {
  if (cache.has(name)) return cache.get(name);
  const source = readFileSync(new URL(`../../crates/musicata-server/web/src/lib/${name}.ts`, import.meta.url), 'utf8');
  const js = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText;
  const exports = {};
  cache.set(name, exports);
  new Function('exports', 'require', js)(exports, dependency => {
    assert.ok(dependency.startsWith('./'), 'only local browser modules are expected');
    return load(dependency.slice(2));
  });
  return exports;
}
const { BrowserAudio } = load('audio');
load('diagnostics').setDiagnosticRendererToken('test-renderer');
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

test('browser decode failures report a safe category and cancellation stays quiet', async () => {
  globalThis.document = { addEventListener() {} };
  const handlers = new Map();
  const el = { error: { code: 3 }, addEventListener(name, handler) { handlers.set(name, handler); } };
  const audio = new BrowserAudio(el);
  audio.rendererAllowed = true; audio.claimed = true;
  const reports = [];
  globalThis.fetch = async (url, options) => { reports.push({ url, body: JSON.parse(options.body) }); return { ok: true }; };
  handlers.get('error')();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(reports.length, 1);
  assert.equal(reports[0].url, '/api/diagnostics/reports');
  assert.equal(reports[0].body.category, 'browser.audio');
  assert.equal(reports[0].body.message, 'audio decode failed');
  el.error = { code: 1 }; handlers.get('error')();
  assert.equal(reports.length, 1, 'requested cancellation is not a failure');
});
test('device-routing failure does not wait for or propagate a failed diagnostic request', async () => {
  const audio = driver();
  audio.ctx.setSinkId = async () => { throw new Error('private device details'); };
  const reports = [];
  globalThis.fetch = async (url, options) => { reports.push(JSON.parse(options.body)); throw new Error('offline'); };
  await audio.setSink('test-device');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(reports.length, 1);
  assert.equal(reports[0].category, 'browser.routing');
  assert.ok(!JSON.stringify(reports[0]).includes('private device'));
  globalThis.fetch = async () => ({ok:true});
});

test('a recovery dropped by the full offline queue is eventually delivered', async () => {
  cache.delete('diagnostics');
  const {reportDiagnostic,setDiagnosticRendererToken}=load('diagnostics');
  setDiagnosticRendererToken('test-renderer');
  const delivered=[];
  globalThis.fetch=async (_url,options)=>{delivered.push(JSON.parse(options.body));return {ok:true};};
  reportDiagnostic('browser.audio',true);
  await new Promise(resolve=>setImmediate(resolve));
  let release;
  globalThis.fetch=(_url,options)=>new Promise(resolve=>{release=()=>{delivered.push(JSON.parse(options.body));resolve({ok:true});};});
  reportDiagnostic('browser.routing',true);
  for(let index=0;index<32;index++)reportDiagnostic('browser.routing',index%2===1);
  reportDiagnostic('browser.audio',false);
  globalThis.fetch=async (_url,options)=>{delivered.push(JSON.parse(options.body));return {ok:true};};
  release();
  for(let index=0;index<20;index++)await new Promise(resolve=>setImmediate(resolve));
  assert.ok(delivered.some(report=>report.category==='browser.audio'&&report.action==='recovery'));
});

test('completion of an old DSP revision cannot recover the current failed revision', async () => {
  globalThis.$state=value=>value;
  globalThis.location={protocol:'http:',host:'test.invalid'};
  let socket;
  globalThis.WebSocket=class {
    static OPEN=1;readyState=1;
    constructor(){socket=this;}send(){}close(){}
  };
  const {connectBrowserRenderer}=load('outputAudio.svelte');
  const delivered=[];
  globalThis.fetch=async (_url,options)=>{delivered.push(JSON.parse(options.body));return {ok:true};};
  let resolveOld;
  const old=new Promise(resolve=>{resolveOld=resolve;});let calls=0;
  const audio={isClaimed:true,setRendererAllowed(){},levels(){return null;},applyOutputEq(){return ++calls===1?old:Promise.reject(new Error('new revision failed'));}};
  const channel=connectBrowserRenderer('browser-local',audio);
  function configuration(revision){return {state:{session_id:'renderer',desired_revision:revision,selection:{enabled:true}},profile:null,meter_subscribed:false};}
  socket.onmessage({data:JSON.stringify({type:'renderer_granted',reporter_token:'test-owner',config:configuration(1)})});
  socket.onmessage({data:JSON.stringify({type:'dsp_config',config:configuration(2)})});
  await new Promise(resolve=>setImmediate(resolve));
  resolveOld();await new Promise(resolve=>setImmediate(resolve));
  channel.close();
  const reports=delivered.filter(report=>report.category==='browser.dsp');
  assert.equal(reports.length,1);
  assert.equal(reports[0].action,'failure');
});

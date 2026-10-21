#!/usr/bin/env node
// Headless checks for hub/console/index.html (the websocket protocol —
// docs/guides/signals-websocket.md, pinned by hub/src/console/protocol.rs).
// Node stdlib only.
//   node tools/console-test/test.js
// 1. extracts both <script> blocks and syntax-checks them with `node --check`;
// 2. loads the pure <script id="logic"> block and asserts the sync-point delay and
//    time<->pixel math, the mock hub's message shapes and lead/lag behaviour,
//    and that every page->hub message matches the protocol table (owned HERE, not
//    imported from the page, so the page can't grade its own homework).
// 3. runs the real page (<script id="ui">) in a fake browser (fakedom.js) under
//    load and counts its DOM writes: only what is on screen, only what changed.
'use strict';
const fs = require('fs'), path = require('path'), os = require('os'), vm = require('vm'), cp = require('child_process');

const root = path.resolve(__dirname, '..', '..');
const html = fs.readFileSync(path.join(root, 'hub', 'console', 'index.html'), 'utf8');

let pass = 0, fail = 0;
function ok(cond, name, extra) { if (cond) { pass++; } else { fail++; console.log('FAIL ' + name + (extra !== undefined ? '  ' + JSON.stringify(extra) : '')); } }
function near(a, b, tol, name) { ok(a !== null && Math.abs(a - b) <= tol, name, { got: a, want: b, tol: tol }); }
function section(s) { console.log('-- ' + s); }

function script(id) {
  const m = html.match(new RegExp('<script id="' + id + '">([\\s\\S]*?)</script>'));
  if (!m) throw new Error('no <script id="' + id + '"> in index.html');
  return m[1];
}

// ---- 0. self-containment ----------------------------------------------------
section('self-contained');
ok(!/(src|href)\s*=\s*["']?(https?:)?\/\//i.test(html), 'no external src/href URLs');
ok(!/@import|url\(\s*["']?https?:/i.test(html), 'no external CSS imports');
ok((html.match(/<script/g) || []).length === 2, 'exactly two inline scripts');

// ---- 1. syntax --------------------------------------------------------------
section('node --check');
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'console-test-'));
for (const id of ['logic', 'ui']) {
  const f = path.join(tmp, id + '.js');
  fs.writeFileSync(f, script(id));
  const r = cp.spawnSync(process.execPath, ['--check', f], { encoding: 'utf8' });
  ok(r.status === 0, 'node --check <script id="' + id + '">', r.stderr);
}

// ---- load logic ---------------------------------------------------------------
const mod = { exports: {} };
vm.runInNewContext(script('logic'), { module: mod, Math, Number, JSON, Map, Set, Array, Object, String, Float32Array, Float64Array, isFinite, Infinity });
const L = mod.exports;

// ---- 2. delay math ------------------------------------------------------------
section('delay math (two sync points)');
const sync = { signal_delay_ms: 70, td_round_trip_ms: 120 };
const engine = { name: 'engine', kind: 'engine' };
const mon = { name: 'monitor', kind: 'monitor' };
const td = { name: 'td', kind: 'td' };
const devTd = { name: 'quest', kind: 'device', follow: 'td' };
const devMusic = { name: 'editor', kind: 'device', follow: 'music' };
ok(L.signalDelayMs(engine, sync) === 0 && L.audioDelayMs(engine, sync) === 0, 'engine never delayed');
ok(L.signalDelayMs(mon, sync) === 0 && L.audioDelayMs(mon, sync) === 0, 'monitor never delayed');
ok(L.signalDelayMs(td, sync) === 70 && L.audioDelayMs(td, sync) === 0, 'td: signals +signal delay, audio 0');
ok(L.signalDelayMs(devTd, sync) === 190 && L.audioDelayMs(devTd, sync) === 120, 'device follow=td: signals +signal+round trip, audio +round trip');
ok(L.signalDelayMs(devMusic, sync) === 70 && L.audioDelayMs(devMusic, sync) === 0, 'device follow=music: exactly what TD gets');
ok(L.signalDelayMs({ name: 'x', kind: 'device' }, sync) === 190, 'device without follow defaults to td');
ok(L.signalDelayMs(td, { signal_delay_ms: 0, td_round_trip_ms: 0 }) === 0 && L.signalDelayMs(null, sync) === 0, 'zero sync / no dest');
ok(L.shiftTs(1000000, 70) === 1070000, 'shiftTs: ms -> us');
ok(L.clampDelay(-5) === 0 && L.clampDelay(6000) === 5000 && L.clampDelay(12.4) === 12 && L.clampDelay('x') === 0, 'clampDelay 0..5000 integer');
ok(L.signalClass('/tussen/midi/cc/1') === 'midi' && L.signalClass('/tussen/clock/beat') === 'midi' && L.signalClass('/tussen/quest/head/yaw') === 'osc' && L.signalClass('/ext/a') === 'osc', 'signal classes per the protocol table');

// ---- 3. time <-> pixel ---------------------------------------------------------
section('time <-> pixel');
const view = { endUs: 10e6, windowUs: 2e6 }, W = 1600;
ok(L.timeToX(10e6, view, W) === W && L.timeToX(8e6, view, W) === 0, 'window ends map to edges');
ok(Math.abs(L.timeToX(9e6, view, W) - 800) < 1e-9, 'midpoint');
let rt = 0; for (let x = 0; x <= W; x += 37) rt = Math.max(rt, Math.abs(L.timeToX(L.xToTime(x, view, W), view, W) - x));
ok(rt < 1e-6, 'x -> t -> x round trip', rt);
// a 70 ms delay moves a marker by exactly 70 ms worth of pixels
ok(Math.abs(L.timeToX(L.shiftTs(9e6, 70), view, W) - L.timeToX(9e6, view, W) - 70e3 / 2e6 * W) < 1e-9, 'delay shift in pixels');
const z = L.zoomView(view, 0.05, 400, W);
ok(Math.abs(L.xToTime(400, z, W) - L.xToTime(400, view, W)) < 1e-6 && z.windowUs === 100e3, 'zoom keeps the anchor time fixed (2 s -> 100 ms)');
ok(L.zoomView(view, 1e-6, 0, W).windowUs === L.MIN_WINDOW_US && L.zoomView(view, 1e6, 0, W).windowUs === L.MAX_WINDOW_US, 'zoom clamps');
ok(L.gridStepMs(2e6, 1600, 70) === 100 && L.gridStepMs(100e3, 1600, 70) === 5, 'grid steps');
ok(Math.abs(L.blockDurUs(48000) - 5333.333) < 0.001, 'block = 256 frames');

// ---- 4. page -> hub messages against the protocol table ------------------------
section('page -> hub protocol');
const SPEC = {   // t -> {required, optional}
  watch: { req: ['addrs'], opt: [] },
  set_sync: { req: [], opt: ['signal_delay_ms', 'td_round_trip_ms'] },
  set_follow: { req: ['dest', 'follow'], opt: [] },
  remap_upsert: { req: ['dest', 'from', 'to', 'keep'], opt: ['id', 'range'] },
  remap_delete: { req: ['id'], opt: [] },
  save: { req: [], opt: [] },
  set_tempo: { req: ['bpm'], opt: [] },
  set_playing: { req: ['playing'], opt: [] },
  record_start: { req: [], opt: ['name'] },
  record_stop: { req: [], opt: [] },
  resolve: { req: [], opt: [] },
};
function conforms(m) {
  const s = SPEC[m.t]; if (!s) return 'unknown t ' + m.t;
  for (const k of s.req) if (!(k in m)) return m.t + ' missing ' + k;
  for (const k of Object.keys(m)) if (k !== 't' && !s.req.includes(k) && !s.opt.includes(k)) return m.t + ' extra field ' + k;
  if (m.t === 'set_sync') {
    const ks = ['signal_delay_ms', 'td_round_trip_ms'].filter(k => k in m);
    if (!ks.length) return 'set_sync with nothing to set';
    for (const k of ks) if (!(Number.isInteger(m[k]) && m[k] >= 0 && m[k] <= 5000)) return 'set_sync ' + k + ' must be an integer 0..5000';
  }
  if (m.t === 'set_follow') {
    if (typeof m.dest !== 'string' || !m.dest) return 'set_follow dest must be a name';
    if (m.follow !== 'td' && m.follow !== 'music') return 'set_follow follow must be td|music';
  }
  if ('range' in m && !(Array.isArray(m.range) && m.range.length === 2)) return 'range must be [lo, hi]';
  if (m.t === 'set_tempo' && !(typeof m.bpm === 'number' && m.bpm >= 20 && m.bpm <= 999)) return 'bpm must be a number in 20..999';
  if (m.t === 'set_playing' && typeof m.playing !== 'boolean') return 'playing must be a bool';
  if (m.t === 'record_start' && 'name' in m && !(typeof m.name === 'string' && m.name.trim())) return 'record_start name must be a non-empty string';
  return null;
}
const sent = [
  L.msg.watch(['/ext/a', '/tussen/quest/controller/trigger']),
  L.msg.setSync({ signal_delay_ms: 70 }),
  L.msg.setSync({ td_round_trip_ms: 25.6 }),
  L.msg.setSync({ signal_delay_ms: 9999, td_round_trip_ms: -3 }),
  L.msg.setFollow('editor', 'music'),
  L.msg.remapUpsert({ dest: 'td', from: '/tussen/*/head/n/yaw', to: '/td/yaw', range: [0, 1], keep: false }),
  L.msg.remapUpsert({ id: 3, dest: 'engine', from: '/ext/a', to: '/engine/a', keep: true }),
  L.msg.remapUpsert({ id: 'tmp-4', dest: 'engine', from: '/ext/a', to: '/engine/a', keep: true }),
  L.msg.remapDelete(3),
  L.msg.save(),
  L.msg.setTempo(128.5),
  L.msg.setTempo(5),
  L.msg.setTempo(5000),
  L.msg.setPlaying(false),
  L.msg.setPlaying(1),
  L.msg.recordStart(), L.msg.recordStart('  sound check '), L.msg.recordStop(),
  L.msg.resolve(),
];
for (const m of sent) ok(conforms(m) === null, 'conforms: ' + JSON.stringify(m), conforms(m));
ok(sent[1].signal_delay_ms === 70 && !('td_round_trip_ms' in sent[1]), 'set_sync carries only the moved field');
ok(sent[2].td_round_trip_ms === 26 && !('signal_delay_ms' in sent[2]), 'sync sent as integer ms');
ok(sent[3].signal_delay_ms === 5000 && sent[3].td_round_trip_ms === 0, 'set_sync clamps 0..5000');
ok(sent[4].follow === 'music' && L.msg.setFollow('editor', 'x').follow === 'td', 'setFollow sends td|music');
ok(conforms({ t: 'set_sync' }) !== null && conforms({ t: 'set_follow', dest: 'q', follow: 'x' }) !== null && conforms({ t: 'set_sync', signal_delay_ms: 1.5 }) !== null, 'table rejects empty set_sync / bad follow / fractional ms');
ok(!('id' in sent[5]) && sent[6].id === 3, 'no id = new; id echoed verbatim');
ok(!('id' in sent[7]), 'optimistic temp ids never reach the hub');
ok(!('range' in sent[6]), 'no range when not given');
ok(sent[10].bpm === 128.5 && sent[11].bpm === 20 && sent[12].bpm === 999 && L.msg.setTempo('x').bpm === 20, 'setTempo clamps 20..999');
ok(sent[13].playing === false && sent[14].playing === true, 'setPlaying sends a bool');
{ // the mock's ALLOWED list accepts everything the page sends
  const h = L.createMockHub(); h.tick(0); for (const m of sent) h.onMessage(JSON.parse(JSON.stringify(m)));
  ok(h.errors.length === 0, 'mock ALLOWED in sync with every page message', h.errors);
  h.onMessage({ t: 'set_tempo', bpm: 5 }); h.onMessage({ t: 'set_playing', playing: 'yes' });
  ok(h.errors.length === 2, 'mock rejects bad set_tempo / set_playing', h.errors);
}
// the form parser feeds remapUpsert
const pf = L.parseRuleForm({ dest: 'td', from: ' /ext/tablet/* ', to: '/td/x', lo: '0.2', hi: '0.8', keep: true });
ok(pf.rule && conforms(L.msg.remapUpsert(pf.rule)) === null && pf.rule.range[0] === 0.2 && pf.rule.from === '/ext/tablet/*', 'rule form -> remap_upsert');
ok(L.parseRuleForm({ dest: 'td', from: '/a', to: '/b', lo: '1', hi: '' }).error, 'half a range is rejected');
ok(L.parseRuleForm({ dest: 'td', from: 'a', to: '/b' }).error && L.parseRuleForm({ dest: '', from: '/a', to: '/b' }).error, 'bad from / no dest rejected');
ok(L.matchPattern('/tussen/*/head/n/yaw', '/tussen/quest/head/n/yaw') && !L.matchPattern('/tussen/*/yaw', '/tussen/quest/head/yaw') && !L.matchPattern('/a/*', '/a'), '* matches exactly one segment');

// ---- 5. mock hub: hub -> page shapes ---------------------------------------------
section('mock hub');
function runMock(opts, seconds, onTick) {
  const hub = L.createMockHub(opts), S = L.createState(), msgs = [];
  const t0 = 5e6; let localMs = 0;
  for (let t = t0; t <= t0 + seconds * 1e6; t += 15e3) {
    if (onTick) onTick(hub, t - t0, S);
    for (const m of hub.tick(t)) { const j = JSON.parse(JSON.stringify(m)); msgs.push(j); L.applyMessage(S, j, localMs); }
    localMs += 15;
  }
  return { hub, S, msgs };
}
const HUB_SPEC = {
  hello: ['sync', 'dests', 'remaps', 'sample_rate', 'osc'],
  dests: ['dests'],
  audio: ['ts', 'blocks'],
  midi: ['ts', 'addr', 'v'],
  osc: ['ts', 'addr', 'v', 'src'],
  addrs: ['full', 'list'],
  saved: ['ok'],
  link: ['ts', 'beat', 'bpm', 'quantum', 'playing', 'mode', 'peers', 'peer_list'],
  dest_status: ['list'],
};
const run = runMock({}, 25, (hub, el) => {
  if (el === 15e3) hub.onMessage(L.msg.watch(['/ext/tablet/fader1', '/tussen/quest/controller/trigger']));
  if (el === 3e6) hub.onMessage(L.msg.save());
});
const types = new Set(run.msgs.map(m => m.t));
for (const t of Object.keys(HUB_SPEC)) ok(types.has(t), 'mock emits ' + t);
let shapeErr = null;
for (const m of run.msgs) {
  const f = HUB_SPEC[m.t]; if (!f) { shapeErr = 'unknown ' + m.t; break; }
  for (const k of f) if (!(k in m)) shapeErr = m.t + ' missing ' + k;
  if (m.t === 'saved' && !('path' in m) && !('error' in m)) shapeErr = 'saved needs path or error';
  if (m.t === 'hello' && !(m.sync && typeof m.sync.signal_delay_ms === 'number' && typeof m.sync.td_round_trip_ms === 'number')) shapeErr = 'hello.sync needs signal_delay_ms + td_round_trip_ms';
  if (m.t === 'hello' && !(m.osc && typeof m.osc.port === 'number' && Array.isArray(m.osc.ips))) shapeErr = 'hello.osc needs port + ips';
  if (m.t === 'hello' || m.t === 'dests') for (const d of m.dests) {
    for (const k of ['name', 'kind']) if (!(k in d)) shapeErr = 'dest missing ' + k;
    if (!['engine', 'td', 'monitor', 'device'].includes(d.kind)) shapeErr = 'dest kind ' + d.kind;
    if (d.kind === 'device' && d.follow !== 'td' && d.follow !== 'music') shapeErr = 'device dest needs follow td|music: ' + JSON.stringify(d);
    if (d.kind !== 'device' && 'follow' in d) shapeErr = 'follow only on devices: ' + JSON.stringify(d);
  }
  if (m.t === 'hello') for (const r of m.remaps) { for (const k of ['id', 'dest', 'from', 'to', 'keep']) if (!(k in r)) shapeErr = 'remap missing ' + k; if ('range' in r && r.range.length !== 2) shapeErr = 'remap range'; }
  if (m.t === 'audio') for (const b of m.blocks) if (b.length !== 3 || !(b[0] <= b[1]) || b[2] < 0) shapeErr = 'audio block ' + JSON.stringify(b);
  if (m.t === 'link' && !(['ts', 'beat', 'bpm', 'quantum'].every(k => typeof m[k] === 'number') && typeof m.playing === 'boolean' && Number.isInteger(m.peers))) shapeErr = 'link types ' + JSON.stringify(m);
  if (m.t === 'link' && !L.LINK_MODES.includes(m.mode)) shapeErr = 'link mode must be waiting|joined|started: ' + m.mode;
  if (m.t === 'link') for (const p of m.peer_list) {
    if (!(typeof p.node === 'string' && typeof p.session === 'string' && typeof p.in_session === 'boolean' && typeof p.local === 'boolean' && typeof p.bpm === 'number' && Number.isInteger(p.age_ms))) shapeErr = 'link peer types ' + JSON.stringify(p);
    for (const k of ['host', 'hostname', 'playing']) if (k in p && p[k] === null) shapeErr = 'link peer ' + k + ' must be absent, not null';
  }
  if (m.t === 'link' && m.peers !== m.peer_list.filter(p => p.in_session).length) shapeErr = 'link peers = in-session peer_list entries: ' + JSON.stringify(m);
  if (m.t === 'addrs') for (const a of m.list) {
    for (const k of ['addr', 'last_ts', 'rate', 'v', 'min', 'max', 'count']) if (!(k in a)) shapeErr = 'addrs entry missing ' + k;
    if (!['in', 'out', 'both'].includes(a.dir)) shapeErr = 'addrs entry dir must be in|out|both: ' + JSON.stringify(a);
  }
  if (shapeErr) break;
}
ok(shapeErr === null, 'every mock message has the protocol fields', shapeErr);
const kinds = new Set(run.msgs.filter(m => m.t === 'dests').map(m => m.dests.map(d => d.name).join(',')));
ok([...kinds].some(k => k.includes('quest')), 'a device registers (dests)');
ok(run.msgs.filter(m => m.t === 'osc').every(m => m.addr === '/ext/tablet/fader1' || m.addr === '/tussen/quest/controller/trigger'), 'osc only for watched addresses');
ok(run.msgs.some(m => m.t === 'osc' && m.addr === '/tussen/quest/controller/trigger'), 'quest trigger arrives while registered');
{ // a watched MIDI address is laned from `midi`; an `osc` copy of it is not drawn twice
  const S4 = L.createState(); S4.watch = ['/tussen/midi/cc/1'];
  L.applyMessage(S4, { t: 'midi', ts: 1, addr: '/tussen/midi/cc/1', v: 0.5 }, 0);
  L.applyMessage(S4, { t: 'osc', ts: 1, addr: '/tussen/midi/cc/1', v: 0.5, src: 'hub' }, 0);
  L.applyMessage(S4, { t: 'midi', ts: 2, addr: '/tussen/midi/cc/2', v: 0.5 }, 0);
  ok(S4.osc.get('/tussen/midi/cc/1').ts.length === 1 && !S4.osc.has('/tussen/midi/cc/2'), 'MIDI lanes from midi, once, only when watched');
}
// audio continuity: block k's ts == first ts + k * 256/sr
const aud = run.msgs.filter(m => m.t === 'audio'), bd = L.blockDurUs(48000);
let gap = 0, expect = aud[0].ts;
for (const m of aud) { gap = Math.max(gap, Math.abs(m.ts - expect)); expect = m.ts + m.blocks.length * bd; }
ok(gap < 1e-3, 'audio blocks contiguous', gap);
const ap = aud.length / 25; ok(ap > 25 && ap < 35, 'audio batched at ~30 Hz', ap);
ok(Math.abs(run.S.audio.count - 25 * 48000 / 256) < 5, 'block count = 187.5/s', run.S.audio.count);
// the address list is permanent and sorted: it only ever grows, never
// reorders, and every entry carries min ≤ v ≤ max
const lists = run.msgs.filter(m => m.t === 'addrs').map(m => m.list.map(a => a.addr));
ok(lists.every(l => l.join() === l.slice().sort().join()), 'every addrs list arrives sorted by address');
{ // the first addrs frame is the whole table; the rest only what changed, and merged they only grow
  const fr = run.msgs.filter(m => m.t === 'addrs');
  ok(fr[0].full === true && fr.slice(1).every(m => m.full === false), 'addrs: first frame full, then diffs', fr.map(m => m.full));
  ok(fr.slice(1).some(m => m.list.length < run.S.addrs.length), 'a diff frame leaves unchanged addresses out', fr.map(m => m.list.length));
  const merged = new Set(); let grows = true;
  for (const m of fr) { const n = merged.size; for (const a of m.list) merged.add(a.addr); grows = grows && merged.size >= n; }
  ok(grows && run.S.addrs.length === merged.size, 'merged, the table only grows; nothing ever drops out', { page: run.S.addrs.length, merged: merged.size });
  ok(run.S.addrs.map(a => a.addr).join() === [...merged].sort().join(), 'the page holds the merged table, sorted');
}
{ // applying diffs: replaced in place, new ones sorted in, a full frame replaces
  const Sd = L.createState(), e = (addr, v) => ({ addr, last_ts: 1, rate: 1, v, min: 0, max: 1, count: 1, dir: 'in' });
  L.applyMessage(Sd, { t: 'addrs', full: true, list: [e('/b', 0.1), e('/d', 0.2)] }, 0);
  L.applyMessage(Sd, { t: 'addrs', full: false, list: [e('/a', 0.3), e('/d', 0.9)] }, 500);
  ok(Sd.addrs.map(a => a.addr + '=' + a.v).join() === '/a=0.3,/b=0.1,/d=0.9', 'diff merged by addr, sorted', Sd.addrs);
  L.applyMessage(Sd, { t: 'addrs', full: false, list: [e('/b', 0.5)] }, 1000);
  ok(Sd.addrs[1].v === 0.5 && Sd.addrs.length === 3, 'a known address is replaced in place');
  L.applyMessage(Sd, { t: 'addrs', full: true, list: [e('/z', 1)] }, 1500);
  ok(Sd.addrs.length === 1 && Sd.addrSeen.size === 4, 'a full frame (a restarted hub) replaces the table; addrSeen keeps all');
}
ok(run.msgs.filter(m => m.t === 'addrs').every(m => m.list.every(a => a.min <= a.v && a.v <= a.max)), 'min ≤ last value ≤ max on every entry');
{ // min/max accumulate over an address's whole life
  const knob = run.msgs.filter(m => m.t === 'addrs').map(m => m.list.find(a => a.addr === '/ext/sensor/knob')).filter(Boolean).pop();
  ok(knob && knob.min < knob.max, 'a moving control has min < max', knob);
}
ok(run.S.addrHeat.has('/ext/sensor/knob'), 'wiggled control gets heat');
ok(!run.S.addrHeat.has('/ext/tablet/fader1') && !run.S.addrHeat.has('/tussen/clock/bpm'), 'steady streams never heat');
{ // replay heat on a timeline: the knob resumes every 7 s; while hot it sorts first
  const hub = L.createMockHub(), S3 = L.createState(); let seenTop = false;
  for (let t = 0, ms = 0; t <= 16e6; t += 15e3, ms += 15) for (const m of hub.tick(t)) { if (L.applyMessage(S3, m, ms) === 'addrs' && L.sortedAddrs(S3.addrs, '', S3.addrHeat, ms)[0].addr === '/ext/sensor/knob' && L.isHot(S3.addrHeat, '/ext/sensor/knob', ms)) seenTop = true; }
  ok(seenTop, 'wiggled control jumps to the top while hot');
}
const top = L.sortedAddrs([{ addr: '/c', last_ts: 5, rate: 0 }, { addr: '/a', last_ts: 1, rate: 0 }, { addr: '/b', last_ts: 9, rate: 0 }], '');
ok(top.map(a => a.addr).join() === '/a,/b,/c', 'stable address order, not recency');
ok(L.sortedAddrs([{ addr: '/a', last_ts: 1, rate: 0 }, { addr: '/b', last_ts: 9, rate: 0 }], '', new Map([['/b', 1000]]), 1500)[0].addr === '/b', 'hot sorts above address order');
ok(L.sortedAddrs(run.S.addrs, 'FADER').every(a => /fader/.test(a.addr)), 'filter is case-insensitive substring');
ok(run.S.saved && run.S.saved.ok && /\.toml$/.test(run.S.saved.path), 'save -> saved{ok,path}');
ok(run.hub.errors.length === 0, 'mock saw no protocol errors', run.hub.errors);

// ---- 5b. OSC endpoint + history tab ------------------------------------------------
section('osc endpoint + history');
ok(L.endpointText({ port: 9000, ips: ['192.0.2.10', '203.0.113.3'], hostname: 'hub.local' }) === '192.0.2.10:9000', 'endpointText: primary ip:port');
ok(L.endpointText({ port: 9000, ips: [], hostname: 'hub.local' }) === 'hub.local:9000', 'endpointText: hostname when no LAN ip');
ok(L.endpointText({ port: 9000, ips: [] }) === null && L.endpointText(null) === null && L.endpointText({ port: 0, ips: ['1.2.3.4'] }) === null, 'endpointText: nothing to show');
ok(L.endpointLines({ port: 9000, ips: ['192.0.2.10', '203.0.113.3'], hostname: 'hub.local' }).join('|') === '192.0.2.10:9000|203.0.113.3:9000|hub.local:9000', 'endpointLines: every ip, then the hostname');
ok(run.S.oscEndpoint && run.S.oscEndpoint.port === 9000 && run.S.oscEndpoint.ips[0] === '192.0.2.10' && run.S.oscEndpoint.hostname === 'mock-hub.local', 'hello carries the endpoint into state');
{ // the history ring: every osc + midi message lands, newest first, capped, filtered
  const Sh = L.createState();
  L.applyMessage(Sh, { t: 'osc', ts: 1, addr: '/djay/turntable1/fx/1/type', v: 0.3, src: '192.0.2.31:9' }, 0);
  L.applyMessage(Sh, { t: 'midi', ts: 2, addr: '/tussen/midi/cc/1', v: 0.5 }, 0);
  ok(Sh.history.total === 2 && Sh.history.buf[0].src === '192.0.2.31:9' && Sh.history.buf[1].src === 'midi', 'osc + midi both land, midi labelled');
  const rows = L.historyRows(Sh, '', 10);
  ok(rows.length === 2 && rows[0].addr === '/tussen/midi/cc/1', 'newest first');
  ok(L.historyRows(Sh, 'DJAY', 10).length === 1 && L.historyRows(Sh, 'nope', 10).length === 0, 'filter is case-insensitive substring');
  for (let i = 0; i < L.HISTORY_CAP + 2000; i++) L.applyMessage(Sh, { t: 'osc', ts: 10 + i, addr: '/x', v: i, src: 's' }, 0);
  ok(Sh.history.buf.length <= L.HISTORY_CAP + L.HISTORY_SLACK && L.HISTORY_CAP <= 500 && Sh.history.total === L.HISTORY_CAP + 2002, 'ring capped, total keeps counting', Sh.history.buf.length);
  ok(L.historyRows(Sh, '', 5).length === 5 && L.historyRows(Sh, '', 5)[0].v === L.HISTORY_CAP + 1999, 'limit + newest kept');
}
{ // a lane's series: only watched addresses get one, and trimming is batched —
  // a per-message splice made a hot address cost O(window) per message once
  // it was KEEP_US old (Chris: "very unresponsive … lots of osc signals")
  const Ss = L.createState(); Ss.watch = ['/ext/hot'];
  const rate = 3000, stepUs = 1e6 / rate, n = Math.ceil((L.KEEP_US / 1e6 + 30) * rate), win = L.KEEP_US / stepUs;
  let trims = 0, prev = 0, maxLen = 0;
  for (let i = 0; i < n; i++) {
    L.applyMessage(Ss, { t: 'osc', ts: 1e6 + i * stepUs, addr: '/ext/hot', v: 0.5, src: 's' }, 0);
    L.applyMessage(Ss, { t: 'osc', ts: 1e6 + i * stepUs, addr: '/ext/other' + (i % 50), v: 0.5, src: 's' }, 0);
    const len = Ss.osc.get('/ext/hot').ts.length; if (len <= prev) trims++;   // a push that didn't grow the series trimmed it
    prev = len; maxLen = Math.max(maxLen, len);
  }
  ok(Ss.osc.size === 1, 'only watched addresses get a series ("*" feeds the history, not the lanes)', [...Ss.osc.keys()].slice(0, 3));
  ok(trims > 0 && trims <= 20, 'a hot series is trimmed in batches, not per message', trims);
  ok(maxLen <= Math.ceil(win * 4 / 3) + 64 && prev >= win - 1, 'series stays bounded and keeps the window', { maxLen, prev, win });
  ok(Ss.history.total === 2 * n, 'every message still lands in the history');
  L.applyLocalEdit(Ss, L.msg.watch([]));
  ok(Ss.osc.size === 0, 'unwatching a lane drops its series');
}
{ // tables row by row (patchRows on a fake DOM): a redraw writes only what changed
  let writes = 0, moves = 0;
  const el = (tag) => {
    const e = { tag, children: [], parent: null, attrs: {}, _html: '', _cls: '',
      get innerHTML() { return this._html; }, set innerHTML(v) { writes++; this._html = v; },
      get className() { return this._cls; }, set className(v) { writes++; this._cls = v; },
      setAttribute(k, v) { this.attrs[k] = v; },
      appendChild(c) { return this.insertBefore(c, null); },
      insertBefore(c, ref) { if (c.parent) c.parent.children.splice(c.parent.children.indexOf(c), 1); const i = ref ? this.children.indexOf(ref) : -1; if (i < 0) this.children.push(c); else this.children.splice(i, 0, c); c.parent = this; if (this.tag === 'tbody') moves++; return c; },
      removeChild(c) { this.children.splice(this.children.indexOf(c), 1); c.parent = null; moves++; return c; } };
    return e;
  };
  const doc = { createElement: el }, tb = el('tbody');
  const list = Array.from({ length: 300 }, (_, i) => ({ addr: '/ext/a' + String(i).padStart(3, '0'), last_ts: 1e6, rate: 2, v: 0.5, min: 0, max: 1, count: 3, dir: 'in' }));
  const ctx = (now, heat) => ({ now, heat: heat || new Map(), watch: ['/ext/a007'], remaps: [{ from: '/ext/*' }], ref: 2e6 });
  const rows = (c, l) => (l || list).map(a => L.addrRow(a, c));
  L.patchRows(tb, rows(ctx(0)), doc);
  const cellText = (r, j) => r.children[j]._html;
  ok(tb.children.length === 300 && tb.children[7].attrs['data-a'] === '/ext/a007' && /★/.test(cellText(tb.children[7], 0)) && /⇢/.test(cellText(tb.children[0], 0)), 'patchRows builds the table (watch star, remap arrow)');
  writes = 0; moves = 0;
  L.patchRows(tb, rows(ctx(0)), doc);
  ok(writes === 0 && moves === 0, 'nothing changed: a redraw writes nothing', { writes, moves });
  const l2 = list.slice(); l2[42] = Object.assign({}, l2[42], { max: 2 });
  L.patchRows(tb, rows(ctx(0), l2), doc);
  ok(writes === 1 && moves === 0 && cellText(tb.children[42], 4) === '2.000', 'one value changed: one cell written', { writes, moves });
  writes = 0;
  const heat = new Map([['/ext/a200', 0]]), hot = L.sortedAddrs(l2, '', heat, 10);
  L.patchRows(tb, hot.map(a => L.addrRow(a, ctx(10, heat))), doc);
  ok(moves === 1 && writes === 1 && tb.children[0].attrs['data-a'] === '/ext/a200' && tb.children[0]._cls === 'hot ', 'a hot row jumps to the top: one move, one class', { writes, moves });
  moves = 0;
  L.patchRows(tb, hot.slice(0, 100).map(a => L.addrRow(a, ctx(10, heat))), doc);
  ok(tb.children.length === 100 && moves === 200 && tb._rows.size === 100, 'rows that fall off are removed', { n: tb.children.length, moves });
  const sr = L.seenRow({ addr: '/x<y>', dir: 'in', count: 4, v: 0.25, min: 0, max: 1, last_ts: 0 }, 1.5e6);
  ok(sr.cells.map(c => c[0]).join('|') === '/x&lt;y&gt;|in|osc|4|0.000|0.250|1.000|1.5', 'seenRow: escaped address, values, ago', sr.cells);
}
{ // only what is on screen redraws, nothing while the page is hidden
  const p = (oscTab, hidden, histPaused) => { const r = L.renderPlan({ oscTab, hidden, histPaused }); return [r.addrs, r.history, r.seen].map(Number).join(''); };
  ok(p(false, false, false) === '100' && p(true, false, false) === '011' && p(true, false, true) === '001', 'renderPlan: the visible tab only; a paused history stays');
  ok(p(false, true, false) === '000' && p(true, true, false) === '000', 'renderPlan: a hidden page renders nothing');
}
{ // the real page (<script id="ui">) in a fake browser (fakedom.js) at ~10k addresses and
  // 5k msg/s: the DOM is written only for what is on screen, and only when it changed
  section('page under load (fake DOM)');
  const { fakeBrowser } = require('./fakedom.js');
  const b = fakeBrowser(), ctx = vm.createContext(b.globals);
  vm.runInContext(script('logic'), ctx); vm.runInContext(script('ui'), ctx);
  b.open();
  b.recv({ t: 'hello', sync: { signal_delay_ms: 0, td_round_trip_ms: 0 }, dests: [], remaps: [], sample_rate: 48000, osc: { port: 9000, ips: [] }, record: { on: false, elapsed_ms: 0 } });
  const N = 10000, entry = (i, ts, v) => ({ addr: '/ext/n/' + String(i).padStart(5, '0'), last_ts: ts, rate: 5, v, min: 0, max: 1, count: 1, dir: 'in' });
  b.recv({ t: 'addrs', full: true, list: Array.from({ length: N }, (_, i) => entry(i, 1e6, 0.5)) });
  let ts = 1e6, k = 0, handlerMs = 0, msgs = 0;
  const second = (n) => {   // one second of traffic: 5k osc frames over the 10k addresses + 2 addrs diffs
    for (let half = 0; half < 2; half++) {
      const t0 = process.hrtime.bigint();
      for (let i = 0; i < 2500; i++) { ts += 200; b.recv({ t: 'osc', ts, addr: '/ext/n/' + String(k++ % N).padStart(5, '0'), v: Math.random(), src: '192.0.2.5:9000' }); }
      handlerMs += Number(process.hrtime.bigint() - t0) / 1e6; msgs += 2500;
      if (n) b.recv({ t: 'addrs', full: false, list: Array.from({ length: 200 }, (_, i) => entry((k + i) % N, ts, Math.random())) });
      b.advance(500);
    }
  };
  second(true);
  ok(b.writes('#addrs tbody') > 0 && b.writes('#hist tbody') === 0 && b.writes('#seen tbody') === 0, 'sync tab: only the remap list is drawn', { addrs: b.writes('#addrs tbody'), hist: b.writes('#hist tbody'), seen: b.writes('#seen tbody') });
  ok(b.rows('#addrs tbody').length === 300, 'the remap list shows its first 300 rows', b.rows('#addrs tbody').length);
  b.advance(3000); b.resetWrites(); b.advance(1000);
  ok(b.writes('#addrs tbody') === 0, 'nothing changed: the remap list is not touched', b.writes('#addrs tbody'));
  b.click('#tab-osc'); b.resetWrites(); second(true);
  ok(b.writes('#hist tbody') > 0 && b.writes('#seen tbody') > 0 && b.writes('#addrs tbody') === 0, 'osc tab: history + address set drawn, the remap list not', { addrs: b.writes('#addrs tbody'), hist: b.writes('#hist tbody'), seen: b.writes('#seen tbody') });
  ok(b.rows('#hist tbody').length === 300 && b.rows('#seen tbody').length === 500, 'history 300 rows, address set 500', [b.rows('#hist tbody').length, b.rows('#seen tbody').length]);
  b.setHidden(true); b.resetWrites(); second(true); second(true);
  ok(b.writes('#hist tbody') + b.writes('#seen tbody') + b.writes('#addrs tbody') + b.writes('#histcount') === 0, 'page hidden: no DOM writes at all for the tables', { hist: b.writes('#hist tbody'), seen: b.writes('#seen tbody') });
  b.setHidden(false);
  ok(b.writes('#hist tbody') > 0 && b.writes('#seen tbody') > 0, 'shown again: drawn at once');
  b.intersect('hist', false); b.resetWrites(); second(true);
  ok(b.writes('#hist tbody') === 0 && b.writes('#seen tbody') > 0, 'history scrolled out of view: not drawn; the address set still is');
  b.intersect('hist', true); b.resetWrites(); b.advance(250); b.resetWrites(); b.advance(1000);
  ok(b.writes('#hist tbody') === 0, 'no new messages: the history is not touched', b.writes('#hist tbody'));
  b.resetWrites(); for (let i = 0; i < 10; i++) { ts += 200; b.recv({ t: 'osc', ts, addr: '/ext/n/00001', v: 0.25, src: 's' }); } b.advance(250);
  ok(b.writes('#hist tbody') > 0 && b.writes('#hist tbody') <= 10 * 9 + 20, 'ten new messages: about ten rows written, not 300', b.writes('#hist tbody'));
  b.click('#tab-sync'); b.resetWrites(); second(true);
  ok(b.writes('#hist tbody') === 0 && b.writes('#seen tbody') === 0, 'back on the sync tab: the osc tables are left alone');
  const us = handlerMs * 1000 / msgs;
  ok(us < 40, 'per-message cost at 5k msg/s stays small (µs, whole onmessage path)', +us.toFixed(1));
}
{ // the newest last_ts without spreading the whole list into Math.max (a RangeError past ~100k addresses)
  const many = Array.from({ length: 200000 }, (_, i) => ({ addr: '/a' + i, last_ts: i }));
  let r = null; try { r = L.latestSeenTs({ latestTs: 5, addrs: many }); } catch (e) { r = e.message; }
  ok(r === 199999, 'latestSeenTs over 200k addresses', r);
}
// the accumulated address table never forgets, even though the mock's addrs
// list drops stale entries (the real hub's doesn't)
ok(run.S.addrSeen.size >= new Set(run.msgs.filter(m => m.t === 'addrs').flatMap(m => m.list.map(a => a.addr))).size, 'addrSeen holds every address any addrs list ever named');
ok([...run.S.addrSeen.values()].every(a => typeof a.count === 'number' && a.count >= 1 && 'v' in a), 'addrSeen entries carry count + last value');
// Direction: device/ext OSC arrives on :9000 (in); MIDI + clock are hub-made (out).
ok(run.S.addrSeen.get('/ext/tablet/fader1').dir === 'in' && run.S.addrSeen.get('/tussen/editor/head/n/yaw').dir === 'in', 'inbound addresses read in');
ok(run.S.addrSeen.get('/tussen/midi/cc/1').dir === 'out' && run.S.addrSeen.get('/tussen/clock/bpm').dir === 'out', 'hub-made addresses read out');
{ // an address that drops off the live list stays in addrSeen; the set sorts by address
  const Sa = L.createState();
  L.applyMessage(Sa, { t: 'addrs', list: [{ addr: '/z/once', last_ts: 1, rate: 1, v: 0.5, count: 3, dir: 'in' }] }, 0);
  L.applyMessage(Sa, { t: 'addrs', list: [{ addr: '/a/later', last_ts: 9, rate: 1, v: 0.1, count: 1, dir: 'out' }] }, 500);
  ok(Sa.addrs.length === 1 && Sa.addrSeen.get('/z/once').count === 3, 'dropped from addrs, kept in addrSeen');
  ok(L.seenRows(Sa, '').map(a => a.addr).join() === '/a/later,/z/once', 'the set is sorted by address, not recency');
  ok(L.seenRows(Sa, 'nope').length === 0 && L.seenRows(Sa, 'ONCE').length === 1, 'seenRows filters, case-insensitive');
}
{ // watch "*": every OSC message reaches the page, and ALLOWED accepts it
  const hub = L.createMockHub(), Sw = L.createState();
  hub.tick(0); hub.onMessage({ t: 'watch', addrs: ['*'] });
  for (let t = 15e3; t <= 2e6; t += 15e3) for (const m of hub.tick(t)) L.applyMessage(Sw, m, t / 1000);
  ok(hub.errors.length === 0, 'watch ["*"] is protocol-clean', hub.errors);
  const addrs = new Set(Sw.history.buf.filter(e => e.src !== 'midi').map(e => e.addr));
  ok(addrs.has('/ext/tablet/fader1') && addrs.has('/tussen/editor/head/n/yaw'), 'unwatched addresses arrive under "*"', [...addrs]);
}

// ---- 6. lead / lag: the thing the console exists for ------------------------------
section('lead / lag');
const S = run.S, endUs = S.latestTs, fromUs = endUs - 10e6;
const ons = L.detectOnsets(S.audio, fromUs, endUs);
ok(Math.abs(ons.length - 20) <= 1, 'one onset per kick (10 s @ 120 bpm)', ons.length);
const kickErr = Math.max(...ons.map(o => { const k = Math.round((o - run.hub.kickTimeUs(0)) / 500e3); return Math.abs(o - run.hub.kickTimeUs(k)); }));
ok(kickErr <= bd + 1, 'onsets within one block of the true kick', kickErr);
const a0 = L.alignStats(S, 0, fromUs, endUs), a70 = L.alignStats(S, 70, fromUs, endUs), a50 = L.alignStats(S, 50, fromUs, endUs);
near(a0.medianMs, 70, bd / 1000 + 0.5, 'signal delay 0: audio lags MIDI by ~70 ms');
near(a70.medianMs, 0, bd / 1000 + 0.5, 'signal delay 70: markers sit on the onsets');
near(a50.medianMs, 20, bd / 1000 + 0.5, 'signal delay 50: 20 ms still to go');
ok(a0.n >= 15, 'alignment uses most note-ons', a0.n);
const run30 = runMock({ leadMs: 30 }, 6);
near(L.alignStats(run30.S, 0, run30.S.latestTs - 4e6, run30.S.latestTs).medianMs, 30, bd / 1000 + 0.5, 'configurable lead (30 ms)');
// note-on raw ts really leads the kick by exactly leadMs (ts is un-delayed hub time)
const firstOn = run.msgs.find(m => m.t === 'midi' && m.addr === '/tussen/midi/note/36' && m.v > 0);
ok(Math.abs(run.hub.kickTimeUs(0) - firstOn.ts - 70e3) < 1, 'note-on ts = kick - 70 ms exactly');

// ---- 6a. the feed is held for sync point 1 -----------------------------------------
section('feed held for the signal delay');
{
  // Like the hub: midi/osc frames leave at ts + signal delay (when td gets
  // its datagram), ts still the receipt time. Run with 70 ms and 0 ms.
  const go = (dMs) => {
    const hub = L.createMockHub(), S = L.createState(), got = [];
    let worstAhead = -Infinity;
    hub.tick(0); hub.onMessage(L.msg.watch(['*'])); hub.onMessage(L.msg.setSync({ signal_delay_ms: dMs }));
    for (let t = 15e3; t <= 6e6; t += 15e3) {
      for (const m of hub.tick(t)) {
        L.applyMessage(S, m, t / 1000);
        if (m.t === 'midi' || m.t === 'osc') got.push({ at: t, m: m });
      }
      worstAhead = Math.max(worstAhead, S.latestTs - t);
    }
    return { hub, S, got, worstAhead };
  };
  const d70 = go(70), d0 = go(0);
  ok(d70.got.length > 100 && d70.got.every(g => g.at >= g.m.ts + 70e3 && g.at < g.m.ts + 70e3 + 15e3), 'every frame arrives at ts + 70 ms (within one tick)');
  ok(d0.got.every(g => g.at < g.m.ts + 15e3), 'with no delay, at once');
  const key = (m) => m.t + m.addr + '@' + m.ts;
  const late = new Set(d70.got.map(g => key(g.m))), cut = 6e6 - 70e3;
  ok(d0.got.filter(g => g.m.ts <= cut).every(g => late.has(key(g.m))), 'holding loses nothing and changes no ts');
  ok(d70.got.every((g, i, a) => i === 0 || a[i - 1].m.ts <= g.m.ts), 'released in ts order');
  const on = d70.got.find(g => g.m.t === 'midi' && g.m.addr === '/tussen/midi/note/36' && g.m.v > 0).m;
  ok(Math.abs(d70.hub.kickTimeUs(0) - on.ts - 70e3) < 1, 'note-on ts still = kick - 70 ms (un-delayed)');
  ok(L.releasedTs(d70.S, 1e6) === 1.07e6 && L.releasedTs(L.createState(), 1e6) === 1e6, 'releasedTs = ts + signal delay');
  { // no audio (no capture device): the signal frames alone carry the clock
    const Sn = L.createState(); L.applyMessage(Sn, { t: 'hello', sync: { signal_delay_ms: 70, td_round_trip_ms: 0 }, dests: [], remaps: [] }, 0);
    L.applyMessage(Sn, { t: 'midi', ts: 1e6, addr: '/tussen/midi/cc/1', v: 0.5 }, 0);
    ok(Sn.latestTs === 1.07e6 && Sn.midi[0].ts === 1e6, 'a frame received at ts says the hub is at ts + delay', Sn.latestTs);
    L.applyMessage(Sn, { t: 'osc', ts: 2e6, addr: '/ext/x', v: 1, src: 'x' }, 0);
    ok(Sn.latestTs === 2.07e6 && Sn.history.buf[1].ts === 2e6, 'osc too; the history keeps ts', Sn.latestTs);
  }
  // The page's idea of hub time (clock offset) comes from latestTs: the
  // delayed frames must neither drag it back nor push it past the hub.
  ok(d70.worstAhead <= 1, 'latestTs never runs ahead of the hub', d70.worstAhead);
  const behind = d70.S.latestTs - 6e6;
  ok(behind > -40e3, 'latestTs keeps up with the hub', behind);
  { // lowering the delay mid-run: newer frames overtake older ones (the hub's
    // heap lets them out on time), and the page keeps its arrays ts-ascending
    const hub = L.createMockHub(), S = L.createState();
    hub.tick(0); hub.onMessage(L.msg.watch(['/ext/tablet/fader1'])); S.watch = ['/ext/tablet/fader1'];
    hub.onMessage(L.msg.setSync({ signal_delay_ms: 300 }));
    let order = 0, prev = -Infinity;
    for (let t = 15e3; t <= 3e6; t += 15e3) {
      if (t === 1995e3) hub.onMessage(L.msg.setSync({ signal_delay_ms: 0 }));
      for (const m of hub.tick(t)) { if (m.t === 'midi') { if (m.ts < prev) order++; prev = Math.max(prev, m.ts); } L.applyMessage(S, m, t / 1000); }
    }
    const asc = (a) => a.every((x, i) => i === 0 || a[i - 1] <= x);
    ok(order > 0, 'lowering the delay reorders the feed', order);
    ok(asc(S.midi.map(e => e.ts)), 'midi stays ts-ascending');
    const ser = S.osc.get('/ext/tablet/fader1');
    ok(ser && asc(ser.ts) && ser.ts.length === ser.v.length, 'a lane series stays ts-ascending');
    const arr = []; for (const t of [1, 3, 2, 5, 4, 4]) L.pushByTs(arr, { ts: t });
    ok(arr.map(e => e.ts).join() === '1,2,3,4,4,5', 'pushByTs');
  }
  // Alignment over what has arrived: still the markers on the kicks at 70.
  const e = d70.S.latestTs, a = L.alignStats(d70.S, 70, e - 4e6, e);
  near(a.medianMs, 0, bd / 1000 + 0.5, 'delayed feed: signal delay 70 still sits on the onsets');
  ok(a.n >= 6, 'alignment sees the delayed note-ons', a.n);
  // History rows keep the receipt ts, newest last.
  const h = d70.S.history.buf;
  ok(h.length > 0 && h.every((x, i) => i === 0 || h[i - 1].ts <= x.ts), 'history: receipt ts, in order');
}

// ---- 6b. Link -------------------------------------------------------------------------
section('Link');
{
  const lk = run.msgs.filter(m => m.t === 'link');
  ok(Math.abs(lk.length / 25 - 4) < 0.3, 'mock sends link at ~4 Hz', lk.length);
  ok(lk.every(m => m.quantum === 4 && m.playing === true && m.peers === 2 && m.bpm === 120), 'mock link: 120 bpm, quantum 4, playing, 2 peers in session');
  // every link message's beat line puts beat k on the k-th note-on (kick - leadMs)
  let worst = 0;
  for (const m of lk) for (const k of [0, 10, 40]) worst = Math.max(worst, Math.abs(m.beat + (run.hub.kickTimeUs(k) - 70e3 - m.ts) * m.bpm / 60e6 - k));
  ok(worst < 1e-6, 'link beat k = k-th note-on', worst);
  ok(run.S.links.length > 0 && run.S.links.length <= L.LINK_KEEP && run.S.links.every((l, i, a) => i === 0 || a[i - 1].ts < l.ts), 'link history kept, bounded, ts ascending', run.S.links.length);
  ok(L.linkLive(run.S, run.S.linkAtMs + 10) && !L.linkLive(L.createState(), 0), 'linkLive');
  // with the signal delay at leadMs the grid lands on the kicks; bars on every 4th
  const ends = run.S.latestTs, g = L.beatGrid(run.S.links, ends - 4e6, ends, 70e3);
  ok(g.length === 8, '4 s @ 120 bpm = 8 beats', g.length);
  ok(g.every(e => Math.abs(e.ts - run.hub.kickTimeUs(e.beat)) < 1e-3), 'shifted grid sits on the kicks');
  ok(g.every(e => e.bar === (e.beat % 4 === 0)) && g.filter(e => e.bar).length === 2, 'bars every quantum');
}
{ // applyMessage + beatGrid on synthetic data
  const S5 = L.createState();
  ok(L.applyMessage(S5, { t: 'link', ts: 1e6, beat: 0, bpm: 120, quantum: 4, playing: true, peers: 2 }, 100) === 'link' && S5.links.length === 1 && S5.links[0].peers === 2, "applyMessage('link')");
  ok(L.linkLive(S5, 1000) && !L.linkLive(S5, 100 + L.LINK_STALE_MS), 'link goes stale after ~2 s');
  const g1 = L.beatGrid(S5.links, 1e6, 3e6, 0);
  ok(g1.map(e => e.beat).join() === '0,1,2,3,4' && g1.every((e, i) => Math.abs(e.ts - (1e6 + i * 500e3)) < 1e-6), 'beats every 500 ms, window ends inclusive', g1);
  ok(g1.map(e => e.bar).join() === 'true,false,false,false,true', 'bar = multiple of quantum');
  const g0 = L.beatGrid(S5.links, 0, 1e6, 0);
  ok(g0.map(e => e.beat).join() === '-2,-1,0', 'before the first message: its line extends back', g0.map(e => e.beat));
  const gs = L.beatGrid(S5.links, 1e6, 3e6, 100e3);
  ok(gs.map(e => e.beat).join() === '0,1,2,3' && Math.abs(gs[0].ts - 1.1e6) < 1e-6 && Math.abs(gs[3].ts - 2.6e6) < 1e-6, 'shift moves the grid later by shiftUs', gs);
  // tempo change at 2 s (beat 2) to 60 bpm: beats then 1 s apart
  L.applyMessage(S5, { t: 'link', ts: 2e6, beat: 2, bpm: 60, quantum: 4, playing: false, peers: 2 }, 200);
  const gt = L.beatGrid(S5.links, 1e6, 5e6, 0);
  ok(gt.map(e => e.beat).join() === '0,1,2,3,4,5' && gt.map(e => e.ts / 1e6).join() === '1,1.5,2,3,4,5', 'tempo change mid-window: each span uses the tempo in force', gt.map(e => [e.beat, e.ts]));
  ok(gt[1].playing === true && gt[3].playing === false, 'grid carries the playing flag in force');
  L.applyMessage(S5, { t: 'link', ts: 2e6, beat: 2, bpm: 90, quantum: 4, playing: true, peers: 2 }, 300);
  ok(S5.links.length === 2 && S5.links[1].bpm === 90, 're-sent instant supersedes');
  for (let i = 0; i < 300; i++) L.applyMessage(S5, { t: 'link', ts: 3e6 + i * 250e3, beat: 0, bpm: 120, quantum: 4, playing: true, peers: 1 }, 400);
  ok(S5.links.length === L.LINK_KEEP, 'link history capped', S5.links.length);
  ok(L.beatGrid([], 0, 1e6, 0).length === 0, 'no Link, no grid');
}
{ // the peer list: carried on `link`, normalised, rendered as display rows
  ok(run.S.linkPeers.length === 3, 'mock: three peers in the latest link', run.S.linkPeers.length);
  const rows = L.peerRows(run.S.linkPeers);
  ok(rows.map(r => r.name).join() === 'dj-laptop.local,this machine,192.0.2.33', 'rows named by hostname, this machine, else address', rows.map(r => r.name));
  ok(rows[0].addr === '192.0.2.21' && rows[1].addr === '192.0.2.10' && rows[2].addr === '', 'the address beside a name; not repeated when it is the name');
  ok(rows.map(r => r.session).join() === 'in session,in session,other session', 'in / out of the session');
  ok(rows[0].tempo === '120.0 bpm ▶' && rows[2].tempo === '98.0 bpm ■', 'tempo and transport', rows.map(r => r.tempo));
  ok(rows[0].seen === 'now' && /^1\.\d s ago$/.test(rows[2].seen), 'last seen', rows.map(r => r.seen));
  ok(L.peerSummary(run.S.linkPeers) === "2 in the hub's session, 1 in another", 'summary line', L.peerSummary(run.S.linkPeers));
  // optional fields absent; junk tolerated; no peer_list = no peers
  const n = L.normPeers([{ node: 'a', session: 's', in_session: true, local: false, bpm: 100, age_ms: 0 }, null, 7]);
  ok(n.length === 1 && n[0].host === null && n[0].hostname === null && n[0].playing === null, 'normPeers: absent fields are null, junk dropped', n);
  ok(L.peerRows(n)[0].name === 'unknown host' && L.peerRows(n)[0].tempo === '100.0 bpm', 'a peer with no address');
  const S8 = L.createState();
  L.applyMessage(S8, { t: 'link', ts: 1, beat: 0, bpm: 120, quantum: 4, playing: true, peers: 0 }, 0);
  ok(Array.isArray(S8.linkPeers) && S8.linkPeers.length === 0, 'a link without peer_list: no peers');
  L.applyMessage(S8, { t: 'link', ts: 2, beat: 0, bpm: 120, quantum: 4, playing: true, peers: 1, peer_list: [{ node: 'x', session: 's', in_session: true, host: '192.0.2.5', local: false, bpm: 120, age_ms: 2500 }] }, 0);
  ok(S8.linkPeers.length === 1 && L.peerRows(S8.linkPeers)[0].seen === '2.5 s ago', 'the latest link replaces the list');
  L.resetBuffers(S8);
  ok(S8.linkPeers.length === 0, 'reset forgets the peers');
  for (const [k, want] of [[0, 0], [1, 1], [2, 2]]) {
    const r = runMock({ peers: k }, 1), last = r.msgs.filter(m => m.t === 'link').pop();
    ok(last.peer_list.length === k && last.peers === want, 'mock peers=' + k, last);
  }
}
{ // modes: waiting → Start session; waiting → a peer joins
  ok(run.msgs.filter(m => m.t === 'link').every(m => m.mode === 'joined'), 'mock with peers: joined');
  ok(JSON.stringify(L.msg.recordStart()) === '{"t":"record_start"}' && JSON.stringify(L.msg.recordStart(' a ')) === '{"t":"record_start","name":"a"}' && JSON.stringify(L.msg.recordStop()) === '{"t":"record_stop"}', 'msg.recordStart / recordStop');
  ok(L.recordLabel({ on: true, take: 'T-x', elapsedMs: 64000, atMs: 100 }, 1600) === '● REC 1:05 · T-x' && L.recordLabel({ on: false, take: 'T-x', elapsedMs: 0, atMs: 0 }, 9) === '' && L.recordLabel(null, 0) === '', 'recordLabel');
  ok(L.normRecord(undefined, 0) === null && L.normRecord({ on: true, take: 't', elapsed_ms: 5 }, 7).atMs === 7, 'normRecord: absent = this hub cannot record');
  { // the Record button through the mock: start (named), a refused second start, stop
    const r = runMock({}, 4, (hub, el) => {
      if (el === 990e3) hub.onMessage(L.msg.recordStart('sound check'));
      if (el === 1500e3) hub.onMessage(L.msg.recordStart());
      if (el === 3000e3) hub.onMessage(L.msg.recordStop());
    });
    const recs = r.msgs.filter(m => m.t === 'hello').map(m => m.record);
    ok(recs.length === 4 && recs.every(x => x && typeof x.on === 'boolean' && Number.isInteger(x.elapsed_ms)), 'every hello carries record', recs);
    ok(!recs[0].on && recs[1].on && recs[1].take.endsWith('-sound-check') && recs[2].on && recs[2].take === recs[1].take, 'record_start: on, named; a second start keeps the take', recs);
    ok(Math.abs(recs[2].elapsed_ms - 510) < 20, 'elapsed counts from the press', recs[2]);
    ok(!recs[3].on && recs[3].take === recs[1].take && r.S.record && !r.S.record.on, 'record_stop: off, the last take still named');
    ok(r.hub.errors.length === 0, 'mock saw no protocol errors (record)', r.hub.errors);
  }
  { // a replaying hub: recording switched off (button shown, disabled), record_start refused, link carries replay
    const r = runMock({ replay: true }, 3, (hub, el) => { if (el === 990e3) hub.onMessage(L.msg.recordStart('nope')); });
    const hellos = r.msgs.filter(m => m.t === 'hello');
    ok(hellos.length === 2 && hellos.every(m => m.record && m.record.on === false && m.record.off === 'recording is off during replay'), 'replaying: hello carries record switched off, before and after record_start', hellos.map(m => m.record));
    ok(r.S.record && r.S.record.off === 'recording is off during replay' && !r.S.record.on, 'replaying: the page keeps the button, switched off');
    ok(L.normRecord({ on: false, elapsed_ms: 0 }, 0).off === '', 'live: not switched off');
    const lk = r.msgs.filter(m => m.t === 'link').pop();
    ok(lk.replay && lk.replay.take && Array.isArray(lk.replay.loop) && r.S.replay && /^REPLAY /.test(L.replayLabel(r.S.replay)), 'replaying: link carries replay', lk.replay);
    ok(r.hub.errors.length === 0, 'mock saw no protocol errors (replay)', r.hub.errors);
  }
  ok(L.replayLabel({ take: 'T-rehearsal', bar: 10.34, bars: 24, loop: [9, 17] }) === 'REPLAY T-rehearsal · bar 10.3/24 · loop 9–17' && L.replayLabel({ take: 't', bar: 1, bars: 3 }) === 'REPLAY t · bar 1.0/3' && L.replayLabel(null) === '', 'replayLabel');
  ok(L.linkLabel({ mode: 'joined', peers: 2 }) === 'Link 2 peers' && L.linkLabel({ mode: 'started', peers: 1 }) === 'Link 1 peer' && L.linkLabel({ mode: 'waiting', peers: 0 }) === 'Link: waiting' && L.linkLabel(null) === 'no Link', 'linkLabel');
  ok(/joins the first one it hears/.test(L.linkModeNote({ mode: 'waiting' })) && /stays until the hub stops/.test(L.linkModeNote({ mode: 'started' })) && /waits again/.test(L.linkModeNote({ mode: 'joined' })), 'linkModeNote says the rule');
  const S9 = L.createState();
  L.applyMessage(S9, { t: 'link', ts: 1, beat: 0, bpm: 120, quantum: 4, playing: true, peers: 1 }, 0);
  ok(S9.links[0].mode === 'joined', 'a link without mode reads as joined (older hubs)');
  ok(JSON.stringify(L.msg.linkStart()) === '{"t":"link_start"}', 'msg.linkStart');
  // waiting: a beat of its own at the configured tempo; the console drives it; Start founds
  const w = runMock({ peers: 0 }, 3, (hub, el) => {
    if (el === 0.9e6) hub.onMessage(L.msg.setTempo(100));
    if (el === 2.1e6) hub.onMessage(L.msg.linkStart());
    if (el === 2.4e6) hub.onMessage(L.msg.linkStart());   // already started: ignored
  });
  const wl = w.msgs.filter(m => m.t === 'link');
  const before = wl.filter(m => m.mode === 'waiting'), after = wl.filter(m => m.mode === 'started');
  ok(before.length >= 7 && before[0].peers === 0 && before[0].peer_list.length === 0 && before[0].bpm === 120, 'waiting: link frames flow with a beat of its own, no peers', before[0]);
  ok(before.some(m => m.bpm === 100), 'waiting: set_tempo moves the own beat');
  ok(after.length >= 3 && after.every(m => m.bpm === 100 && m.peers === 0), 'link_start: started, at the waiting tempo', after[0]);
  const lastWait = before[before.length - 1], firstStart = after[0];
  ok(Math.abs(lastWait.beat + (firstStart.ts - lastWait.ts) * 100 / 60e6 - firstStart.beat) < 1e-6, 'starting keeps the beat continuous');
  ok(w.hub.errors.length === 0, 'mock saw no protocol errors (link_start)', w.hub.errors);
  // waiting → peers appear at 126: joined, at their tempo
  const j = runMock({ peers: 0, joinMs: 1500 }, 3);
  const jl = j.msgs.filter(m => m.t === 'link'), firstJoined = jl.findIndex(m => m.mode === 'joined');
  ok(firstJoined > 0 && jl.slice(0, firstJoined).every(m => m.mode === 'waiting' && m.bpm === 120), 'waiting at 120 until a peer appears');
  ok(jl.slice(firstJoined).every(m => m.mode === 'joined' && m.bpm === 126 && m.peers === 2 && m.peer_list.length === 3), 'then joined at the peers\' 126', jl[firstJoined]);
  const jr = runMock({ peers: 0, joinMs: 500 }, 2, (hub, el) => { if (el === 1.5e6) hub.onMessage(L.msg.linkStart()); });
  ok(jr.msgs.filter(m => m.t === 'link').pop().mode === 'joined', 'Start session once joined: ignored');
}
{ // set_tempo / set_playing through the mock: reported bpm/playing follow, beat line stays continuous
  const r = runMock({}, 3, (hub, el) => { if (el === 1.5e6) { hub.onMessage(L.msg.setTempo(90)); hub.onMessage(L.msg.setPlaying(false)); } });
  const lk = r.msgs.filter(m => m.t === 'link'), last = lk[lk.length - 1];
  ok(last.bpm === 90 && last.playing === false, 'mock reports the new tempo / transport');
  const before = lk.filter(m => m.bpm === 120).pop(), after = lk.find(m => m.bpm === 90);
  ok(Math.abs(before.beat + (after.ts - before.ts) * 120 / 60e6 - after.beat) < 1e-6, 'beat line continuous across set_tempo');
  ok(r.hub.errors.length === 0, 'mock saw no protocol errors (Link)', r.hub.errors);
}

// ---- 6b. the anchored strip's beat anchor --------------------------------------
section('anchored strip (beatAnchor)');
{
  const S6 = L.createState();
  ok(L.beatAnchor(S6.links, 5e6, 0) === null, 'no Link, no anchor');
  L.applyMessage(S6, { t: 'link', ts: 1e6, beat: 0, bpm: 120, quantum: 4, playing: true, peers: 1 }, 0);
  const a = L.beatAnchor(S6.links, 2.2e6, 0);
  ok(a && a.beat === 2 && Math.abs(a.ts - 2e6) < 1e-6, 'latest beat line at or before now', a);
  ok(a && Math.abs(a.periodUs - 500e3) < 1e-6, 'carries the beat period');
  ok(a && a.bar === false && L.beatAnchor(S6.links, 3.1e6, 0).bar === true, 'bar flag like the grid (beat 4 = a bar)');
  const on = L.beatAnchor(S6.links, 2e6, 0);
  ok(on && on.beat === 2, 'a beat line exactly at now anchors (window end inclusive)', on);
  ok(L.beatAnchor(S6.links, 2.6e6, 0).beat === 3, 'the anchor advances beat by beat');
  // the anchor is the SHIFTED line, like the drawn grid: +70 ms keeps beat 2
  // in reach (2.07 s <= 2.2 s), +350 ms pushes it past now and beat 1 anchors
  const a70 = L.beatAnchor(S6.links, 2.2e6, 70e3);
  ok(a70 && a70.beat === 2 && Math.abs(a70.ts - 2.07e6) < 1e-6, 'shifted grid: anchor ts carries the shift', a70);
  const a350 = L.beatAnchor(S6.links, 2.2e6, 350e3);
  ok(a350 && a350.beat === 1 && Math.abs(a350.ts - 1.85e6) < 1e-6, 'a shift can hand the anchor to the previous beat', a350);
  const S7 = L.createState();
  L.applyMessage(S7, { t: 'link', ts: 1e6, beat: 0, bpm: 0, quantum: 4, playing: false, peers: 1 }, 0);
  ok(L.beatAnchor(S7.links, 2e6, 0) === null, 'bpm 0: nothing to anchor to');
  // tempo change: the anchor uses the tempo in force (60 bpm after 2 s = beats 1 s apart)
  L.applyMessage(S6, { t: 'link', ts: 2e6, beat: 2, bpm: 60, quantum: 4, playing: true, peers: 1 }, 1);
  const at = L.beatAnchor(S6.links, 4.5e6, 0);
  ok(at && at.beat === 4 && Math.abs(at.ts - 4e6) < 1e-6 && Math.abs(at.periodUs - 1e6) < 1e-6, 'anchor follows a tempo change', at);
}

// ---- 7. delays round-trip through the mock and the page state ------------------------
section('delay round trip');
const S2 = L.createState(), hub2 = L.createMockHub();
const feed = (t) => { for (const m of hub2.tick(t)) L.applyMessage(S2, JSON.parse(JSON.stringify(m)), 0); };
feed(1e6); feed(1.1e6);
ok(S2.dests.find(d => d.kind === 'engine') && S2.helloCount === 1, 'hello built dests');
ok(S2.sync.signal_delay_ms === 0 && S2.sync.td_round_trip_ms === 0, 'hello built sync');
let m = L.msg.setSync({ signal_delay_ms: 70 }); L.applyLocalEdit(S2, m);
ok(S2.unsaved && S2.sync.signal_delay_ms === 70, 'optimistic local edit + unsaved');
hub2.onMessage(m); feed(1.2e6);
ok(S2.sync.signal_delay_ms === 70 && hub2.state.sync.signal_delay_ms === 70 && S2.sync.td_round_trip_ms === 0, 'hub state follows set_sync; re-sent hello agrees');
m = L.msg.setSync({ td_round_trip_ms: 110 }); L.applyLocalEdit(S2, m); hub2.onMessage(m); feed(1.25e6);
ok(S2.sync.td_round_trip_ms === 110 && S2.sync.signal_delay_ms === 70, 'round trip set; signal delay untouched');
const ed = () => S2.dests.find(d => d.name === 'editor');
ok(ed().follow === 'td' && L.signalDelayMs(ed(), S2.sync) === 180 && L.audioDelayMs(ed(), S2.sync) === 110, 'device follows TD by default');
m = L.msg.setFollow('editor', 'music'); L.applyLocalEdit(S2, m);
ok(ed().follow === 'music', 'optimistic follow edit');
hub2.onMessage(m); feed(1.3e6);
ok(ed().follow === 'music' && L.signalDelayMs(ed(), S2.sync) === 70 && L.audioDelayMs(ed(), S2.sync) === 0, 'set_follow round trip; music = what TD gets');
// set_follow on a static dest is ignored by the hub; the next hello wins
const helloN = S2.helloCount;
L.applyLocalEdit(S2, { t: 'set_follow', dest: 'td', follow: 'music' }); hub2.onMessage({ t: 'set_follow', dest: 'td', follow: 'music' }); feed(1.35e6);
ok(S2.helloCount === helloN + 1 && !('follow' in S2.dests.find(d => d.name === 'td')), 'set_follow on a static dest: ignored, hello re-sent');
hub2.onMessage({ t: 'set_follow', dest: 'nobody', follow: 'td' }); feed(1.36e6);
ok(S2.helloCount === helloN + 2, 'set_follow on an unknown dest: ignored, hello re-sent');
{ // the late-arriving quest device carries follow and takes set_follow
  const r = runMock({}, 22, (hub, el) => { if (el === 21e6) hub.onMessage(L.msg.setFollow('quest', 'music')); });
  const ds = r.msgs.filter(x => x.t === 'dests' && x.dests.some(d => d.name === 'quest'));
  ok(ds.length && ds[0].dests.find(d => d.name === 'quest').follow === 'td', 'quest registers with follow td');
  ok(r.S.dests.find(d => d.name === 'quest').follow === 'music', 'quest follow -> music round trip');
  ok(r.hub.errors.length === 0, 'mock saw no protocol errors (follow)', r.hub.errors);
}
const up = L.msg.remapUpsert({ dest: 'td', from: '/ext/a', to: '/td/a', keep: false }); L.applyLocalEdit(S2, up);
ok(S2.remaps.some(r => L.isTempId(r.id)), 'new rule shows as pending');
hub2.onMessage(up); feed(1.4e6);
const real = S2.remaps.find(r => r.from === '/ext/a');
ok(real && !L.isTempId(real.id) && S2.remaps.length === 2, 'hub id replaces the temp one');
hub2.onMessage(L.msg.remapDelete(real.id)); feed(1.5e6);
ok(!S2.remaps.some(r => r.from === '/ext/a'), 'remap_delete');
hub2.onMessage(L.msg.save()); feed(1.6e6);
ok(!S2.unsaved && S2.saved.ok, 'saved ok clears unsaved');
L.applyMessage(S2, { t: 'saved', ok: false, error: 'permission denied' }, 0);
ok(S2.saved.ok === false && S2.saved.error === 'permission denied', 'saved error kept for display');
ok(hub2.errors.length === 0, 'mock saw no protocol errors', hub2.errors);
const hub3 = L.createMockHub(); hub3.tick(0);
hub3.onMessage({ t: 'set_delay', dest: 'td', midi_delay_ms: 5 }); hub3.onMessage({ t: 'save', extra: 1 });
hub3.onMessage({ t: 'set_sync' }); hub3.onMessage({ t: 'set_sync', signal_delay_ms: 2.5 }); hub3.onMessage({ t: 'set_follow', dest: 'editor', follow: 'late' });
ok(hub3.errors.length === 5, 'mock is strict (old set_delay, unknown field, empty/fractional set_sync, bad follow)', hub3.errors);
ok(hub3.state.sync.signal_delay_ms === 0 && hub3.state.dests.find(d => d.name === 'editor').follow === 'td', 'rejected edits change nothing');

// ---- destination status (the status column, the top-bar pill, the ↻ button) ----
section('destination status');
{
  // A hub frame as protocol.rs/reach.rs make it (field names owned HERE).
  const frame = { t: 'dest_status', list: [
    { name: 'engine', target: '127.0.0.1:3928', addr: '127.0.0.1:3928', lookup: 'ok', state: 'ok', detail: 'sending; no error reported', osc_rate: 60.04 },
    { name: 'td', target: 'td.local:9001', addr: '192.0.2.4:9001', lookup: 'ok', state: 'error', detail: 'audio: nobody listening on port 5006', osc_rate: 30, rtp: { port: 5006, rate: 187.5, error: 'nobody listening on port 5006' } },
    { name: 'djay', target: 'andras-macbook-pro.local:9010', lookup: 'pending', state: 'pending', detail: 'looking up andras-macbook-pro.local…', osc_rate: 0 },
    { name: 'flux', target: 'x:1', addr: '192.0.2.9:1', lookup: 'ok', state: 'idle', detail: 'nothing sent yet (no subscribed traffic)', osc_rate: 0 },
  ] };
  const S = L.createState();
  ok(L.applyMessage(S, frame, 1000) === 'dest_status' && S.destStatus.size === 4, 'dest_status is applied');
  const cell = (name, kind, at) => L.destStatusCell(S, { name: name, kind: kind || 'td' }, at === undefined ? 1500 : at);
  const e = cell('engine', 'engine');
  ok(e.cls === 'ok' && e.text === '● OSC 60/s' && /sends to: 127\.0\.0\.1:3928/.test(e.title), 'ok: green with its rate', e);
  ok(/not received/.test(e.title), 'the tooltip says green is not a delivery receipt');
  const t = cell('td');
  ok(t.cls === 'bad' && t.text === '✕ audio: nobody listening on port 5006' && /audio \(RTP\) port 5006: nobody listening/.test(t.title), 'error: red, says why', t);
  ok(cell('djay').cls === 'pending' && cell('djay').text === '… looking up andras-macbook-pro.local…', 'pending: looking up');
  ok(/nothing \(not resolved\)/.test(cell('djay').title), 'unresolved: sends to nothing');
  ok(cell('flux').cls === 'idle' && cell('flux').text === '○ idle', 'idle');
  ok(cell('speakers', 'engine').text === 'local output', 'a static dest the hub does not report: a local audio output');
  ok(cell('quest', 'device').text === '● announced', 'a device: announced');
  ok(cell('engine', 'engine', 1000 + L.DEST_STATUS_STALE_MS + 1).text === '…', 'stale status shows as unknown, not green');
  const sum = L.destSummary(S, 1500);
  ok(sum.text === 'dests 1/4 sending · 1 ✕ · 1 …' && sum.cls === 'bad' && sum.bad.length === 1 && /^td: /.test(sum.bad[0]), 'summary pill', sum);
  ok(L.destSummary(S, 1000 + L.DEST_STATUS_STALE_MS) === null && L.destSummary(L.createState(), 0) === null, 'no pill when stale or empty');
  ok(L.normDestStatus({ name: 'x', state: 'weird' }).state === 'error', 'an unknown state is shown as an error, never as ok');
  // The mock hub: td's RTP refuses until ↻, then it is green.
  let kicked = false;
  const r = runMock({}, 3, (hub, el) => { if (el >= 2e6 && !kicked) { kicked = true; hub.onMessage(L.msg.resolve()); } });
  const st = r.msgs.filter(m => m.t === 'dest_status');
  ok(st.length >= 3, 'mock sends dest_status every second', st.length);
  ok(st[0].list.find(d => d.name === 'td').state === 'error' && st[st.length - 1].list.find(d => d.name === 'td').state === 'ok', 'mock: resolve turns td green');
  ok(r.hub.errors.length === 0, 'mock saw no protocol errors (resolve)', r.hub.errors);
}

console.log((fail ? 'FAILED' : 'ok') + ': ' + pass + ' passed, ' + fail + ' failed');
process.exit(fail ? 1 : 0);

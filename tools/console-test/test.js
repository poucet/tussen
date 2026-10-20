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
  addrs: ['list'],
  saved: ['ok'],
  link: ['ts', 'beat', 'bpm', 'quantum', 'playing', 'mode', 'peers', 'peer_list'],
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
ok(lists.every((l, i) => i === 0 || lists[i - 1].every(a => l.includes(a))), 'the list only grows; nothing ever drops out');
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
  ok(Sh.history.buf.length <= L.HISTORY_CAP + 512 && Sh.history.total === L.HISTORY_CAP + 2002, 'ring capped, total keeps counting', Sh.history.buf.length);
  ok(L.historyRows(Sh, '', 5).length === 5 && L.historyRows(Sh, '', 5)[0].v === L.HISTORY_CAP + 1999, 'limit + newest kept');
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

console.log((fail ? 'FAILED' : 'ok') + ': ' + pass + ' passed, ' + fail + ' failed');
process.exit(fail ? 1 : 0);

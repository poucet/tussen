#!/usr/bin/env node
// Drives the real page in headless Chrome (?mock=1) over the DevTools protocol:
// no uncaught errors, the UI builds from the mock hub, the signal delay (sync
// point 1) moves the markers onto the kicks, the round trip (sync point 2) and
// the follow switch round-trip, watch/rule/save round-trip, the Link peer
// dropdown (open, outside click, Escape, empty state), waiting → Start
// session and waiting → a session appears, and the Record button. Node stdlib only (needs
// Node >= 22 for the global WebSocket). Skips (exit 0) when Chrome is absent.
//   node tools/console-test/browser.js            # screenshot -> <repo>/.screenshots/progress/<date>-console-test/
//   SCREENSHOT_DIR=… node browser.js              # a lane labels its own directory
'use strict';
const fs = require('fs'), path = require('path'), os = require('os'), cp = require('child_process');

const ws_root = path.resolve(__dirname, '..', '..');
const repoRoot = ws_root.includes(path.sep + '.workspaces' + path.sep) ? ws_root.split(path.sep + '.workspaces' + path.sep)[0] : ws_root;
const CHROME = process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
if (!fs.existsSync(CHROME)) { console.log('skip: no Chrome at ' + CHROME); process.exit(0); }
const shotDir = process.env.SCREENSHOT_DIR || path.join(repoRoot, '.screenshots', 'progress', new Date().toISOString().slice(0, 10) + '-console-test');
const page = 'file://' + path.join(ws_root, 'hub', 'console', 'index.html') + '?mock=1&watch=/ext/tablet/fader1,/tussen/quest/controller/trigger';

let pass = 0, fail = 0;
function ok(c, name, extra) { if (c) pass++; else { fail++; console.log('FAIL ' + name + (extra !== undefined ? '  ' + JSON.stringify(extra) : '')); } }
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

let ch = null;
(async () => {
  const prof = fs.mkdtempSync(path.join(os.tmpdir(), 'console-chrome-'));
  ch = cp.spawn(CHROME, ['--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check', '--user-data-dir=' + prof, '--remote-debugging-port=0', '--window-size=1600,1200', 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
  const killer = setTimeout(() => { console.log('FAIL timeout'); ch.kill('SIGKILL'); process.exit(1); }, 90e3);
  const wsUrl = await new Promise((res, rej) => { let buf = ''; ch.stderr.on('data', d => { buf += d; const m = buf.match(/DevTools listening on (ws:\/\/\S+)/); if (m) res(m[1]); }); ch.on('exit', () => rej(new Error('chrome exited: ' + buf))); });
  const port = new URL(wsUrl).port;
  const targets = await (await fetch('http://127.0.0.1:' + port + '/json/list')).json();
  const tgt = targets.find(t => t.type === 'page');
  const ws = new WebSocket(tgt.webSocketDebuggerUrl);
  await new Promise(r => ws.addEventListener('open', r, { once: true }));
  let id = 0; const waiting = new Map(), errors = [];
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); }
    if (m.method === 'Runtime.exceptionThrown') errors.push(m.params.exceptionDetails.exception ? m.params.exceptionDetails.exception.description : m.params.exceptionDetails.text);
    if (m.method === 'Runtime.consoleAPICalled' && (m.params.type === 'error' || m.params.type === 'warning')) errors.push(m.params.args.map(a => a.value !== undefined ? JSON.stringify(a.value) : a.description).join(' '));
  });
  const cdp = (method, params) => new Promise(r => { const i = ++id; waiting.set(i, r); ws.send(JSON.stringify({ id: i, method, params: params || {} })); });
  const ev = async (expr) => { const r = await cdp('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true }); if (r.result.exceptionDetails) throw new Error(expr + ': ' + JSON.stringify(r.result.exceptionDetails)); return r.result.result.value; };

  await cdp('Runtime.enable'); await cdp('Page.enable');
  await cdp('Emulation.setDeviceMetricsOverride', { width: 1600, height: 1200, deviceScaleFactor: 1, mobile: false });
  await cdp('Page.navigate', { url: page });
  await sleep(3500);

  ok(/MOCK/.test(await ev("document.querySelector('#conn').textContent")), 'status pill says MOCK');
  const row = (name) => "document.querySelector('#dests tbody tr[data-dest=\"" + name + "\"]')";
  const cells = async (name) => ev("[...(" + row(name) + ").children].slice(3).map(c => c.textContent).join('/')");
  ok(await ev("document.querySelectorAll('#dests tbody tr').length") >= 4, 'destination rows built from hello');
  ok(/never delayed/.test(await ev(row('engine') + ".textContent")) && /never delayed/.test(await ev(row('monitor') + ".textContent")), 'engine and monitor rows explain why');
  ok(await ev("[...document.querySelectorAll('#dests tbody tr')].every(r => !!r.querySelector('select.follow') === /device/.test(r.children[1].textContent))"), 'a follow switch on device rows only');
  ok(await ev(row('editor') + ".querySelector('select.follow').value") === 'td', 'editor follows TD by default');
  ok(await ev("document.querySelectorAll('#shiftdest, #trimctl').length") === 0, 'no per-dest shift selector or device trim');
  ok(await ev("document.querySelectorAll('#addrs tbody tr').length") >= 5, 'address list populated');
  { // the remap list holds a stable address order (hot rows excepted) and shows min/max
    const addrCols = await ev("[...document.querySelectorAll('#addrs tbody tr')].filter(r => !r.classList.contains('hot')).map(r => r.dataset.a)");
    ok(addrCols.join() === addrCols.slice().sort().join(), 'remap list sorted by address (non-hot rows)', addrCols);
    ok(await ev("document.querySelectorAll('#addrs thead th').length") === 6, 'remap list has min/max columns');
    ok(await ev("[...document.querySelectorAll('#addrs tbody tr')].every(r => { const m = parseFloat(r.children[3].textContent), x = parseFloat(r.children[4].textContent); return !isNaN(m) && !isNaN(x) && m <= x; })"), 'every remap row shows min ≤ max');
  }
  ok(await ev("document.querySelector('#tl').clientHeight") === 16 + 200 + 34 + 2 * 40, 'canvas has two OSC lanes for ?watch=');
  ok(/Link 2 peers\b/.test(await ev("document.querySelector('#linkst').textContent")), 'Link group shows peers');
  // ---- the peer dropdown: click the count; outside click and Escape close it --
  const dropOpen = () => ev("document.querySelector('#peerdrop').classList.contains('open') && document.querySelector('#peerdrop').offsetParent !== null");
  const peerRowsText = () => ev("[...document.querySelectorAll('#peerdrop tbody tr')].map(r => [...r.children].map(c => c.textContent).join('|'))");
  ok(!(await dropOpen()), 'peer list closed at first');
  await ev("document.querySelector('#linkst').click()");
  await sleep(400);
  ok(await dropOpen() && await ev("document.querySelector('#linkst').getAttribute('aria-expanded')") === 'true', 'clicking the count opens the peer list');
  const pr = await peerRowsText();
  ok(pr.length === 3, 'three peers listed', pr);
  ok(/^dj-laptop\.local\|192\.0\.2\.21\|in session\|120\.0 bpm ▶\|/.test(pr[0] || ''), 'a remote peer: hostname, address, in session, tempo', pr[0]);
  ok(/^this machine\|192\.0\.2\.10\|in session/.test(pr[1] || ''), 'a peer on this machine', pr[1]);
  ok(/^192\.0\.2\.33\|\|other session\|98\.0 bpm ■/.test(pr[2] || ''), 'a peer in another session', pr[2]);
  ok(/2 in the hub's session, 1 in another/.test(await ev("document.querySelector('#peerdrop').textContent")), 'summary line');
  { // anchored under the count, inside the viewport
    const r = await ev("(() => { const a = document.querySelector('#linkst').getBoundingClientRect(), d = document.querySelector('#peerdrop').getBoundingClientRect(); return { top: d.top - a.bottom, left: d.left - a.left, right: d.right }; })()");
    ok(r.top >= 0 && r.top < 12 && Math.abs(r.left) < 2 && r.right <= 1600, 'dropdown anchored under the count', r);
  }
  await sleep(600);
  ok((await peerRowsText()).length === 3, 'the open list keeps updating');
  fs.mkdirSync(shotDir, { recursive: true });
  const shotP = await cdp('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1600, height: 300, scale: 1 } });
  fs.writeFileSync(path.join(shotDir, 'console-link-peers-open.png'), Buffer.from(shotP.result.data, 'base64'));
  await ev("document.querySelector('#peerdrop').dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))");
  ok(await dropOpen(), 'a click inside the list keeps it open');
  await ev("document.querySelector('#tl').dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))");
  ok(!(await dropOpen()), 'an outside click closes it');
  await ev("document.querySelector('#linkst').click()");
  ok(await dropOpen(), 'reopens');
  await ev("document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))");
  ok(!(await dropOpen()) && await ev("document.querySelector('#linkst').getAttribute('aria-expanded')") === 'false', 'Escape closes it');
  await ev("document.querySelector('#linkst').click(); document.querySelector('#linkst').click()");
  ok(!(await dropOpen()), 'clicking the count again closes it');
  ok(await ev("document.querySelector('#bpm').value") === '120' && /stop/.test(await ev("document.querySelector('#play').textContent")), 'tempo 120 and ■ stop while playing');
  const align0 = await ev("document.querySelector('#align').textContent");
  const med = (t) => { const m = t.match(/median of \d+: ([+-][\d.]+) ms/); return m ? Number(m[1]) : null; };
  ok(med(align0) !== null && Math.abs(med(align0) - 70) <= 6, 'delay 0: page measures ~+70 ms lag', align0);

  // sync point 1: signal delay to 70 with the buttons, 7 × "+10"
  const clickN = (ctl, label, n) => ev("(() => { const b = [...document.querySelectorAll('#" + ctl + " button')].find(b => b.textContent === '" + label + "'); for (let i = 0; i < " + n + "; i++) b.click(); })()");
  await clickN('sigctl', '+10', 7);
  await sleep(1500);
  ok(await ev("document.querySelector('#sigctl input[type=number]').value") === '70', 'signal delay reads 70');
  ok(/unsaved/.test(await ev("document.querySelector('#unsaved').textContent")), 'unsaved indicator on');
  const align70 = await ev("document.querySelector('#align').textContent");
  ok(med(align70) !== null && Math.abs(med(align70)) <= 6, 'signal delay 70: markers sit on the kicks', align70);
  // keyboard: → +1, ⇧← −10, → ×9 back to 70
  await ev("(() => { const k = (key, sh) => window.dispatchEvent(new KeyboardEvent('keydown', { key: key, shiftKey: !!sh, bubbles: true })); k('ArrowRight'); })()");
  await sleep(100);
  ok(await ev("document.querySelector('#sigctl input[type=number]').value") === '71', 'ArrowRight: signal delay +1');
  await ev("(() => { const k = (key, sh) => window.dispatchEvent(new KeyboardEvent('keydown', { key: key, shiftKey: !!sh, bubbles: true })); k('ArrowLeft', true); for (let i = 0; i < 9; i++) k('ArrowRight'); })()");
  await sleep(1000);
  ok(await ev("document.querySelector('#sigctl input[type=number]').value") === '70', 'shift+ArrowLeft −10, then back to 70; hello agrees');

  // ---- anchored strip: the purple line holds still; at 70 the kick sits on it --
  ok(await ev("document.querySelector('#aw').clientWidth") > 300, 'anchored strip visible on the sync page');
  ok(await ev("getComputedStyle(document.querySelector('#aw')).cursor") === 'grab', 'strip shows the grab cursor');
  const awProbe = "(() => { const c = document.querySelector('#aw'); const g = c.getContext('2d'); const W = c.width, H = c.height; const d = g.getImageData(0, 0, W, H).data; const colGreen = (x) => { let n = 0; for (let y = 0; y < H; y++) { const i = (y * W + x) * 4; if (d[i + 1] > 120 && d[i + 1] > d[i] + 30 && d[i + 1] > d[i + 2] + 20) n++; } return n; }; const colPurple = (x) => { let n = 0; for (let y = 0; y < H; y++) { const i = (y * W + x) * 4; if (d[i] > 140 && d[i + 2] > 200 && d[i + 2] > d[i + 1] + 60) n++; } return n; }; let best = -1, bx = -1; for (let x = 2; x < W - 2; x++) { const n = colGreen(x); if (n > best) { best = n; bx = x; } } return { W: W, H: H, peakX: bx, peakN: best, centrePurple: colPurple(Math.round(W / 2)) + colPurple(Math.round(W / 2) - 1) }; })()";
  // kicks repeat every beat (244 px at 120 bpm in the 1600 ms window), so the
  // brightest column is SOME kick — what must hold is its phase against the line
  const phaseErr = (p) => { const beatPx = p.W * 500 / 1600, m = ((p.peakX - p.W / 2) % beatPx + beatPx) % beatPx; return Math.min(m, beatPx - m); };
  const p1 = await ev(awProbe);
  ok(p1.centrePurple > p1.H, 'purple line at the centre', p1);
  ok(p1.peakN > 10 && phaseErr(p1) <= 10, 'delay 70: the kicks sit on the line', p1);
  await sleep(700);
  const p2 = await ev(awProbe);
  ok(p2.centrePurple > p2.H, 'the line is still at the centre while content flows', p2);
  // drag the waveform right 30 ms worth of px → delay ≈ 40 (MouseEvent clientX
  // is an integer, so allow ±2 ms), back left → ≈ 70 again
  const drag = (sign) => ev("(() => { const c = document.querySelector('#aw'); const r = c.getBoundingClientRect(); const px = " + sign + " * 30 / 1600 * r.width; const y = r.top + r.height / 2, x0 = r.left + r.width / 2; c.dispatchEvent(new MouseEvent('mousedown', { clientX: x0, clientY: y, bubbles: true })); window.dispatchEvent(new MouseEvent('mousemove', { clientX: x0 + px, clientY: y, bubbles: true })); window.dispatchEvent(new MouseEvent('mouseup', { clientX: x0 + px, clientY: y, bubbles: true })); })()");
  await drag(1);
  await sleep(1200);
  const dv = Number(await ev("document.querySelector('#sigctl input[type=number]').value"));
  ok(Math.abs(dv - 40) <= 2, 'dragging the waveform right 30 ms lowers the signal delay to ~40; slider follows', dv);
  await drag(-1);
  await sleep(1200);
  const dv2 = Number(await ev("document.querySelector('#sigctl input[type=number]').value"));
  ok(Math.abs(dv2 - 70) <= 2, 'dragging back raises it to ~70; the hub hello agrees', dv2);
  if (dv2 !== 70) await ev("(() => { const n = document.querySelector('#sigctl input[type=number]'); n.value = '70'; n.dispatchEvent(new Event('input')); })()");
  await sleep(800);
  const p3 = await ev(awProbe);
  ok(p3.peakN > 10 && phaseErr(p3) <= 10, 'after the drag round trip the kicks are back on the line', p3);

  // sync point 2: round trip 30
  await clickN('rtctl', '+10', 3);
  await sleep(1000);
  ok(await ev("document.querySelector('#rtctl input[type=number]').value") === '30', 'TD round trip reads 30 (after the hub echoed it)');
  ok(await cells('engine') === '0/0' && await cells('monitor') === '0/0', 'engine, monitor: 0/0', [await cells('engine'), await cells('monitor')]);
  ok(await cells('td') === '70/0', 'td: signal delay / audio 0', await cells('td'));
  ok(await cells('editor') === '100/30', 'device following TD: signal+round trip / round trip', await cells('editor'));
  // follow switch: set_follow; the hub's next hello must agree (else the select reverts after 600 ms)
  await ev(row('editor') + ".querySelector('select.follow').value = 'music'; " + row('editor') + ".querySelector('select.follow').dispatchEvent(new Event('change'))");
  ok(await cells('editor') === '70/0', 'follow music: optimistic effective delay = TD', await cells('editor'));
  await sleep(1200);
  ok(await ev(row('editor') + ".querySelector('select.follow').value") === 'music' && await cells('editor') === '70/0', 'follow music survives the next hello');
  await ev(row('editor') + ".querySelector('select.follow').value = 'td'; " + row('editor') + ".querySelector('select.follow').dispatchEvent(new Event('change'))");
  await sleep(1200);
  ok(await ev(row('editor') + ".querySelector('select.follow').value") === 'td' && await cells('editor') === '100/30', 'follow back to td round trip');

  // watch toggle + rule form + rule round trip
  await ev("[...document.querySelectorAll('#addrs tbody tr')].find(r => r.dataset.a === '/tussen/midi/cc/1').click()");
  await sleep(300);
  ok(await ev("document.querySelector('#rfrom').value") === '/tussen/midi/cc/1', 'click fills the rule form');
  ok(await ev("document.querySelector('#tl').clientHeight") === 16 + 200 + 34 + 3 * 40, 'click adds an OSC lane');
  await ev("document.querySelector('#rdest').value = 'td'; document.querySelector('#rto').value = '/td/cc1'; document.querySelector('#rlo').value = '0'; document.querySelector('#rhi').value = '2'; document.querySelector('#rsubmit').click()");
  await sleep(300);
  ok(await ev("[...document.querySelectorAll('#rules tbody tr')].some(r => /\\/td\\/cc1/.test(r.textContent) && /delete/.test(r.textContent))"), 'new rule listed with a hub id (edit/delete)');
  await ev("document.querySelector('#save').click()");
  await sleep(300);
  ok(/saved → \/mock/.test(await ev("document.querySelector('#savedmsg').textContent")), 'save shows the path');
  ok(!/unsaved/.test(await ev("document.querySelector('#unsaved').textContent")), 'unsaved cleared after save');

  // zoom to 100 ms and pause for the screenshot (select a marker for the readout)
  await ev("document.querySelector('#win').value = 400; document.querySelector('#win').dispatchEvent(new Event('change'))");
  await sleep(1200);
  await ev("document.querySelector('#pause').click()");
  fs.mkdirSync(shotDir, { recursive: true });
  const shot = await cdp('Page.captureScreenshot', { format: 'png' });
  fs.writeFileSync(path.join(shotDir, 'console-mock-sync70-rt30-400ms.png'), Buffer.from(shot.result.data, 'base64'));
  // 4 s window: the Link beat grid (bars on the accented kicks at signal delay 70)
  await ev("document.querySelector('#pause').click(); document.querySelector('#win').value = 4000; document.querySelector('#win').dispatchEvent(new Event('change'))");
  await sleep(1500);
  await ev("document.querySelector('#pause').click()");
  const shot2 = await cdp('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1600, height: 440, scale: 1 } });
  fs.writeFileSync(path.join(shotDir, 'console-mock-link-grid-4s.png'), Buffer.from(shot2.result.data, 'base64'));
  // transport + tempo round trip (after the shots: a tempo change moves the grid's phase off the kicks)
  await ev("document.querySelector('#pause').click()");
  await ev("document.querySelector('#play').click()");
  await sleep(400);
  ok(/start/.test(await ev("document.querySelector('#play').textContent")), 'stop -> set_playing false round trip');
  await ev("document.querySelector('#play').click(); { const n = document.querySelector('#bpm'); n.value = '2000'; n.dispatchEvent(new Event('change')); }");
  await sleep(1000);
  ok(await ev("document.querySelector('#bpm').value") === '999' && /stop/.test(await ev("document.querySelector('#play').textContent")), 'tempo clamped to 999 and echoed by the mock; playing again');
  // ---- OSC endpoint + the osc tab -------------------------------------------
  ok(await ev("document.querySelector('#eptext').textContent") === '192.0.2.10:9000', 'endpoint shows the mock hub ip:port');
  ok(/mock-hub\.local:9000/.test(await ev("document.querySelector('#endpoint').title")), 'tooltip lists the hostname form');
  await ev("document.querySelector('#epcopy').click()");
  await sleep(300);
  ok(/copied|⌘C/.test(await ev("document.querySelector('#epcopy').textContent")), 'copy button gives feedback');
  const copied = await ev("navigator.clipboard && navigator.clipboard.readText ? navigator.clipboard.readText().catch(() => null) : null");
  if (copied !== null) ok(copied === '192.0.2.10:9000', 'clipboard holds ip:port', copied);

  await ev("document.querySelector('#tab-osc').click()");
  await sleep(1500);
  ok(await ev("document.body.classList.contains('tab-osc')") && await ev("document.querySelector('#main').offsetParent === null"), 'osc tab hides the sync page');
  ok(await ev("document.querySelectorAll('#hist tbody tr').length") >= 20, 'live history fills under watch "*"');
  ok(await ev("[...document.querySelectorAll('#hist tbody tr')].some(r => /\\/tussen\\/editor\\/head\\/n\\/yaw/.test(r.textContent))"), 'an unwatched address appears in the history');
  ok(await ev("[...document.querySelectorAll('#hist tbody tr')].some(r => /midi/.test(r.children[3].textContent))"), 'MIDI signals appear, labelled midi');
  const seenAddrs = await ev("[...document.querySelectorAll('#seen tbody tr')].map(r => r.children[0].textContent)");
  ok(seenAddrs.length >= 6 && seenAddrs.join() === seenAddrs.slice().sort().join(), 'address set populated and sorted by address', seenAddrs);
  ok(await ev("document.querySelectorAll('#seen thead th').length") === 8, 'address set has min and max columns');
  ok(await ev("[...document.querySelectorAll('#seen tbody tr')].every(r => { const m = parseFloat(r.children[4].textContent), v = parseFloat(r.children[5].textContent), x = parseFloat(r.children[6].textContent); return m <= v && v <= x; })"), 'min ≤ last value ≤ max on every set row');
  ok(await ev("[...document.querySelectorAll('#seen tbody tr')].some(r => parseFloat(r.children[4].textContent) < parseFloat(r.children[6].textContent))"), 'a moving control shows min < max');
  ok(await ev("[...document.querySelectorAll('#seen tbody tr')].some(r => r.children[0].textContent === '/tussen/midi/cc/1' && r.children[1].textContent === 'out')"), 'hub-made addresses read out');
  ok(await ev("[...document.querySelectorAll('#seen tbody tr')].some(r => r.children[0].textContent === '/ext/tablet/fader1' && r.children[1].textContent === 'in')"), 'inbound addresses read in');
  // filter + pause
  await ev("{ const f = document.querySelector('#histfilter'); f.value = 'fader1'; f.dispatchEvent(new Event('input')); }");
  ok(await ev("[...document.querySelectorAll('#hist tbody tr')].every(r => /fader1/.test(r.children[1].textContent))"), 'history filter narrows to the substring');
  await ev("{ const f = document.querySelector('#histfilter'); f.value = ''; f.dispatchEvent(new Event('input')); }");
  await ev("document.querySelector('#histpause').click()");
  const frozen = await ev("document.querySelector('#hist tbody tr').textContent");
  await sleep(800);
  ok(await ev("document.querySelector('#hist tbody tr').textContent") === frozen, 'pause freezes the history view');
  ok(/live/.test(await ev("document.querySelector('#histpause').textContent")), 'pause button flips to live');
  const shot3 = await cdp('Page.captureScreenshot', { format: 'png' });
  fs.writeFileSync(path.join(shotDir, 'console-osc-tab-history-addresses.png'), Buffer.from(shot3.result.data, 'base64'));
  await ev("document.querySelector('#histpause').click(); document.querySelector('#tab-sync').click()");
  await sleep(400);
  ok(await ev("!document.body.classList.contains('tab-osc') && document.querySelector('#oscpage').offsetParent === null"), 'sync tab comes back');

  // ---- the Record button: start, the running clock, stop ------------------------
  ok(!(await ev("document.querySelector('#rec').hidden")) && /Record/.test(await ev("document.querySelector('#rec').textContent")), 'Record button in the top bar');
  ok(await ev("document.querySelector('#recinfo').textContent") === '', 'not recording: no clock');
  await ev("document.querySelector('#rec').click()");
  await sleep(1400);
  ok(/Stop/.test(await ev("document.querySelector('#rec').textContent")) && await ev("document.querySelector('#rec').classList.contains('on')"), 'recording: the button turns into Stop');
  const recText = await ev("document.querySelector('#recinfo').textContent");
  ok(/^● REC 0:0[1-2] · 2026-10-20T21-14-03$/.test(recText), 'recording: elapsed time and take name', recText);
  const shotR = await cdp('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1600, height: 120, scale: 1 } });
  fs.writeFileSync(path.join(shotDir, 'console-recording.png'), Buffer.from(shotR.result.data, 'base64'));
  await ev("document.querySelector('#rec').click()");
  await sleep(400);
  ok(/Record/.test(await ev("document.querySelector('#rec').textContent")) && await ev("document.querySelector('#recinfo').textContent") === '', 'Stop: back to Record, the clock goes');
  ok(await ev("document.querySelector('#replayst').hidden"), 'live: no REPLAY label');
  // A replaying hub: no Record button at all, the REPLAY label instead.
  await cdp('Page.navigate', { url: page + '&replay=1' });
  await sleep(1500);
  ok(await ev("document.querySelector('#recgrp').hidden && document.querySelector('#recgrp').offsetParent === null"), 'replaying: no Record button');
  ok(/^REPLAY 2026-10-20T21-14-03-rehearsal · bar \d+\.\d\/24 · loop 9–17$/.test(await ev("document.querySelector('#replayst').textContent")) && !(await ev("document.querySelector('#replayst').hidden")), 'replaying: the REPLAY label', await ev("document.querySelector('#replayst').textContent"));
  const shotRp = await cdp('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1600, height: 120, scale: 1 } });
  fs.writeFileSync(path.join(shotDir, 'console-replaying.png'), Buffer.from(shotRp.result.data, 'base64'));
  await cdp('Page.navigate', { url: page });
  await sleep(1500);

  // ---- waiting for a Link session → Start session ----------------------------
  ok(await ev("document.querySelector('#linkstart').hidden"), 'in a session: no Start session button');
  await cdp('Page.navigate', { url: page + '&peers=0' });
  await sleep(1500);
  ok(/^Link: waiting\b/.test(await ev("document.querySelector('#linkst').textContent")), 'no peers: the field says waiting');
  ok(!(await ev("document.querySelector('#linkstart').hidden")), 'waiting: Start session offered');
  ok(await ev("document.querySelector('#bpm').value") === '120' && !(await ev("document.querySelector('#bpm').disabled")), 'waiting: own beat at 120, tempo editable');
  await ev("document.querySelector('#linkst').click()");
  await sleep(300);
  ok(await ev("document.querySelector('#peerdrop .empty') && document.querySelector('#peerdrop .empty').textContent") === 'no Link peers', 'empty state: no Link peers');
  ok(/joins the first one it hears/.test(await ev("document.querySelector('#peerdrop .mode').textContent")), 'the dropdown says what waiting means');
  const shotW = await cdp('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1600, height: 300, scale: 1 } });
  fs.writeFileSync(path.join(shotDir, 'console-link-waiting.png'), Buffer.from(shotW.result.data, 'base64'));
  await ev("{ const n = document.querySelector('#bpm'); n.value = '104'; n.dispatchEvent(new Event('change')); }");
  await sleep(700);
  await ev("document.querySelector('#linkstart').click()");
  await sleep(700);
  ok(/^Link 0 peers\b/.test(await ev("document.querySelector('#linkst').textContent")), 'started: the field shows the peer count', await ev("document.querySelector('#linkst').textContent"));
  ok(await ev("document.querySelector('#linkstart').hidden"), 'started: the button goes');
  ok(await ev("document.querySelector('#bpm').value") === '104', 'started at the tempo set while waiting');
  ok(/stays until the hub stops/.test(await ev("document.querySelector('#peerdrop .mode').textContent")), 'the open dropdown follows: started');
  const shotS = await cdp('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1600, height: 300, scale: 1 } });
  fs.writeFileSync(path.join(shotDir, 'console-link-started.png'), Buffer.from(shotS.result.data, 'base64'));
  // ---- waiting → a session appears: joined, at its tempo -----------------------
  await cdp('Page.navigate', { url: page + '&peers=0&join=2500' });
  await sleep(1200);
  ok(/^Link: waiting\b/.test(await ev("document.querySelector('#linkst').textContent")), 'waiting before the peers come');
  await sleep(2300);
  ok(/^Link 2 peers\b/.test(await ev("document.querySelector('#linkst').textContent")), 'a session appears: joined, its peers counted');
  ok(await ev("document.querySelector('#bpm').value") === '126' && await ev("document.querySelector('#linkstart').hidden"), 'at the session\'s 126; no Start session');

  ok(errors.length === 0, 'no uncaught errors / console errors', errors);

  console.log('screenshots: ' + path.join(shotDir, 'console-mock-sync70-rt30-400ms.png') + ', console-mock-link-grid-4s.png, console-osc-tab-history-addresses.png, console-link-peers-open.png, console-link-waiting.png, console-link-started.png, console-recording.png, console-replaying.png');
  console.log((fail ? 'FAILED' : 'ok') + ': ' + pass + ' passed, ' + fail + ' failed');
  clearTimeout(killer); ws.close(); ch.kill('SIGKILL');
  process.exit(fail ? 1 : 0);
})().catch(e => { console.log('FAIL ' + e.stack); if (ch) ch.kill('SIGKILL'); process.exit(1); });

#!/usr/bin/env node
// Live smoke for the signals websocket (docs/guides/signals-websocket.md):
// connects to a RUNNING hub as two simultaneous read-only subscribers with
// watch "*" — the shape an external shader page uses next to the open
// console — and checks both get the same feed; neither starves the other.
// Read-only by construction (?readonly=1): safe against a live performance.
// Skips (exit 0) when no hub is listening. Node >= 22 (global WebSocket).
//   node tools/console-test/smoke.js            # hub console on 127.0.0.1:9098
//   CONSOLE=127.0.0.1:9099 node smoke.js        # another [console] port
'use strict';

const CONSOLE = process.env.CONSOLE || '127.0.0.1:9098';
const WINDOW_MS = 3000;

let pass = 0, fail = 0;
function ok(c, name, extra) { if (c) pass++; else { fail++; console.log('FAIL ' + name + (extra !== undefined ? '  ' + JSON.stringify(extra) : '')); } }

function client(label) {
  const c = { label, hello: null, events: 0, osc: 0, open: false };
  c.ready = new Promise((resolve, reject) => {
    const ws = new WebSocket('ws://' + CONSOLE + '/ws?readonly=1');
    c.ws = ws;
    ws.addEventListener('open', () => { c.open = true; ws.send(JSON.stringify({ t: 'watch', addrs: ['*'] })); });
    ws.addEventListener('message', (ev) => {
      const m = JSON.parse(ev.data);
      if (m.t === 'hello' && !c.hello) { c.hello = m; resolve(); }
      if (m.t === 'osc') c.osc++;
      c.events++;
    });
    ws.addEventListener('error', () => reject(new Error(label + ': connect failed')));
    ws.addEventListener('close', () => { if (!c.hello) reject(new Error(label + ': closed before hello')); });
  });
  return c;
}

(async () => {
  // Reachable at all? One probe socket decides skip-vs-run, so a machine
  // without a hub exits 0 like browser.js does without Chrome.
  const reachable = await new Promise((resolve) => {
    const ws = new WebSocket('ws://' + CONSOLE + '/ws?readonly=1');
    const t = setTimeout(() => { ws.close(); resolve(false); }, 2000);
    ws.addEventListener('open', () => { clearTimeout(t); ws.close(); resolve(true); });
    ws.addEventListener('error', () => { clearTimeout(t); resolve(false); });
  });
  if (!reachable) { console.log('skip: no hub console at ' + CONSOLE + ' (start one: tussen-hub run)'); process.exit(0); }

  const a = client('a'), b = client('b');
  await Promise.all([a.ready, b.ready]);
  for (const c of [a, b]) {
    ok(c.hello && c.hello.osc && typeof c.hello.osc.port === 'number', c.label + ': hello carries the OSC endpoint', c.hello && c.hello.osc);
    ok(Array.isArray(c.hello && c.hello.dests), c.label + ': hello carries dests');
  }
  console.log('-- OSC endpoint: ' + JSON.stringify(a.hello.osc) + '; listening ' + WINDOW_MS + ' ms…');
  await new Promise((r) => setTimeout(r, WINDOW_MS));

  // Both subscribers see the same firehose: counts within 20% of each
  // other (same hub, same window; exact equality races the window edges).
  console.log('-- a: ' + a.events + ' frames (' + a.osc + ' osc) · b: ' + b.events + ' frames (' + b.osc + ' osc)');
  const lo = Math.min(a.osc, b.osc), hi = Math.max(a.osc, b.osc);
  ok(hi === 0 || lo >= hi * 0.8, 'neither subscriber starves (osc counts within 20%)', { a: a.osc, b: b.osc });
  if (hi === 0) console.log('note: no OSC flowed in the window — wiggle something to smoke the feed itself');

  a.ws.close(); b.ws.close();
  console.log((fail ? 'FAILED: ' : 'ok: ') + pass + ' passed, ' + fail + ' failed');
  process.exit(fail ? 1 : 0);
})().catch((e) => { console.log('FAIL ' + e.message); process.exit(1); });

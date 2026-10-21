// A fake browser for running the console's <script id="ui"> in node: just
// enough DOM, timers, clock and WebSocket to drive the page from hub frames
// and count what it writes to the DOM. No Chrome (Chris: a lane never
// launches a browser, headless or not).
//   const b = fakeBrowser(); vm.runInNewContext(logic + ui, b.globals);
//   b.ws.recv({...}); b.advance(500); b.writes('#hist tbody')
'use strict';

const WRITE_PROPS = new Set(['innerHTML', 'textContent', 'className', 'title', 'hidden', 'disabled', 'value']);

function fakeBrowser() {
  let now = 0, nextTimer = 1;
  const timers = new Map(), rafs = [], writes = new Map(), sockets = [], observers = [];
  const count = (el) => {
    let e = el; while (e && !e.label) e = e.parent;   // a created element counts for the table it sits in
    const k = e ? e.label : '(detached)'; writes.set(k, (writes.get(k) || 0) + 1);
  };
  const noop = () => undefined;
  function makeEl(tag, label) {
    const base = {
      tag, label, parent: null, children: [], dataset: {}, style: {}, attrs: {},
      value: '', checked: false, clientWidth: 1600, clientHeight: 300,
      classList: { s: new Set(), toggle(c, on) { (on === undefined ? !this.s.has(c) : on) ? this.s.add(c) : this.s.delete(c); }, add(c) { this.s.add(c); }, remove(c) { this.s.delete(c); }, contains(c) { return this.s.has(c); } },
      setAttribute(k, v) { this.attrs[k] = String(v); }, getAttribute(k) { return this.attrs[k]; },
      addEventListener: noop, removeEventListener: noop, focus: noop, scrollIntoView: noop, select: noop, closest: () => null,
      getBoundingClientRect: () => ({ top: 0, left: 0, width: 1600, height: 300 }),
      getContext: () => new Proxy({ measureText: () => ({ width: 10 }) }, { get: (t, p) => (p in t ? t[p] : noop), set: () => true }),
      appendChild(c) { return proxy.insertBefore(c, null); },
      insertBefore(c, ref) {
        if (c.parent) c.parent.children.splice(c.parent.children.indexOf(c), 1);
        const i = ref ? base.children.indexOf(ref) : -1;
        if (i < 0) base.children.push(c); else base.children.splice(i, 0, c);
        c.parent = proxy; count(proxy); return c;
      },
      removeChild(c) { base.children.splice(base.children.indexOf(c), 1); c.parent = null; count(proxy); return c; },
      remove() { if (base.parent) base.parent.removeChild(proxy); },
    };
    const proxy = new Proxy(base, {
      get: (t, p) => (p in t ? t[p] : typeof p === 'symbol' ? undefined : (p.startsWith('on') || p.startsWith('_') ? undefined : noop)),
      set: (t, p, v) => { if (WRITE_PROPS.has(p)) count(proxy); t[p] = v; return true; },
    });
    return proxy;
  }
  const bySel = new Map();
  const document = {
    hidden: false, activeElement: null,
    body: makeEl('body', 'body'),
    querySelector: (sel) => { if (!bySel.has(sel)) bySel.set(sel, makeEl('el', sel)); return bySel.get(sel); },
    createElement: (tag) => makeEl(tag, null),
    addEventListener: (type, f) => { if (type === 'visibilitychange') document._vis = f; },
    execCommand: () => false,
  };
  const globals = {
    document, console, Math, Number, JSON, Map, Set, Array, Object, String, Boolean, Date, Promise, Error, RegExp, Symbol, Proxy,
    Float32Array, Float64Array, isFinite, isNaN, parseFloat, parseInt, Infinity, NaN, URLSearchParams, URL,
    performance: { now: () => now },
    location: { host: '127.0.0.1:9098', protocol: 'http:', search: '' },
    localStorage: { getItem: () => null, setItem: noop },
    navigator: {},
    isSecureContext: false, devicePixelRatio: 1,
    fetch: () => Promise.reject(new Error('no network in the fake browser')),
    requestAnimationFrame: (f) => { rafs.push(f); return rafs.length; },
    setInterval: (f, ms) => { const id = nextTimer++; timers.set(id, { f, ms: ms || 1, at: now + (ms || 1), every: true }); return id; },
    setTimeout: (f, ms) => { const id = nextTimer++; timers.set(id, { f, ms: ms || 0, at: now + (ms || 0), every: false }); return id; },
    clearInterval: (id) => timers.delete(id), clearTimeout: (id) => timers.delete(id),
    WebSocket: function (url) { this.url = url; this.sent = []; this.send = (t) => this.sent.push(JSON.parse(t)); sockets.push(this); },
    IntersectionObserver: function (cb) { this.cb = cb; this.els = []; this.observe = (el) => this.els.push(el); observers.push(this); },
  };
  globals.window = globals;
  globals.addEventListener = noop;
  return {
    globals, document,
    get ws() { return sockets[sockets.length - 1]; },
    // Open the page's socket and hand it hub frames.
    open() { this.ws.onopen(); },
    recv(m) { this.ws.onmessage({ data: JSON.stringify(m) }); },
    // Run the clock forward, firing timers (and one animation frame per 16 ms).
    advance(ms) {
      const end = now + ms;
      while (true) {
        let due = null;
        for (const [id, t] of timers) if (t.at <= end && (!due || t.at < due[1].at)) due = [id, t];
        if (!due) break;
        now = due[1].at;
        if (due[1].every) due[1].at += due[1].ms; else timers.delete(due[0]);
        due[1].f();
      }
      now = end;
      const fs = rafs.splice(0); for (const f of fs) f(now);
    },
    // A table scrolled in or out of the viewport.
    intersect(id, on) { for (const o of observers) for (const el of o.els) if (el.label === '#' + id) o.cb([{ target: { id }, isIntersecting: on }]); },
    setHidden(h) { document.hidden = h; if (document._vis) document._vis(); },
    click(sel) { document.querySelector(sel).onclick(); },
    writes(label) { return writes.get(label) || 0; },
    resetWrites() { writes.clear(); },
    rows(sel) { return document.querySelector(sel).children; },
  };
}

module.exports = { fakeBrowser };

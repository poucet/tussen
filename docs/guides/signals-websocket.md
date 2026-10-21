# The signals websocket

How an external page or script reads the hub's live signals — OSC, MIDI, the
beat — over the console's websocket. A browser shader sketchpad, a VJ tool or
a monitoring script consumes this to drive itself from the same signals every
other subscriber sees. The field names below are pinned by
`hub/src/console/protocol.rs` (the source of truth).

## Connect

```
ws://127.0.0.1:<console port>/ws        # port: hub.toml [console], default 9098
```

The console binds 127.0.0.1 only, so the consumer runs on the hub laptop.
Who may connect, by the browser's `Origin`:

| Origin | Access |
|---|---|
| the console page itself · none (scripts, `file://`) | full: commands + feed |
| any other loopback origin (`http://127.0.0.1:8787`, `http://localhost:5173`, …) | **read-only**: `watch` + every event |
| anything non-loopback | refused (403) |

`?readonly=1` on the URL opts any connection down to read-only — use it from
scripts so a feed consumer can never disturb a live performance. Read-only
connections may send `watch` and nothing else; commands are ignored (hub log
says so). Multiple simultaneous clients are fine; each gets the full feed and
slow consumers are dropped frames rather than allowed to stall the hub.

## First frame: `hello`

Sent on connect, and re-sent to everyone after any sync/follow/remap change
and when recording starts or stops.

```json
{ "t": "hello",
  "sync":  { "signal_delay_ms": 0, "td_round_trip_ms": 0 },
  "dests": [ { "name": "engine", "kind": "engine" },
             { "name": "quest", "kind": "device", "follow": "td" } ],
  "remaps": [ { "id": 3, "dest": "td", "from": "/a/*", "to": "/b", "range": [0, 2], "keep": true } ],
  "sample_rate": 48000,
  "osc":   { "port": 9000, "ips": ["192.0.2.2"], "hostname": "hub.local" },
  "record": { "on": true, "take": "2026-10-20T21-14-03-rehearsal", "path": "…", "elapsed_ms": 4210 } }
```

`record` is the Record button's state: `on`, the take being written (or the
last one, after a stop), how long it has been recording when the frame was
made, and `error` if the last start failed. `off` is set when recording is
switched off on this hub, saying why (`simply-tussen replay`: "recording is
off during replay"): the console shows the button disabled with that as its
tooltip, and `record_start` is refused. A full-access client starts and
stops takes with `{"t":"record_start"}` (optional `"name"`) and
`{"t":"record_stop"}`; every change re-sends `hello`.

`osc` is where OSC *senders* should aim (the console's copy button);
`hostname` is absent when unknown, `ips` empty on a routeless machine.
`td_round_trip_ms` is the second sync delay (see the README's sync section);
a `dest`'s `follow` says which of the two delays that device gets.

## Subscribe: `watch`

The only message a read-only client sends. OSC events arrive **only for
watched addresses**; each `watch` replaces the previous list.

```json
{ "t": "watch", "addrs": ["*"] }                          // everything
{ "t": "watch", "addrs": ["/tussen/pad/", "/djay/"] }     // exact addresses — no prefix match, see below
```

`"*"` watches every address. Anything else matches the **full address
exactly**, so for a feed consumer `["*"]` is the right call; filter
client-side.

## The feed

One JSON object per frame; `ts` is the hub clock in **microseconds** (one
epoch for all frames — deltas are meaningful, the absolute value is not).

| frame | shape | when |
|---|---|---|
| `osc` | `{"t":"osc","ts":…,"addr":"/ext/x","v":0.5,"src":"127.0.0.1:5000"}` | per message, as routed — watched addresses only; hub-made signals (`/tussen/clock/…`, `/tussen/pad/…`) carry `"src":"hub"`, signals played from a take (`simply-tussen replay`) `"src":"replay"` |
| `midi` | `{"t":"midi","ts":…,"addr":"/tussen/midi/cc/74","v":0.5}` | per MIDI-derived signal, always (no watch needed) |
| `addrs` | `{"t":"addrs","full":false,"list":[{"addr":"/ext/x","last_ts":…,"rate":2.5,"v":0.5,"min":0,"max":1,"count":12,"dir":"in"}]}` | on connect `"full":true`: every address since hub start (permanent, nothing is evicted); then ~2 Hz `"full":false`, only the entries that changed since the last frame (a quiet address is sent again only while its `rate` decays to 0, and a frame with nothing in it is not sent) — merge them by `addr`. Each list is sorted by address; `dir` ∈ `in`/`out`/`both` |
| `link` | `{"t":"link","ts":…,"beat":2.5,"bpm":120,"quantum":4,"playing":true,"mode":"joined","peers":1,"peer_list":[…]}` | ~4 Hz and on Link status change; `mode` is `waiting` (in no session; the beat is the hub's own), `joined` or `started` (from the console); `peers` counts the others in the hub's session, `peer_list` is every peer heard (below); while replaying a take, also `"replay":{"take":"…","bar":9.5,"bars":24,"loop":[9,17]}` (`loop` only with `--loop`) |
| `audio` | `{"t":"audio","ts":…,"blocks":[[min,max,rms],…]}` | ~30 Hz; per-RTP-packet level summaries — the console's waveform, **not** audio data |
| `dests` | `{"t":"dests","dests":[…]}` | when a device registers or leaves |
| `saved` | `{"t":"saved","ok":true,"path":"…"}` | reply to the console's save |
| `dest_status` | `{"t":"dest_status","list":[{"name":"td","target":"td-laptop.local:9001","addr":"192.0.2.21:9001","lookup":"ok","state":"error","detail":"audio: nobody listening on port 5006","osc_rate":60,"rtp":{"port":5006,"rate":187.5,"error":"nobody listening on port 5006"}}]}` | ~1 Hz, and at once after `resolve`; one entry per static destination with a network address (below) |

**Link peers.** Each `peer_list` entry is one Link peer:
`{"node":"Qx7#kP2a","session":"…","in_session":true,"host":"192.0.2.21","hostname":"dj-laptop.local","local":false,"bpm":120,"playing":true,"age_ms":40}`
— `node` is its Link node id, `host` the address it is heard from (absent if
none), `hostname` that address's reverse-DNS name once known (absent until
then, and for `local` peers, which run on the hub's machine), `playing` its
start/stop state (absent if it never set one), `age_ms` how long ago it was
last heard. Link does not say which app a peer is.

**Destination status.** `dest_status` says, per static destination in
`hub.toml` (not devices, not local `audio_out`s), whether the hub is reaching
it. `target` is `host:port` as configured, `addr` what it resolved to (absent
until it does), `lookup` ∈ `ok`/`pending`/`failed`/`no_host`, `osc_rate` OSC
datagrams/s, `rtp` the audio stream for a destination with `rtp_port`.
`state` is `ok` (sent, no error reported), `idle` (nothing to send yet),
`pending` (looking the name up), `warn` (the name stopped resolving; the last
address is still used) or `error`; `detail` says why in one line. OSC and RTP
are UDP: the only errors there are the ones the network sends back ("nobody
listening on port N" is the receiver's ICMP), so `ok` is never proof of
receipt. A full-access client sends `{"t":"resolve"}` (the console's ↻) to
look every name up now and forget earlier send errors; the hub also
re-resolves every 5 s by itself.

**Rates.** `watch "*"` is per-message: with devices publishing at 60 Hz plus
MIDI, expect hundreds of frames a second — batch your reads per animation
frame rather than doing work per message.

**The beat** arrives two ways: the `link` frame above, and the hub's own
clock signals as ordinary OSC — `/tussen/clock/bpm`, `/beat`, `/phase`,
`/running`, `/onbeat` (docs/guides/osc-signals.md) — covered by `watch "*"`.

**Timing: the signals wait for sync point 1.** `osc` and `midi` frames leave
the hub when the visuals destination (`td`) gets the same signal over UDP:
held back by `signal_delay_ms`, from the same delay line, so a page lines
up with the music the way TouchDesigner does. A page gets sync point 1 only,
never a device's `td_round_trip_ms`: it renders on the hub's machine, next
to the audio, like TD, and draws its own picture rather than watching TD's.
`ts` is still when the hub *received* the signal, so a frame arrives
`signal_delay_ms` after its `ts` (recording and the console's alignment
rely on that). Signals played from a take wait the same. What does not wait:
`audio` (raw, like TD's audio), `link` (the console draws its beat grid at
`ts + signal_delay_ms`), and `addrs` (`last_ts` is receipt time). Just after
the delay is lowered, newer frames can overtake older ones for up to the
old delay: keep anything you sort by `ts` sorted.

**Audio is not on the websocket.** The `audio` frames are 3-number level
summaries for drawing a waveform. A browser page that wants the actual music
captures it itself: `getUserMedia` on the same loopback/capture device the hub
reads (`[audio] device`) — the standard Web Audio path, and the same audio
every device hears.

## Minimal consumer

```js
const ws = new WebSocket("ws://127.0.0.1:9098/ws?readonly=1");
const sig = {};                              // address → latest value
ws.onopen = () => ws.send(JSON.stringify({ t: "watch", addrs: ["*"] }));
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  if (m.t === "osc" || m.t === "midi") sig[m.addr] = m.v;   // read per frame, not per message
  if (m.t === "link") sig["/tussen/clock/bpm"] = m.bpm;
};
```

Smoke it live (two simultaneous subscribers against a running hub; skips
when none runs): `node tools/console-test/smoke.js`.

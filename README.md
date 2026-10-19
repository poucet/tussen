# tussen-hub

> tussen-hub is a [simply.audio lab](http://simply.audio) project.

An OSC/MIDI/audio hub for live performance. One Rust process that:

- **routes OSC by subscription** — everything sends to one port (:9000);
  destinations declare prefixes they want (`/tussen/`, `/djay/`, `/ext/…`) and
  get exactly those, with per-destination excludes and address remaps;
- **turns MIDI into OSC** — every controller on the machine becomes
  `/tussen/midi/cc/<n>`, `note/<n>`, `bend`, … (0..1 floats), on change plus a
  slow refresh so late joiners heal;
- **turns a game controller into OSC** — a pad plugged into the hub machine
  becomes `/tussen/pad/…` (sticks, triggers, buttons, press events);
- **joins Ableton Link** — waits for a session and joins it at its tempo
  (it never imposes its own), keeping a beat of its own at `[link] tempo`
  meanwhile; the console can start a session instead; publishes the beat as
  `/tussen/clock/…`;
- **captures and streams the music** — a CoreAudio device or output tap,
  streamed as RTP L16 to whoever asked for it;
- **aligns it all**: two sync delays hold OSC/MIDI back so control signals,
  the beat and the returning music arrive together — set live, by ear or by
  eye, on the built-in console.

Built to drive visual rigs (shaders, TouchDesigner, browser pages) from the
same signals the musicians play; bring any OSC consumer.

## Quick start

```sh
cd hub && cargo build --release
./target/release/tussen-hub config init    # copy hub.toml to ~/.config/tussen/hub.toml
./target/release/tussen-hub run            # reads your config, else the repo's hub.toml
```

Or, from the repo root, `./tussen` does both, then prints the console
URL and the OSC endpoint and opens the console.

Your config lives in `~/.config/tussen/hub.toml` (`$XDG_CONFIG_HOME/tussen/`
if set, macOS too), so you can edit it without touching the checkout. Until
it exists the hub runs the repo's `hub.toml`, the shipped example, and the
console's **Save** creates your file from it rather than writing into the
repo. `tussen-hub config path` says which file the hub would use.

No hardware needed to try it: `tussen-hub run --fake-midi --fake-pad` (or
`./tussen --fake-midi --fake-pad`) invents a controller and a gamepad. The hub prints one stats line a second:

```
devices=1 in=420/s out=1130/s audio=187.5pkt/s audio_ovf=0 console_drop=0 malformed=0 send_errors=0
```

Then:

- open the **console**: <http://127.0.0.1:9098> — every signal live, the sync
  sliders, and the OSC endpoint with a copy button;
- point any OSC tool at **:9000** under its own prefix — e.g. djay Pro's OSC
  output sends `/djay/…`, and it fans out to every subscriber unchanged;
- `tussen-hub list` prints the audio devices and MIDI ports it can see;
  `tussen-hub listen --name me` registers a fake device and prints what it
  receives.

macOS is the tested platform, and the audio capture (CoreAudio, macOS 14.2+
for output taps) is macOS-only; routing, MIDI, Link and the console are
portable Rust, untested elsewhere. First audio run: macOS asks to let your
terminal record system audio — allow it.

A whole room — music laptops on Link, a mixer into the hub, a visuals
laptop, performance devices — is [docs/guides/setup.md](docs/guides/setup.md).

## How signals flow

```
MIDI / gamepad / Link ──► tussen-hub :9000 ◄── OSC: devices, djay Pro, /ext/ tools
                               │
                               ├── OSC (never delayed) ──────────► engine (music software)
                               ├── OSC/MIDI (signal delay) ──────► td (visuals)
                               ├── RTP audio :5006 ──────────────► td (visuals)
                               ├── OSC/MIDI + RTP (both delays) ─► registered devices
                               └── http + ws :9098 ──────────────► the console, your scripts
```

Devices register themselves (an OSC `/tussen/hub/hello`), get the signal fan-
out and the audio stream, and are forgotten when they stop saying hello.
Static destinations live in `hub.toml`.

**The two delays.** The music takes time to come back through a mixer, and a
visuals chain takes time to render. `signal_delay_ms` holds MIDI/OSC back so
they land with the returning music; `td_round_trip_ms` holds a device's whole
bundle (audio included) back so it lands with the visuals. Set both on the
console — the anchored waveform widget holds a beat line still at its centre,
and you drag the waveform until the kicks sit on the line.

## hub.toml

The shipped file is a working single-machine example; every section is
commented. The short version:

| Section | What |
|---|---|
| `[osc]` | the listen port (:9000), packet cap, device timeout |
| `[[dest]]` | a static destination: `name`, `host`, `port`, `subscribe`/`exclude` prefixes, optional `rtp_port` for the audio stream |
| `[audio]` | the capture: `""` = default output (tapped), or a device name from `tussen-hub list` |
| `[midi]` | MIDI inputs (`"*"` = all) and the refresh cadence |
| `[pad]` | game controllers on this machine (`enabled`, name filters, refresh) |
| `[link]` | the hub's own tempo (while it waits, and for a session started from the console) and quantum |
| `[sync]` | the two delays, plus per-device `follow` overrides — the console writes these |
| `[[remap]]` | per-destination address rewrites with optional range mapping — the console writes these |
| `[console]` | the console port (loopback only) |

Which file: `--config <path>` if given, else your user config
(`~/.config/tussen/hub.toml`, or under `$XDG_CONFIG_HOME`), else the nearest
`hub.toml` up from the current directory — the repo's shipped example. The
hub logs its pick at startup (`config: <path> (user)`). `--audio-device "<name>"` overrides
`[audio] device` for one run.

## The console

`http://127.0.0.1:9098`. Left: destinations, devices, Link peers and tempo.
Middle: the signal timeline — audio waveform, MIDI markers, the beat grid —
plus the anchored alignment strip for setting the signal delay by eye or by
drag. Right: every address ever seen (direction, min/max, rate), remap rules,
and the osc-traffic tab: a live scrolling feed of every message. **Save**
writes the sync and remap state back into the config in use, comments
intact — into your user config if the hub was running the repo's example (it
copies the example there first, then uses that file).

## Reading the signals from your own code

The console's websocket doubles as a read-only live feed for external local
pages and scripts — a browser shader, a VJ tool, a logger:

```js
const ws = new WebSocket("ws://127.0.0.1:9098/ws?readonly=1");
ws.onopen = () => ws.send(JSON.stringify({ t: "watch", addrs: ["*"] }));
ws.onmessage = (ev) => { /* {"t":"osc","addr":"/tussen/pad/lx","v":0.5,…} */ };
```

Frame shapes, origin rules and rates: [docs/guides/signals-websocket.md](docs/guides/signals-websocket.md).
Every address the network uses: [docs/guides/osc-signals.md](docs/guides/osc-signals.md).

## Tests

```sh
cd hub && cargo test                 # router, sync, link, audio, console protocol
node tools/console-test/test.js      # the console page, headless
node tools/console-test/browser.js   # the same page in real Chrome (skips without one)
node tools/console-test/smoke.js     # live two-subscriber smoke (skips without a hub)
```

## License

MIT — see [LICENSE](LICENSE).

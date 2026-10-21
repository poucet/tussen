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
  eye, on the built-in console;
- **records and replays** — the console's Record button keeps every
  signal, the audio and the Link timeline as a take;
  `simply-tussen replay latest --loop 9..17` plays it back as if live,
  leading Link, so the visuals can be built without the band.

Built to drive visual rigs (shaders, TouchDesigner, browser pages) from the
same signals the musicians play; bring any OSC consumer.

## Quick start

```sh
./simply-tussen config init    # copy hub.toml to ~/.config/tussen/hub.toml
./simply-tussen run            # build, start the hub, open the console
```

`./simply-tussen` builds `simply-tussen` from the checkout and passes every command
and flag on to it (`./simply-tussen --help`); `run` also opens the console in your
browser. To use the hub outside the checkout, `./simply-tussen install` puts
`simply-tussen` in `~/.local/bin` (`--prefix DIR` for elsewhere); then
`simply-tussen run` works from any directory, the same minus the browser.

On start the hub prints where to reach it:

```
  tussen hub console → http://127.0.0.1:9098
  hub OSC            → 192.168.1.20:9000 (local: 127.0.0.1:9000)
  config             → /Users/you/.config/tussen/hub.toml (user)
```

After that it stays quiet unless something changes (a device joins or
leaves, a Link session, a save) or goes wrong. `run --stats` adds a
counters line every second.

Your config lives in `~/.config/tussen/hub.toml` (`$XDG_CONFIG_HOME/tussen/`
if set, macOS too), so you can edit it without touching the checkout. Until
it exists the hub runs the shipped example (the repo's `hub.toml`, or the
copy built into `simply-tussen` outside a checkout), and the console's
**Save** creates your file from it rather than writing into the repo.
`simply-tussen config path` says which config the hub would use.

No hardware needed to try it: `./simply-tussen run --fake-midi --fake-pad` invents
a controller and a gamepad.

Then:

- open the **console**: <http://127.0.0.1:9098> — every signal live, the sync
  sliders, and the OSC endpoint with a copy button;
- point any OSC tool at **:9000** under its own prefix — e.g. djay Pro's OSC
  output sends `/djay/…`, and it fans out to every subscriber unchanged;
- `simply-tussen list` prints the audio devices and MIDI ports it can see;
  `simply-tussen listen --name me` registers a fake device and prints what it
  receives.

macOS is the tested platform, and the audio capture (CoreAudio, macOS 14.2+
for output taps) is macOS-only; routing, MIDI, Link and the console are
portable Rust, untested elsewhere. First audio run: macOS asks to let your
terminal record system audio — allow it.

A whole room — music laptops on Link, a mixer into the hub, a visuals
laptop, performance devices — is [docs/guides/setup.md](docs/guides/setup.md).

## How signals flow

```
MIDI / gamepad / Link ──► simply-tussen :9000 ◄── OSC: devices, djay Pro, /ext/ tools
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
| `[[dest]]` | a static destination: `name`, `host`, `port`, `subscribe`/`exclude` prefixes, optional `rtp_port` for the audio stream; or `audio_out = "<device>"` (no OSC fields) to play the audio stream on a local output; optional `modes` (`run`, `replay`; default: all for a network dest, `["replay"]` for an `audio_out`) |
| `[audio]` | the capture: `""` = default output (tapped), or a device name from `simply-tussen list` |
| `[midi]` | MIDI inputs (`"*"` = all) and the refresh cadence |
| `[pad]` | game controllers on this machine (`enabled`, name filters, refresh) |
| `[link]` | the hub's own tempo (while it waits, and for a session started from the console) and quantum |
| `[sync]` | the two delays, plus per-device `follow` overrides — the console writes these |
| `[[remap]]` | per-destination address rewrites with optional range mapping — the console writes these |
| `[console]` | the console port (loopback only) |

Which file: `--config <path>` if given, else your user config
(`~/.config/tussen/hub.toml`, or under `$XDG_CONFIG_HOME`), else the nearest
`hub.toml` up from the current directory — the repo's shipped example —
else that same example, built into `simply-tussen`. The startup banner names
the pick (`config → <path> (user)`). `--audio-device "<name>"` overrides
`[audio] device` for one run.

## The console

`http://127.0.0.1:9098`. Left: destinations, devices, Link peers and tempo.
Middle: the signal timeline — audio waveform, MIDI markers, the beat grid —
plus the anchored alignment strip for setting the signal delay by eye or by
drag. Right: every address ever seen (direction, min/max, rate), remap rules,
and the osc-traffic tab: a live scrolling feed of every message. **Save**
writes the sync and remap state back into the config in use, comments
intact — into your user config if the hub was running the shipped example (it
copies the example there first, then uses that file).

## Record and replay

Press **● Record** in the console's top bar to record a take (every
signal, the audio and the Link timeline); then, with nobody performing:

```sh
./simply-tussen takes                          # name, length, size
./simply-tussen replay latest --loop 9..17     # loop bars 9–17, as if live
```

Consumers see a replay as live, the take leads the Link session, and the
sync delays apply as set, so it can drive TouchDesigner and the devices
end to end. To hear it, add a local output destination to hub.toml
(`[[dest]]` with `audio_out = ""` for the default output). It plays in
`replay` only unless its `modes` says otherwise (`modes = ["run"]` for
live headphones). `./simply-tussen export latest` packs a take into one
zip to share (`--no-audio` for the signals alone), and
`replay <file.zip>` plays one from anywhere.
Guide: [docs/guides/record-and-replay.md](docs/guides/record-and-replay.md).

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

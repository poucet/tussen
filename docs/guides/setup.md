# Setting up a room

The hub on one machine is the README's quick start. This guide is the rest:
several laptops, a mixer and a visuals rig, each doing its part, with the
hub lining it all up. Who runs what, then the steps on each machine.

```
music laptops (DAW, DJ software, …) ──audio──► mixer ──► hub laptop (audio interface → capture device)
     ▲  Ableton Link + OSC from the hub                         │
     │  DJ software ──OSC──► hub :9000                          │
     └──────────────────────── hub laptop ◄─────────────────────┘
                                   │  OSC/MIDI signals + RTP audio
                   ┌───────────────┴───────────────┐
                   ▼                               ▼
          visuals laptop (TouchDesigner, …) ──► performance devices (phone, headset, laptop)
```

Every one of these roles can share a machine; the shipped `hub.toml` runs all
of them on one, and a real room is that file with real hostnames in it.

## Network and ports

One dedicated router, client isolation off, multicast and broadcast allowed.
Wire the hub and visuals laptops if you can. No IPs to note down: address
laptops by their `.local` name (macOS: System Settings ▸ General ▸ Sharing ▸
Local hostname). The hub re-resolves every hostname in `hub.toml` every 5 s,
so a laptop that boots after the hub, or gets a new IP, is picked up without
a restart.

| Port | Who listens | For |
|---|---|---|
| 9000 | hub | everything sends OSC here |
| 9098 | hub (loopback only) | the console and its websocket |
| 3928 | the music software (`engine` destination) | hub → engine OSC, never delayed |
| 9001 | visuals (`td` destination) | hub → visuals OSC |
| 5006 | visuals laptop | the music as RTP (`rtp_port` of the `td` destination) |
| whatever it says | each device | its signals and its own audio stream, from its hello |

All of these except 9000 and 9098 are per-destination settings in `hub.toml`.

## Hub laptop

1. **Audio in.** Mixer output → the audio interface. The hub captures any
   device `tussen-hub list` prints: a device with inputs is read, an
   output-only one is tapped (a CoreAudio process tap, macOS 14.2+; the
   output still plays). `""` taps the default output — fine on one machine,
   where the music software plays locally. With a mixer, name the
   interface, or a loopback device carrying its input.
2. **Edit `hub.toml`** — three lines for a room, each with a commented
   example under it:

   | Section | One machine (as shipped) | A room |
   |---|---|---|
   | `[[dest]] engine` | `host = "127.0.0.1"` | `host = "<music-laptop>.local"` |
   | `[[dest]] td` | `host = "127.0.0.1"` | `host = "<visuals-laptop>.local"` |
   | `[audio]` | `device = ""` | `device = "<the capture device's exact name>"` |

   The `djay` destination is OSC *into* djay Pro; point it at the DJ laptop
   or delete it. Use real names; a typo shows up as a resolve warning in
   the log.
3. **Run it:** `tools/tussen` from the repo root. It builds the hub,
   starts it, prints the console URL and the OSC endpoint to hand to the
   music laptops, and opens the console (its top bar has a copy button for
   the endpoint). Ctrl-C stops it. `tools/tussen --fake-midi` stands in
   for a MIDI controller; `--audio-device "<name>"` overrides `[audio]` for
   one run.
4. **First run on macOS:** allow incoming connections, and let the terminal
   record system audio. Dismissed it? System Settings ▸ Privacy & Security ▸
   *Screen & System Audio Recording* (*System Audio Recording Only*) → the
   terminal app, then restart the hub.
5. **Check:** the start-up log names the capture (`audio: tapping output …`
   or `audio: capturing input …`), and the stats line reads
   `audio=187.5pkt/s` once anyone is listening. `tussen-hub listen --name me
   --audio` registers a throwaway device and prints the stream's peak level.
6. **Link.** The hub never founds an Ableton Link session by itself. It
   starts *waiting* (the console's Link field says `Link: waiting`), with a
   beat of its own at `[link] tempo`, and joins the first session it hears
   at that session's tempo — a DJ who switches Link on later keeps their
   tempo. When everyone else has left a session it joined, it waits again.
   If nobody else will join, press **start session** in the console: the
   hub founds one at the current tempo and stays in it until it stops. In
   a session the console sets tempo and start/stop for every peer; click
   the peer count to see who is connected.
7. **MIDI controllers** on this laptop publish `/tussen/midi/…`
   (`[midi] inputs = ["*"]` takes every port). A Novation Launchpad X sends
   per-pad pressure only in *Polyphonic Aftertouch* mode (Settings ▸ the
   third Scene Launch button); on *Channel Pressure* you get the single
   `/tussen/midi/pressure`.
8. **A game controller** plugged into this laptop (USB or Bluetooth)
   publishes `/tussen/pad/…`. Check: the console's osc-traffic tab streams
   `/tussen/pad/…` when you move a stick.

## Music laptops

- **The engine** — whatever software makes the sound from the signals:
  turn its OSC input on, on the port the `engine` destination sends to,
  and turn Link on.
- **Link-only players** (Rekordbox in Performance mode, Live, …): Link on.
  That is the whole integration: they share the beat.
- **djay Pro:** Link on. Settings ▸ OSC output → host = the hub laptop's
  `.local` name (or its IP if djay rejects the name), port **9000**. Its
  `/djay/…` addresses then reach every subscriber with no hub config. For
  OSC *into* djay, match its input port to the `djay` destination's.
- **A program that only follows MIDI clock:** run a Link-to-MIDI-clock
  bridge on that laptop and set the program to follow MIDI clock.
- **Any other OSC tool:** send to the hub on :9000 under your own prefix
  (`/ext/<tool>/…` reaches every default subscriber).
- Audio out → the mixer.

## Visuals laptop

The `td` destination gets the signals on :9001, held back by the signal
delay so they land with the music, and the music itself as a standard RTP
stream on :5006: L16, 48 kHz stereo, payload type 96, 256 frames a packet
(`tussen-hub sdp` prints the same as an SDP file). No tussen code runs here:
GStreamer turns the stream into a loopback audio device, and the visuals
software reads that device.

```
hub ──RTP :5006──► gst-launch-1.0 ──► loopback device ──► your visuals software
```

**Once** (macOS):

1. A loopback device: Rogue Amoeba's Loopback, or the free
   [BlackHole 2ch](https://github.com/ExistentialAudio/BlackHole)
   (`brew install blackhole-2ch`).
2. GStreamer: `brew install gstreamer`, or the official runtime `.pkg`
   (then `export PATH=/Library/Frameworks/GStreamer.framework/Versions/1.0/bin:$PATH`).
   Check: `gst-inspect-1.0 rtpL16depay` and `gst-inspect-1.0 osxaudiosink`
   both describe an element.
3. The loopback device's id — `osxaudiosink device=` takes a number, not a
   name: `gst-device-monitor-1.0 Audio/Sink` prints a ready-made
   `osxaudiosink device=<id>` under each device. The id can change after a
   reboot or a replug.

**Each session** (leave the terminal open):

```sh
gst-launch-1.0 udpsrc port=5006 \
  caps="application/x-rtp,media=audio,clock-rate=48000,encoding-name=L16,channels=2,payload=96" \
  ! rtpjitterbuffer latency=20 ! rtpL16depay ! audioconvert ! audioresample \
  ! osxaudiosink device=<id> buffer-time=20000 latency-time=5000
```

- `rtpjitterbuffer latency=20` holds 20 ms against network jitter (its
  default is 200 ms — never leave it out). Crackles on Wi-Fi: raise it to
  30–40.
- `buffer-time`/`latency-time` (µs) shrink the sink's buffer; raise
  `buffer-time` if the output underruns.
- To try it on one machine: run the hub as shipped (`td` is `127.0.0.1`)
  and run the line above; swap the sink for `fakesink` to test the pipeline
  silently.

Expect the visuals to hear the music some 30–50 ms after the hub captured
it, plus the visuals software's own input buffer. The signal delay already
moves the signals onto the audio the hub *sends*; if the visuals still see
signals early, measure the remainder once (a click on the beat in the mix
against `/tussen/clock/onbeat`) and add it to the signal delay.

**In TouchDesigner:** an *Audio Device In* CHOP on the loopback device
(small Buffer Length — it is latency); an *OSC In* CHOP on port 9001
(Queued on, so one-shot events like `onbeat` survive); an *Ableton Link*
CHOP for tempo and bar counting. The OSC clock is lined up with the music
TD hears; the Link CHOP runs ahead of it by the signal delay. Every
address: [osc-signals.md](osc-signals.md). Anything else with an OSC input
and an audio input works the same way.

## Performance devices

A device is any app that registers with the hub; the hub then sends it the
signals it subscribes to and, if it asks, its own copy of the audio. A
ready-made one is the tussen Unity app (laptop, Android phone, headset —
separate repo). Writing your own takes three messages, all to the hub's
:9000:

| Message | Arguments |
|---|---|
| `/tussen/hub/hello` | name, the port it listens on, subscribe prefixes (comma-separated), exclude prefixes (comma-separated), its RTP audio port or 0 |
| `/tussen/hub/welcome` (hub → device) | the hub's OSC port, the device's name, the visuals laptop's IPv4 or `""` |
| `/tussen/hub/bye` | name |

Send the hello about once a second: it is also the keep-alive, and a device
silent for `[osc] device_timeout_ms` (3 s) is forgotten. A device that
doesn't know where the hub is can broadcast its hello to port 9000; the
welcome arrives from the hub's IP (not from :9000 — use the port in its
first argument). Where the router drops
broadcasts, give the device the hub's IP instead. A device publishes its
own signals under `/tussen/<name>/…`.

The welcome's third argument is the `td` destination's address — so a
device that composites the visuals laptop's picture (say, NDI over its
camera) can find it even where mDNS is blocked. That picture never passes
through the hub; the hub's part is timing (sync point 2 below). This is
also why the room's `hub.toml` needs the visuals laptop's real name, not
127.0.0.1.

## Soundcheck, in the console

Open <http://127.0.0.1:9098> on the hub laptop. Changes apply at once;
**Save** writes them into `hub.toml`, comments kept. To practise without a
hub, open `hub/console/index.html?mock=1` (a fake hub whose MIDI leads its
kicks by 70 ms; `&lead=<ms>` changes that).

The music comes back through the mixer *after* the MIDI, OSC and beat that
caused it, and a visuals picture reaches a device later again. Two numbers,
both set by hand:

| | Number | What waits | Who gets it |
|---|---|---|---|
| **Sync point 1** | signal delay | MIDI + OSC, the beat included; never the audio | the visuals destination, and every device |
| **Sync point 2** | TD round trip | everything the hub sends a device, its audio included | every device set to *in time with TD* |

The engine is never delayed: it makes the audio.

1. **Sync point 1.** Play something with a clear attack (a kick on a pad).
   The timeline shows the waveform and, ahead of each hit, the MIDI marker
   that caused it. Raise the signal delay — or drag the waveform in the
   anchored strip onto the beat line — until the markers and the Link grid
   sit on the kicks. ←/→ = ±1 ms, ⇧ = ±10. Save.
2. **Sync point 2.** Nothing streams back to the hub, so judge it where it
   lands: with the visuals flashing on `/tussen/clock/onbeat` and the
   device flashing on the same beat, raise the TD round trip until the two
   coincide on the device's screen. If that holds the device too far behind
   the room, flip its switch to *in time with the music*. Save.
3. **Check:** the stats line shows `devices=<n>` and `audio=187.5pkt/s`;
   the visuals' audio input moves; every Link app shows 1+ peers.

**Remapping** an address for one destination: wiggle the control (its
address jumps to the top of the address list), click it, pick the dest,
type the address its binding expects, optionally a range (`lo`..`hi`), and
tick *keep* to send the original too. `*` matches one path segment. Save.

## If something's off

- **Hub won't start, port 9000 busy:** another hub is running —
  `lsof -i :9000`.
- **A device never registers from another machine:** the network drops its
  broadcast hello (client isolation, guest Wi-Fi). Use a network that
  doesn't, or give the device the hub's IP.
- **No audio stream:** the hub streams only to someone who asked — a
  registered device with an RTP port, or a destination with `rtp_port`.
  Silent stream: is the music actually on the device the hub captures (its
  start line names it; `tussen-hub list` marks the default)? A tap that logs `only digital
  silence` is missing the system-audio-recording permission (hub laptop,
  step 4).
- **Console shows nothing:** `[console] enabled = true`, and only
  `127.0.0.1` works, not the laptop's LAN IP.
- **The visuals laptop's sound crackles:** raise `rtpjitterbuffer latency`
  first, then the visuals software's input buffer. Sound stops after a
  replug: look the device id up again.

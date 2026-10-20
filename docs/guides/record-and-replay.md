# Record and replay

Record a whole performance through the hub, then replay it when nobody is
performing, so you can keep working on shaders, TouchDesigner or a device
app against the real show. Everything downstream sees a replay as a live
performance: the same OSC addresses, the same RTP audio stream, the same
`/tussen/clock/…` beat, and an Ableton Link session that follows the take.

The design record behind this is [docs/design/replay.md](../design/replay.md).

## Quick start

1. Start the hub as usual: `./simply-tussen run`.
2. In the console's top bar, press **● Record**. It turns into **■ Stop**,
   with the elapsed time and the take's name beside it
   (`● REC 2:41 · 2026-10-20T21-14-03`).
3. Press **■ Stop** when the part you want is done. Record again as often
   as you like; every press makes a new take.
4. Quit the hub, then replay:

```sh
./simply-tussen takes                          # what's recorded
./simply-tussen replay latest                  # loop the newest take
./simply-tussen replay latest --loop 9..17     # loop bars 9 to 17
```

`./simply-tussen replay` opens the console, as `run` does.

## What's recorded, and what isn't

The hub records what **enters** it, stamped with the time it arrived.

| Recorded |
|---|
| All OSC sent to the hub: devices (head, controllers, phones), djay Pro's `/djay/…`, `/ext/…` tools, each with its sender's address |
| MIDI from controllers plugged into the hub machine (`/tussen/midi/…`) |
| A gamepad on the hub machine (`/tussen/pad/…`) |
| The captured audio, exactly what went out over RTP, timed by when each packet left the hub |
| The Link timeline the hub publishes: tempo, beat, start/stop |
| The two sync delays (signal delay, TD round trip), at the start and at every change |
| Device registrations, so the take knows who was there. These are **never replayed**, so no phantom devices appear. |

**Not recorded, or not replayed:**

- **NDI.** It never passes the hub. During a replay TouchDesigner renders
  and sends it fresh (see [below](#testing-the-visual-path-with-no-performers)).
- Remap rules and follow switches. A replay uses the ones set on the hub
  that replays it.
- The `/tussen/clock/…` messages are recorded but not replayed: a replay
  makes them fresh from the recorded Link timeline, so they stay in step
  across loops.

**Starting mid-show.** A take started while signals are already flowing
first stores the last value of every address the hub has routed (knobs,
faders, held notes, poses; events like `…/press` excluded) and the current
Link timeline. A replay from its start then sees a consistent world.

## Recording

- **The console's Record button** is the normal way, at any time during
  `run`. Pages opened read-only (`?readonly=1`, or another local site)
  can't record.
- `./simply-tussen run --record [NAME]` starts a take at launch, named
  `NAME`. Stop it from the console, or stop the hub.
- Takes are named by their UTC start time, plus the name if given:
  `2026-10-20T21-14-03-rehearsal`.
- **A replaying hub doesn't record.** During `simply-tussen replay` the
  Record button stays in its place, greyed out ("recording is off during
  replay"), and a `record_start` sent over the websocket is refused.

## Replaying

```
simply-tussen replay <take> [--loop A..B] [--from BAR] [--offset-ms N]
                            [--replay-link lead|off] [--config FILE] [--takes-dir DIR] [--stats]
```

| | |
|---|---|
| `<take>` | `latest`, a take's folder name, the end of one (`rehearsal`), or a path |
| `--loop A..B` | loop whole bars of the recorded Link timeline: from bar A's first beat to bar B's (`9..17` is 8 bars). Without it, the whole take loops |
| `--from BAR` | start at bar BAR (then loop as above) |
| `--offset-ms N` | a manual nudge: play the audio N ms ahead of the signals (negative: later). Default 0 |
| `--replay-link off` | leave Link alone (default `lead`, below) |
| `--config`, `--takes-dir`, `--stats` | as for `run` |

**What changes while replaying.** `replay` starts the same hub as `run`:
the same config, destinations, devices, RTP stream, sync delays and
console. Only the sources change:

- The hub's own MIDI, gamepad, audio capture and Link are off; the take
  plays instead.
- OSC from the network still arrives and is routed as usual, so a device
  you are testing with works normally.
- In the console, the top bar shows `REPLAY <take> · bar 10.3/24 · loop 9–17`,
  and replayed signals carry `src: "replay"` in the OSC feed.

**Loops and starting partway in.** Bars are counted on the recorded Link
timeline (bar 1 is the first bar line in the take). At every loop seam,
and when starting with `--from`:

- every address gets the value it had at that point, if it differs from
  what was last sent;
- MIDI notes and pad buttons not held at that point are released;
- events (`…/press`, `…/tap`, `onbeat`, …) are never fired again;
- the beat keeps counting up, in the same bar phase as recorded.

A loop of whole bars has no gap at the seam. The whole take, which
rarely ends on a bar line, waits less than a bar at the seam so the beat
stays in phase. The audio fades over 5 ms at each seam.

## Timing

- **A replay reproduces the hub's ingress.** Audio and signals play at
  the exact times they entered the hub, relative to each other. The
  mixer's latency is already in the recorded audio timing; nothing is
  shifted.
- **Your current sync delays apply**, as they do live: the signal delay
  for TouchDesigner and the devices, the TD round trip for devices that
  follow TD. With the venue's settings, a replay lines up exactly as the
  show did. You can still adjust them in the console during a replay.
- `--offset-ms` is there if you want to nudge the audio by hand.

## Hearing a replay

The hub has no speakers of its own. A local output is a **destination**:
add one to the hub config and the hub's audio stream plays on that
device. By default only in `replay`: live, the room already hears the
music, and playing it again doubles it and can feed back.

```toml
[[dest]]
name = "speakers"
audio_out = ""                       # this machine's default output
# audio_out = "MacBook Pro Speakers" # or a name from `simply-tussen list` (a unique part works)
modes = ["replay"]                   # the default for an audio_out
```

**`modes` says when a destination exists.** Any `[[dest]]` takes
`modes = ["run", "replay"]`, a list of the modes it is active in.

- Without `modes`, a network destination (`host`/`port`) is active in
  every mode, and an `audio_out` one in `replay` only.
- Set it to change that: `modes = ["run"]` monitors the live capture on
  headphones, `modes = ["run", "replay"]` plays in both.
- `"mesh"` is accepted and reserved: there is no mesh mode yet, so it
  never activates. Any other value is a config error naming the
  destination.
- An inactive destination is not created at all: no socket, no RTP, no
  device opened, not in the console. The startup lists it as skipped:
  `dest speakers → audio out "" — replay only, inactive in run`.

- An `audio_out` destination has no OSC (no `host`, `port` or
  `subscribe`). The shipped hub.toml has one commented out.
- The startup lists it with the other destinations:
  `dest speakers → audio out "MacBook Pro Speakers" (44100 Hz), kind engine`.
  An output that can't be opened is a warning, not a stop: the hub runs
  without it.
- During `run` it can't be the output `[audio] device` taps (the default
  output, with `device = ""`): it would capture itself, so the hub refuses.

**It gets what any RTP consumer gets.** Inside the hub it is an RTP
receiver on loopback, fed the same stream through the same delay ring as
the devices. Loops and seams sound exactly as they do on the RTP stream.
Its `kind` picks the delay, by the same rule as every other consumer's
audio:

| `kind` | audio delay |
|---|---|
| `engine` (default) | none: the room, hearing the music as it entered the hub |
| `device` | the TD round trip (unless `[sync.device.<name>] follow = "music"`), in time with the devices' picture |
| `td` | none (TouchDesigner's audio is never delayed) |

The output plays once 20 ms has arrived (its jitter buffer), so it runs
about 20 ms plus the device's own buffer behind the stream. If the device's
clock drifts and more than 80 ms piles up, it skips back to 20 ms.

**Rate and channels.** A device at another rate (44.1 kHz, 96 kHz) gets the
stream resampled linearly. That's fine for monitoring, not for a master.
Stereo plays to the first two channels of a multichannel device, and is
averaged onto a mono one.

## Link

During a replay the hub **leads** the Link session with the take's
timeline:

- With nobody else in a session, it starts one at the take's tempo and
  beat.
- If a session exists, it joins, proposes the take's tempo, and starts
  the take on that session's next bar line, in the right bar phase.
- Recorded tempo changes and start/stop are proposed as they come.
- At every loop seam the take is lined up with the session's bars again,
  so it stays aligned even if someone moved the session.

TouchDesigner's Link CHOP and other Link apps follow along. A DJ in the
same session is moved to the take's tempo; use `--replay-link off` to
leave Link alone (then read the beat from `/tussen/clock/…`).

## Testing the visual path with no performers

A replay drives TouchDesigner live, so the whole visual chain can be
tested with no performers, no live audio and no live signals:

1. Use the room's config: the `td` destination with its `rtp_port`, and
   the sync delays set at soundcheck.
2. `./simply-tussen replay latest --loop 9..17` on the hub laptop.
3. TouchDesigner: Link CHOP on (it follows the replay), the audio input
   on the RTP stream and OSC In as at a show. It renders and sends its NDI
   to the devices as usual.
4. Open the devices. They register, get the replayed signals and audio
   with both sync delays applied, and composite TouchDesigner's NDI.
5. Judge the alignment on a device as at soundcheck
   ([setup.md](setup.md#soundcheck-in-the-console)). The loop repeats the
   same bars, so there is time to adjust.

## A take on disk

One folder per take, in `~/Music/tussen/takes/` (or
`$XDG_MUSIC_DIR/tussen/takes/`; `--takes-dir` picks another):

```
2026-10-20T21-14-03-rehearsal/
  audio.wav     the audio the hub streamed; 16-bit, the [audio] rate and channels (48 kHz stereo)
  events.bin    every signal (raw OSC bytes and sender), Link changes, delay changes, audio timing anchors
  take.toml     start time, duration, audio format, event count
  hub.toml      the hub config in use when recording started
```

- `audio.wav` opens in any DAW.
- `events.bin` is a compact, append-only binary log.
- **Crash-safe:** a take is written as it goes and flushed four times a
  second. If the hub dies mid-recording, the take stays valid; at most the
  last quarter second is lost.
- **Size:** about 0.7 GB per hour of audio, plus up to about 1.9 GB per
  hour of events for a busy show. `simply-tussen takes` shows each take's
  length and size.
- A replay holds the take's event log in memory: fine for rehearsals,
  heavy for hours-long takes.

## Later

- Console transport: play and pause, a take list, a draggable loop.
- Live pads played over a replay.
- Smaller, shareable takes: compression, FLAC, trimming, anonymised
  exports.
- Always-on recording of the last N minutes.

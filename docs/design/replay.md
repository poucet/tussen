> **Status: decided and partly built.** Chris settled the decisions on 2026-10-20 (below, *Settled*); stages S1 and S2 are built, with Link lead pulled forward from S4 (§ As built). Nothing here changes a hub that is not recording or replaying.

# Replay: record a performance, play it back as if live

## Summary

1. **A take** is one directory: the hub's audio as a WAV, every signal the hub routed as an append-only event log, the Link timeline, the sync state, a copy of the config.
2. **Recorded at ingress**: in `Loop::route`, the one function every OSC message, every generator signal and every hello passes through. Before remap and before the sync delays.
3. **Replayed at ingress**: a replay is a **generator**. It feeds the recorded signals back into the router and the recorded audio into the existing RTP sender. Remaps, sync points, subscriptions and late-registering devices all work unchanged.
4. **Consumers change nothing**: same addresses, same RTP stream (one SSRC, continuous sequence), same `/tussen/clock/…`. The websocket's `src` says `"replay"`. That is the one visible mark.
5. **One clock**: everything the hub stamps is `Instant`, which on macOS is `mach_absolute_time`. Takes store µs since take start.
6. **A replay is the hub's ingress, reproduced**: audio at its recorded packet stamps, signals at their recorded arrival, no lead (D6). The rig's sync points apply unchanged and align them as they did live.
7. **Crash-safe by construction**: no footer, no index at close. Ctrl-C while tapping already exits without destructors (`hold()`), so a take is valid at every flush.
8. **Iteration is a bar-aligned A–B loop** that replays the state at A on every pass, so a shader sees a consistent world each time.
9. **Link** (settled: built with S1): while replaying, the hub **leads** the local Link session with the take's timeline. It founds a session if none exists, and otherwise places the take on that session's bar grid and proposes the take's tempo. TD's Link CHOP and Ableton follow. `--replay-link off` leaves Link alone.
10. **Built for the hackathon**: the console's Record button (or `run --record [NAME]` at launch), then `simply-tussen replay <take|latest> [--loop A..B] [--from BAR]` loops in real time.
11. **NDI is out**: the hub never carries it, so a take cannot hold it.

Numbers, measurements and prior art: [docs/research/replay-numbers.md](../research/replay-numbers.md).
Claims carry □ (follows from the code or arithmetic), ◇ (possible or measured; evidence named), 🚫 (impossible here; the lever named).

## As built (2026-10-20)

User guide: [docs/guides/record-and-replay.md](../guides/record-and-replay.md). This doc stays the design record.

- **Recording**: the console's **Record** button (top bar; elapsed time and take name while on) at any time, over `record_start` / `record_stop`, its state in `hello.record`. A take started mid-run is seeded at t = 0 with the last value of every non-event address the router has routed (with its source) and the current Link timeline. `run --record [NAME]` starts one at launch. A replaying hub has recording wired as live but switched off (`hello.record.off`; the button shows, disabled; `record_start` refused), so layering a recording over a replay later is a one-line change in `cmd/run.rs`.
- **Replay is its own command** (settled, superseding D10's "flags on `run`"): `simply-tussen replay <take|latest|path> [--loop A..B] [--from BAR] [--offset-ms N] [--replay-link lead|off] [--config …] [--takes-dir DIR]`. It runs `run`'s startup (one shared path in `cmd/run.rs`), with the take as the sources. `./simply-tussen replay` opens the console as `run` does. `--loop` is whole bars, from bar A's first beat to bar B's; `--offset-ms` is a manual audio nudge, default 0 (D6: no lead). `simply-tussen takes` lists takes.
- **Streams recorded**: OSC, MIDI and pad (all at `route`, with origin), audio (the RTP sender's blocks, packet stamps as anchors), the Link timeline (tempo, beat, start/stop, mode, quantum, from the link thread), the signal delay. Not NDI.
- **Code**: `hub/src/take.rs` (format, writer, reader), `hub/src/replay.rs` (grid, plan, event thread, audio pacer, Link lead), the `route` tap and `Src::{Net, Hub, Replay}` in `hub/src/hub.rs`, the audio tap in `generators/audio/mod.rs`, the timeline tap in `link.rs`.
- **Tests**: `hub/tests/replay.rs` (a synthetic session recorded and replayed with timing, audio and tempo checks; a take cut mid-write; a bar loop with state restore; Link lead founding and joining with simply-link peers), unit tests in `take.rs` and `replay.rs`.
- **Simplified from this doc**: the whole log is read into memory on open (fine for rehearsal-length takes); no `levels.bin`, `take dump`, markers or `[replay]` config section; the sync record holds only the two delays; audio anchors once a second and on slips over 20 ms; the seam fade is 5 ms in and out (not a crossfade); a replay's console `link` frame lists peers without reverse DNS.
- **Link lead, as built**: the replay listens for `link_wait` (1.2 s). Alone, it founds at the take's tempo and beat. In a session, it proposes the take's tempo and starts the take on the session's next bar line in the right bar phase, whoever else is there (Q2 below: leading is the default while replaying). Every recorded tempo change and start/stop is proposed at its own time. Every pass after the first is placed on the session's grid, so phase that someone else moved is caught up at the next seam. A whole-take loop waits less than a bar at the seam to stay in phase; a whole-bar loop never waits.

## Decisions

Ranked by how much each one changes. The ids are stable so review comments can cite them. **Settled** (Chris, 2026-10-20): every recommendation below stands except D5, which became "lead Link from S1", and D4's S5, which is later.

| # | Decision | Recommendation | What it changes |
|---|---|---|---|
| D5 | Link during replay (§5) | **Settled: the replay leads Link from S1.** Found a session if none exists (overriding waiting while replaying), propose the take's tempo and every recorded tempo change, place the take on the session's bar grid at the start and at every seam; `--replay-link off` leaves Link alone. `/tussen/clock/…` comes from the same timeline | TD's Link CHOP and Ableton follow a replay; contradicts "the hub never imposes a tempo" while replaying (§12) |
| D1 | Where to record (§1) | **Ingress**, in `Loop::route` (+ `RouterCmd::Sync`), audio at the sender's packetization, Link state in the link thread. Not egress per destination | replay re-uses router, remaps and sync points; a take is not a forensic record of what each consumer got |
| D6 | How audio and OSC line up on replay (§2.3) | **Settled: ingress-faithful, no lead.** Audio plays at its recorded packet stamp relative to the signals' recorded arrival; the rig's sync points align them as live. `replay --offset-ms` is a manual nudge, default 0 | a replay drives TD and the devices with live timing, so the visual path lines up as at the venue (§4.3) |
| D2 | Timebase (§2.1, §2.2) | **One host clock (`Instant`), µs since take start; audio timed by its RTP packet stamps** plus `(frame, τ)` anchors, not by capture stamps | replay reproduces what live consumers got; capture stamps stay the mesh's concern |
| D4 | Live inputs during replay (§4.2) | **S1: live network OSC merges; the hub's own generators (capture, MIDI, pad, Link) are replaced by the take.** S5: per-prefix *claims* decide, so a performer can play pads over a recorded set | what a shader dev can do live while a take plays |
| D3 | Event log format (§3.2) | **Our own length-prefixed binary log** (raw OSC bytes, µs, origin), index rebuilt by scanning on open; `take dump` to NDJSON. MCAP as the considered alternative | a dependency vs. ~150 lines; tooling (Foxglove) vs. none |
| D7 | Loop semantics (§6.1) | **Whole bars from the recorded timeline**; the beat keeps counting up across the seam; 5 ms audio crossfade | phase continuity for every beat consumer |
| D8 | State on seek and loop (§6.2) | **Restore the last value of every non-event address** at the target, plus note-offs; events (`…/press`, `onbeat`, `tap`, `recenter`) never re-fired | a consistent world after a jump; needs an event list |
| D11 | Recorded hellos and byes (§4.1) | **Recorded, never replayed** | no phantom devices at a venue's IP addresses |
| D10 | How a replay is started (§7) | **Settled otherwise: `simply-tussen replay <take>`**, its own command over `run`'s startup; recording from the console's Record button (pulled forward from S3), `run --record` as a launch shortcut | one config, one console; replay and live never share a process |
| D9 | Where takes live (§3.1) | **`~/Music/tussen/takes/`** (`$XDG_MUSIC_DIR` when set); `--takes-dir` overrides (Chris: takes are recordings — a visible folder with your music, not a hidden data dir) | where users find their takes |
| D12 | Playback rate (§6.3) | **1.0 only** until a later stage; varispeed (audio pitched) is the first step beyond | keeps audio and OSC trivially in step |

### Questions Chris answered (2026-10-20)

1. **Must TD's Link CHOP follow a replay at the hackathon?** Yes: Link lead is built with S1 (D5).
2. **May a replay drive a room with live Link peers?** Leading is the default while replaying; `--replay-link off` is the way out.
3. **Live pads over a replay** (S5): later.
4. **Always-on recording** (S7): later.
5. **The audio lead (D6)**: none. Replay is ingress-faithful (§2.3).

## §0 What the tree already answers

Code archaeology on `main` (`quplzywy`). Several questions shrink because of what exists.

- **One choke point for every signal** □. `Loop::inbound` flattens bundles and calls `route(m, Some(from), now)`; generator messages call `route(m, None, now)` (`hub/src/hub.rs`). Hellos and byes pass through `route` too. Welcomes do not: they go straight into `per_dest`.
- **Generators already publish "as if received"** □. `generators.rs`: "The hub routes them exactly like signals received on :9000". A replay generator needs no router change to be heard.
- **There is already a ring-fed RTP sender without a device** □. `generators/audio/mod.rs`, `stream_ring`: "Stream whatever is pushed into `ring` … as RTP, exactly as a capture does … For tests." Replay audio is that, paced.
- **Delays are computed from the router's `now`** □ (`sync::due(now, delay_ms)`). A message injected at the right time is delayed correctly with no change.
- **RTP packets are stamped when they are made** □ (`ring.next_slot(Instant::now())`), and so are the console's waveform levels (`LevelBatch::push`). The signal delay was set by eye against those stamps. Recording the same stamps reproduces the live stream (§2.2).
- **The capture timestamp exists and is dropped** □ (`ring_sink(…, _capture_ns)`). Replay does not need it (§2.2); the mesh proposal does.
- **The ownership rule is by address, not by origin** □ (`Router::routes`: a device never gets `/tussen/{name}/`). A live device named like a recorded one does not get the recorded one's signals (§9).
- **The hub re-bundles per destination per round** □ (`deliver`, `bundles::pack`). Consumers never see the sender's bundle boundaries, so a take need not keep them.
- **MIDI and pad state self-heal every 500 ms** □ (`midi.rs`, `pad.rs` `refresh_ms`). After a jump, hub-generated state recovers by itself; device signals recover within a frame at 60 Hz. Snapshots matter mostly for `/djay/`, `/ext/` and other on-change sources.
- **Ctrl-C exits without destructors while tapping** □ (`hold()`: `std::process::exit(128 + sig)`). A take cannot rely on a clean close.
- **simply-link has `found`, `leave`, `propose_tempo`, `propose_start_stop`; no phase move** □ (`simply-link/src/net.rs`). The mesh proposal wants `propose_timeline`. Placing the take on the grid (§5) needs none.
- **`link.rs` keeps its clock maths pure** □: `sample`, `Beats`, `next_wake`, `private` take a `SessionState` and a host time. A replay can publish `/tussen/clock/…` from a recorded timeline with the same functions.
- **Unknown config sections are ignored** □ (`config.rs` module doc). A `[replay]` section is additive for free.
- **`./tussen` passes its arguments to `run`** □ (`tussen` header comment). Flags on `run` reach the one-command launcher with no script change.

## §1 What is recorded, and where (D1, D11)

### §1.1 Ingress, not egress

| Option | Records | For | Against |
|---|---|---|---|
| **a. Ingress** (`route`, before remap and delay) | every message once, with its origin and arrival time | replay goes through today's router, remaps and sync points, so it is correct for **today's** rig □; a remap edited during replay applies; one copy per message | does not show what each consumer actually received |
| b. Egress per destination | what each destination got, after remap and delay | forensic: "what did TD see" | N copies; replay would have to bypass the router; remaps and delays baked in; devices that join later get nothing □ |
| c. Both | | | twice the disk for a question nobody asked yet |

**Recommendation: a.** The point is to iterate on the visuals, so the rig in front of you decides remaps and delays. Egress is reconstructible from an ingress take plus a config, which the take stores.

### §1.2 The four taps

| What | Where | Stamp |
|---|---|---|
| **Every routed message**: network OSC with its source address, generator signals (MIDI, pad, clock) | `Loop::route`, before the console tap | the router's `now` (the delays' own base) □ |
| **Sync state**: both delays, follow switches, remap rules | `Loop::command`, `RouterCmd::Sync` | the router's `now` |
| **Audio**: every 256-frame block the sender packetizes, as L16 | the sender thread, beside `packet_into` | the packet's `Instant` (§2.2) |
| **Link timeline**: tempo, beat at a host time, playing, mode, quantum | the link thread, on change and once a second | `clock.instant(now)` |

- The router never blocks on the recorder: it `try_send`s `(now, origin, bytes)` into a bounded channel. A full channel drops and counts (`rec_drop=` on the stats line) □.
- The recorder thread owns the files. The sender thread hands blocks over an SPSC ring, never writing a file itself.
- **Hellos and byes are recorded** as ordinary messages, so a take knows which devices were present. **They are never replayed (D11)**: a replayed hello would register a device at a venue's IP and stream to it □. The replay skips `/tussen/hub/`.
- **Not recorded**: welcomes (not routed), remap outputs (re-made on replay), the console's `watch` lists, Link peers' identities (the console frame is diagnostics, not performance).
- Origins are interned: a `u16` per distinct source (`192.0.2.21:50123`, `hub`). Takes therefore hold performers' LAN addresses. A shared take should be dumped with `--anonymize` (later stage).

## §2 One clock, and how audio and OSC are co-timed (D2, D6)

### §2.1 The timebase

- **Every stamp is an `Instant`** □: the router's `now`, the sender's packet stamp, simply-link's `HostClock`. On macOS that is `CLOCK_UPTIME_RAW` = `mach_absolute_time` (research §6).
- A take stores **µs since take start** (`t = 0` at the first record). `take.toml` also stores the hub clock at `t = 0` (`console::us_at`, the websocket's `ts` epoch) and the UTC wall time. A take maps onto the console's timeline and a human's diary.
- Replay maps take time τ to host time `h = h₀ + (τ − τ₀)` at rate 1. `h₀` is chosen at play, seek and loop seams.

### §2.2 Audio: the packet stamp, not the capture stamp

| Stamp | What it is | Use |
|---|---|---|
| capture `capture_ns` | when the device captured the first frame | the mesh: placing audio on a remote grid needs the true capture instant |
| **packet `Instant`** | when the sender made the RTP packet, a device buffer or so later | **what every live consumer received, and what `signal_delay_ms` was tuned against by eye** □ |

- **Recommendation: record the packet stamp.** Replaying it reproduces the live stream's timing, including the constant capture-to-packet lag that the recorded signal delay already absorbed □.
- Audio is stored contiguous (one WAV). Timing is **anchors** `(frame, τ)` once a second and at every discontinuity. Between anchors the sample clock rules. A ring overflow becomes a silence gap of the right length plus a `gap` record. The WAV never loses step □.
- `capture_ns` is recorded in the anchor too when the hub has it (the mesh carries it through the ring, S6). It costs 8 B a second.
- **Aligned with the mesh proposal**: the anchor is the mesh contribution header's `(frame, beat, tempo)` plus τ. A take's audio could later be offered to a mesh as a site.

### §2.3 Where recorded audio lands on replay (D6)

**Settled (Chris, 2026-10-20): ingress-faithful, no lead.** A replay reproduces the hub's ingress: every signal at its recorded arrival time, every audio packet at its recorded packet stamp, relative to each other exactly as they entered the hub live. The rig's sync points (`signal_delay_ms`, `td_round_trip_ms`) then align them as they did live.

- **Why the mixer does not matter**: live, a signal entered at `a` and the music it caused entered at `c ≈ a + M` (M: how late the mix comes back). The take holds both times, so M is already in the recorded audio timing. Replayed at those times, TD gets the signal at `a + D` and the music at `a + M`, as at the venue; a rig whose signal delay is tuned to the mixer lines them up as it did live. The signal delay in force while recording plays no part.
- **Dropped**: the proposal's options of playing the audio early by the recorded signal delay (as if the mixer had no latency), and their 30–50 ms error when TD's own audio path was folded into the delay. With no lead there is nothing to get wrong.
- `replay --offset-ms N` is a manual nudge (audio N ms ahead of the signals, negative behind), default 0.

## §3 The take (D3, D9)

### §3.1 A directory per take

```
~/Music/tussen/takes/2026-10-20T21-14-03-rehearsal/
  take.toml      # meta: version, UTC start, hub-clock at t=0, sample rate, channels,
                 #       packet frames, signal delay, markers, duration (rewritten each flush)
  audio.wav      # L16 stereo 48 kHz, exactly what RTP carried
  events.bin     # append-only log: OSC, sync, Link, audio anchors, markers
  hub.toml       # the config in use when recording started
  levels.bin     # [min,max,rms] per packet, for the overview waveform (S3)
```

- **Inspectable**: the WAV opens in any DAW; `tussen-hub take dump <take>` prints the log as NDJSON (`jq`-ready).
- **Where (D9)**: `~/Music/tussen/takes/` (`$XDG_MUSIC_DIR/tussen/takes/` when set): takes are recordings, so they sit visibly with the user's music rather than in a hidden data directory. `--takes-dir <path>` or `[replay] dir` override it.
- **Names**: UTC timestamp plus `--take-name`. `latest` resolves to the newest.

### §3.2 The event log (D3)

```
"TUSSTAKE" u16 version                      # file header
record := u32 len · u8 kind · u64 t_us · payload
  kind 1  osc      u16 origin · raw OSC message bytes (exact types, re-routable as is)
  kind 2  origin   u16 id · utf-8 ("192.0.2.21:50123", "hub")
  kind 3  sync     JSON of the sync state (both delays, follow, remaps)
  kind 4  link     f64 bpm · f64 beat at t · u8 playing · u8 mode · f64 quantum
  kind 5  anchor   u64 frame · u64 capture_ns or 0 · u8 flags (gap)
  kind 6  marker   utf-8 label
```

| Option | For | Against |
|---|---|---|
| **a. Own log** (above) | ~150 lines, no dependency, raw OSC bytes replay with no re-encode; a torn tail is just a short last record □ | own tooling: `take dump` |
| b. MCAP | spec'd; chunk index and compression; `mcap recover`; Foxglove plots channels over time ◇ | a dependency and a schema story for OSC; its index lives in an optional summary that a killed hub never writes, so we would rebuild it anyway ◇ |
| c. NDJSON | readable with `jq` | 2–3× bigger; OSC types (int vs float, blobs) need a convention; slower to scan ◇ |

- **Recommendation: a**, with `take dump --format mcap` as a later export if Foxglove proves useful.
- **No on-disk index**: the reader scans on open and builds the index in memory (§6.2). Sequential reads of 1–2 GB take seconds; a stage-1 take of a rehearsal is ~0.5 GB/h ◇. An index sidecar written opportunistically is a later optimisation. Like `rosbag reindex`, it is derived data, never trusted over the log.
- **Size** (research §2–4): **~1.2 GB/h** for a rehearsal (one device, MIDI, pad, clock), **~2.6 GB/h** for a three-device show. The **event log outweighs the audio** at that rate. Address interning (~2.3×) or zstd (~7× measured on a synthetic log ◇) is a later stage, not a stage-1 need.

### §3.3 Crash safety

- **Append-only, no footer, no close-time work** □. A take is valid after every flush.
- Writers flush every **250 ms** (a hub crash loses at most that) and `F_FULLFSYNC` once a second (power loss) ◇.
- **The WAV header's sizes are patched at every flush** and **repaired on open** from the file length □.
- **A torn last record** is detected by its length and ignored □.
- **Disk full or a write error stops the recording, never the hub**: a log line, a console flag, the show goes on.
- A take still being written can be opened read-only: the reader stops at the last whole record. So can a take whose hub was killed.

## §4 Replay semantics (D4, D11)

### §4.1 A replay is a generator

```
 events.bin ──► replay thread ──► (Message, Origin::Replay) ──► router: route() ──► remap ─► delay ─► every subscriber
                (precise wakeups)                                (the same code as live)
 audio.wav  ──► pacer thread ───► rtrb ring ──► stream_ring's sender ──► RTP: statics + devices, both sync points
```

- **OSC**: the replay thread sleeps to each record's `h(τ)` with the router's precise-wakeup policy (`timing::precise_wakeups`). Without it a macOS sleep overshoots by 3 ms on average, 10 ms at worst ◇ (`timing.rs`). It sends into the router as a generator does.
- **Origin**: a third origin besides "network" and "hub": `Origin::Replay`. The websocket's `osc` frame says `"src":"replay"` and the console's seen table marks those addresses. **The only visible difference** □, and a documented one (signals-websocket.md gains one value).
- **Skipped on replay**: `/tussen/hub/…` (D11). Clock records are skipped from S2 on (§5).
- **Audio**: a pacer pushes 256-frame blocks into an `rtrb` ring at `h(τ)`, and the existing sender (`stream_ring`) does the rest. Same packetizer, same SSRC, continuous `seq` and RTP timestamp across loops and seeks □. A receiver cannot tell a seam from a live transition. Devices and statics get it at their sync points, as live.
- **Devices registering during replay work**, because registration is live and unaffected □. That is how a shader dev connects: open the Unity app or TD; it says hello and gets the recorded performance from its next packet on.
- **Pause** sends silent RTP packets, so receivers' jitter buffers and analysis (rms, bands) see silence instead of an underrun ◇. OSC stops, and last values hold at the consumers.
- **Speakers**: the hub has no audio output today □. To hear a replay, use what exists: a static destination with an `rtp_port` and `ffplay` on the SDP from `tussen-hub sdp` ◇, or GStreamer into a loopback device as for TD (`setup.md`). Hub speakers come with the mesh proposal's output stage (`simply-audio-device`), and replay would reuse it.

### §4.2 Live inputs while a take plays (D4)

| Source | S1 | S5 (claims) |
|---|---|---|
| devices' hellos, registration | live □ | live |
| network OSC (devices, `/ext/` tools, djay) | **merged**: routed as live | merged, unless its prefix is **claimed** by the replay; then dropped and counted (`live_masked=`) |
| hub MIDI, pad generators | **not started**; the take's `/tussen/midi/`, `/tussen/pad/` play | started; the claim per prefix decides which plays |
| hub audio capture | **not started**; the take's audio plays | not started (no mixing; that needs the mesh mixer) |
| hub Link generator | **not started**; the take's `/tussen/clock/` plays | started; the take is placed on its grid (§5) |

- **A claim** is a prefix the replay owns: by default every source prefix present in the take (`/tussen/<dev>/`, `/tussen/midi/`, `/tussen/pad/`, `/djay/`, each `/ext/<source>/`). Releasing a claim on the console hands the prefix to live, and the replay drops its records for it. That is "play pads live over a recorded set" (S5).
- **S1 merges** because the realistic hackathon case has no collisions: the only live sender is the developer's own device, under its own name.
- **Not offered**: renaming replayed sources (e.g. `/tussen/replay/quest/…`). It would break the rule that consumers need no change. The mesh's `/mesh/<site>/` wrap is the place for that if it is ever wanted.

### §4.3 Testing the visual path with a replay

NDI is not recorded (the hub never carries it), but the live NDI path works during a replay, so the whole visual chain can be tested with no performers, no live audio and no live signals:

- TouchDesigner (`td` destination) renders from the replayed OSC, the replayed audio on its `rtp_port` (undelayed, as live) and the Link session the replay leads (its Link CHOP follows).
- TD sends its NDI straight to the devices, as at a show.
- The devices get the replayed signals `signal_delay_ms + td_round_trip_ms` late and the audio `td_round_trip_ms` late (if they follow TD), exactly as live, so they line up with the NDI coming back.

What makes this hold: the replay injects at `route`, so both sync points apply unchanged, and the audio plays at its recorded ingress time (D6, no lead). Test: `a_replay_drives_td_and_a_device_through_both_sync_points` in `hub/tests/replay.rs` (60 ms signal delay, 40 ms round trip: hit and music meet at TD within 8 ms, and at the device 40 ms later).

### §4.4 Recording while replaying

**Refused in S1.** Later, "bounce" (record what a replay with live claims produced) is a natural edit primitive. The recorder taps ingress, so a bounce takes both streams with no new code. The refusal is policy, not mechanism.

## §5 Link during replay (D5)

> **Settled**: the replay leads from S1, in every hub state (§ As built). The options below are the reasoning as proposed; §5.3's placement rule is what was built, with the tempo always proposed.

### §5.1 What consumers read

- `/tussen/clock/…` over OSC and the websocket (every hub consumer) □.
- **The Link session itself**: TouchDesigner's Ableton Link CHOP, Ableton Live, djay (`setup.md`) □. These follow the network session, not the hub's OSC.

### §5.2 Options

| Option | `/tussen/clock/…` | Link session | Cost |
|---|---|---|---|
| **a. S1: hub not a Link peer while replaying** | the take's own records, replayed | untouched; TD's Link CHOP shows whatever else is there | zero; the Link CHOP disagrees with the take |
| b. Drive Link to the take's beat | from the session | phase moved to the take on start, seek and loop | needs `propose_timeline` (simply-link, also wanted by mesh S2b); yanks every peer's phase on every seek |
| **c. S4: place the take on the grid** | from the live session (the Link generator, unchanged) | tempo set to the take's; **phase never moved**: the take starts on a bar of the live grid | launch quantisation (≤ one bar's wait on play and seek); absolute beat numbers differ from the take's, bar phase does not |

**Recommendation: a for S1, c for S4.** The rule: **the replay is placed onto the Link grid, never the grid onto the replay.** It is Ableton's own launch quantisation, and it needs nothing simply-link lacks.

### §5.3 Placement, precisely (S4)

- **Start or seek** to take position τₛ: pick the next live beat `b` with `b ≡ beat_rec(τₛ) (mod quantum)`, and set `h₀` so that `h(τₛ) = host_at_beat(b)` □.
- **Tempo**: the take's tempo at τₛ is proposed when the replay starts, then at every recorded tempo change. One mechanism (`propose_tempo`), as the console's tempo field uses today □.
- **A–B loop of whole bars**: `beat_rec(B) − beat_rec(A) = n · quantum`, so at the seam the live beat continues and `beat_rec(A) ≡ beat_rec(B)` □. **No wait, no phase move.** Not whole bars means a wait at every seam, so loop points snap to bars (§6.1).
- The beat keeps counting up across seams. `onbeat` stays regular, and phase-driven shaders never jump □.

### §5.4 Interaction with waiting and joining

| Hub state when replay starts | S4 default (`--link auto`) | `--link lead` |
|---|---|---|
| **waiting** (nobody else; the home case) | **found a session** at the take's tempo, like the console's *start session*: TD and Ableton then join it | same |
| **joined/started, alone in it** | propose the take's tempo | same |
| **joined, with other peers** (a venue, DJs live) | **do not propose**; place by phase at the session's tempo. Audio is not stretched, so if tempos differ the console warns: "take at 124, session at 126: drifting" | propose the take's tempo; the human chose to drive the room |

- A replay-founded session behaves as a console-started one: it stays until the hub stops (`link.rs` rule) □.
- **Pause**: no Link change by default. `--link transport` adds start/stop proposals for peers that follow transport (Live).
- **Someone else changes tempo mid-replay** (`--link auto`): the replay keeps its speed, re-places at the next bar it can, and warns. It never re-asserts. This keeps the hub's "no stance to re-assert" principle □ (`link.rs` module doc).

## §6 Transport: loop, seek, state (D7, D8, D12)

### §6.1 Loops (D7)

- **The core of "iterate on one part"**: loop bars 33–41 while editing a shader.
- Loop points come from the recorded Link timeline. Bars count from the take's first bar line `b₁` (the first multiple of `quantum`, Link's phase convention): bar `n` starts at `beat_rec = b₁ + (n − 1) · quantum`, mapped back to τ. A take without a session (`mode = waiting`) still has the hub's own beat □, so bars always exist.
- **Snap to whole bars** by default; `--loop-ms A..B` for free loops (they wait or jump at the seam under S4).
- **Audio seam**: a 5 ms crossfade, as in the mesh proposal's gap fades. RTP stays one continuous stream □.
- **Clock across the seam**: S1 replays recorded clock records, so `/tussen/clock/beat` jumps back once per pass of the whole take. **From S2 the replay publishes `/tussen/clock/…` itself**: `link::sample` and `link::Beats` run on a `SessionState` built from the recorded timeline, unrolled by `n · quantum` per pass. The same functions, so the same signals □. S4 hands that to the live Link generator.

### §6.2 State on seek and loop (D8)

A jump must leave every consumer in the world as it was at the target: knobs where they were, notes held that were held, pose where the performer stood.

- **Snapshot** at a target τ: for every address seen before τ, its last record before τ. Built at open by one scan: every 5 s of take, a map `address id → file offset of its last record` (~14 MB/h for 500 addresses ◇).
- **On a jump**: send each address's snapshot value **where it differs from what consumers last got**, in one router round. Addresses first seen after τ get nothing, except **MIDI notes and pad buttons, which are sent 0**: no hung notes □.
- **Events are never re-fired**: addresses matching `[replay] events`, default `*/press`, `*/tap`, `*/recenter`, `/tussen/clock/onbeat`, `*/onset`: the "event" entries of `osc-signals.md`.
- **Belt and braces** □: MIDI and pad state re-sends every 500 ms anyway, and 60 Hz device signals heal within a frame. Snapshots matter most for on-change sources (`/djay/`, `/ext/`, slow CCs).
- Link state at τ comes from the same scan, so the bar grid is right after a jump.

### §6.3 Rate (D12)

- **1.0 only** at first. OSC at other rates is trivial; audio is not. Varispeed (pitched, the linear resampler in simply-audio-capture) is the cheapest step beyond. Time-stretching is not proposed.
- **Markers**: `kind 6` records. Set from the console during recording or replay; jump-to-marker is a seek.

## §7 Console and CLI (D10)

### §7.1 CLI

```
tussen-hub run --record [--take-name rehearsal]   # record everything until the hub stops
tussen-hub run --replay latest                    # loop the newest take (S1: whole take)
tussen-hub run --replay <name|path> --loop-bars 33..41 [--from-bar 33] [--audio-lead-ms 40]   # S2
tussen-hub run --replay latest --link auto|lead|off   # S4
tussen-hub takes                                   # name, date, duration, size, sources
tussen-hub take dump <take> [--from 1:20] [--grep /tussen/midi/]   # NDJSON
```

- **Flags on `run`, not a new command (D10)**: one process, one config, one console, and `./tussen --replay latest` works unchanged □. `--record` with `--replay` is refused in S1 (§4.4).
- `[replay]` config section (ignored by today's hub □): `dir`, `events`, `flush_ms`.

### §7.2 Console (S3)

- **Record button** (left column, under Link): elapsed time, size, `rec_drop` if any. A red frame around the page while recording.
- **Take list**: name, date, length, sources. Click to open, which replaces the live sources (§4.2) after a confirm, since it is a show-affecting action.
- **Transport** above the timeline: play/pause, position as `bar.beat` and `mm:ss`, loop on/off, `--link` mode.
- **Overview strip** of the whole take from `levels.bin`, with the bar grid; drag a loop region, which snaps to bars; markers as ticks. The existing live timeline and anchored strip stay as they are, fed by the replay like live.
- **Websocket**: new frame `{"t":"replay","state":"playing","take":"…","pos_us":…,"bar":33.2,"loop":[33,41],"len_us":…,"lead_ms":40}` at ~4 Hz, also to read-only clients (a shader page can show where it is). New full-access commands: `record`, `replay_open`, `replay_transport`, `replay_loop`, `mark`. Existing frames unchanged.

## §8 Stages

**S0, no code, today** ◇: a `monitor`-style static destination on a spare port subscribed to everything, `oscdump` (liblo) into a file, `oscsendfile <hub> 9000 <file>` later. No audio, no origins, hellos included (harmful), no loops. It shows the need, and the hub can do better in a day.

| Stage | What | Proves | Acceptance tests |
|---|---|---|---|
| **S1** record + replay, whole take, CLI | recorder thread; router tap in `route` + `command`; sender audio tap (SPSC) → WAV with patched header; Link tap; `--record`; replay generator (OSC thread with precise wakeups, `Origin::Replay`, skip `/tussen/hub/`); audio pacer into `stream_ring`; audio lead; whole-take loop with crossfade; `--replay` swaps capture, MIDI, pad, Link for the take; synthetic Link frames to the console | **a take plays to unchanged consumers as if live** | in-process hub, fake generators + a fake device: (1) take's events equal the routed sequence, stamps within 1 ms; (2) WAV equals the L16 sent, sample-exact, frames = packets × 256; (3) writer dropped without close → take opens, WAV header repaired, events up to the last flush; (4) replay into a hub with a registered listener: same address sequence, inter-arrival within ±2 ms of the take; no device registered from recorded hellos; (5) RTP: audio sample-identical, audio-vs-OSC offset = recorded − lead ± one packet (5.3 ms); one SSRC, `seq` continuous across the loop; (6) `src:"replay"` on the websocket; (7) **no `--record`/`--replay` → no takes dir, all existing tests unchanged** |
| **S2** loop a section, seek, state | scan-built index + snapshots; `--loop-bars`, `--from-bar`; clock synthesized via `link::sample`/`Beats` on the recorded timeline, unrolled; `take dump`; `takes` | **iterate on one part** | loop 2 bars of a take with a held note, a CC ramp and a `/djay/` value: at every seam the listener gets the state at A (note held or off as at A, CC at A's value), no `…/press` re-fired; beat strictly increasing across 10 seams, `beat mod 4` equal to the recorded one; seek into the middle: first round carries the snapshot |
| **S3** console | record button, take list, transport, overview strip, loop drag, markers, `replay` frame and commands | **no terminal needed at the hackathon table** | console-test: open take, set loop by drag (snaps to bars), play, see `replay` frames; read-only client gets frames, commands ignored |
| **S4** Link placement | `--link` modes `auto`, `lead`, `off`, `transport`; founds when waiting; launch-quantised start and seek; tempo proposals; clock from the live Link generator again | **TD's Link CHOP and Ableton follow a replay** | fake Link session (simply-link's reference peer): replay start lands on a bar ±1 ms; whole-bar loop for 10 min: phase error vs session 0 (no re-placement); a peer at a different tempo in `auto` → warning, no proposal; waiting hub → session founded at the take's tempo |
| **S5** claims, live over replay | per-prefix claims, console toggles, `live_masked=` counter; live generators run beside the replay | **a performer plays pads over a recorded set** | claim released on `/tussen/pad/` → live pad reaches td, replayed pad dropped; claimed prefix → live dropped and counted |
| **S6** format and fidelity | `capture_ns` through the ring (shared with mesh S4); interned addresses; zstd chunks; FLAC export; `take trim`; `take dump --format mcap --anonymize` | takes are small and shareable | 3-device take ≤ 0.8 GB/h; trim round-trips; anonymized dump holds no IPs |
| S7 retroactive capture | an always-on ring of the last N minutes, "capture" saves it as a take (Ableton's Capture MIDI, for everything) | the good take is never lost | ring of 10 min at ~2.6 GB/h fits in a fixed disk budget; capture produces a take indistinguishable from `--record` |

- **S1 effort ◇: ~800 lines including tests, 1–2 days for one lane.** Roughly recorder 250, replay 300, CLI and wiring 80, tests 200. It needs no simply-link change and no new dependency beyond what the hub has.
- **S2 is the stage that makes the hackathon case good** (loop bars 33–41), about a day on top ◇. S1 + S2 before a hackathon is the target. S3 is nice to have.
- S4 and S5 are independent of each other. S6 shares `capture_ns` with mesh S4: whichever lands first carries it.

## §9 Risks and unknowns

| Risk | State | How to settle it |
|---|---|---|
| The recorder slows the router at 10 k msg/s | ◇ one `encode` + `try_send` per message; the router already encodes per routed message | S1 benchmark: router `in=` rate with and without `--record`; drops counted, never blocking |
| Replay timing jitter on macOS | ◇ 3 ms mean without the time-constraint policy (`timing.rs`) | precise wakeups on the replay and pacer threads; S1 test (4) bounds it |
| A live device has a recorded device's name | □ ownership is by address: it will not receive the recorded one's signals, and in S1 both publish under one prefix | console warning on collision; S5 claims; the setup guide says to pick another name |
| Audio speakers for the shader dev | □ the hub has no output | ffplay/GStreamer now (§4.1); the mesh output stage later |
| Absolute beat numbers differ from the take under S4 | □ by placement | consumers counting absolute beats (rare) see an offset; bar phase is exact |
| A venue's DJs yanked by a replay's tempo | □ only with `--link lead` | `auto` never proposes when others are in the session |
| Takes leak LAN addresses and device names | □ origins are recorded | `--anonymize` on dump (S6); takes stay local by default |
| WAV 4 GiB limit | □ 6.2 h at L16 stereo 48 kHz | RF64 or split at S6; a warning at 6 h |
| Disk fills mid-show | ◇ | recording stops, the hub does not; console flag |
| A take's config differs from today's (other device names, ports) | □ replay uses today's config by design | `take.toml` + `hub.toml` copy let the console show the diff |

## §10 Simplification ledger

**Additive**: nothing is removed. The ledger comes out ahead because replay **is** existing machinery fed from a file.

| Need | Reused as is | Generalised (small) | New |
|---|---|---|---|
| injecting signals | `Generator`, the router's channel, `route()` | `route(m, origin, now)`: `Origin::{Net(addr), Hub, Replay}` instead of `Option<SocketAddr>` | the replay thread |
| delivering them right | subscriptions, ownership rule, remaps, both sync points, delay lines | | |
| audio out | `stream_ring`, `Packetizer`, `DelayRing`, `Readers`, statics, `sdp` | | the pacer |
| clock on replay | `link::sample`, `Beats`, `next_wake`, `private` | a `SessionState` from recorded timeline points | unrolling across loops |
| Link placement | `found`, `propose_tempo`, the console-start rule | | launch quantisation |
| recording | the console tap's position in `route`; the sender's level tap | a second consumer beside `LevelBatch` | recorder thread, two writers |
| state after a jump | MIDI/pad refresh (heals anyway) | | snapshot index |
| config/paths | `config_file::Env`'s XDG logic | a data-dir twin of `user_path` | `[replay]` |
| console | tap, frames, commands, timeline, anchored strip | | `replay` frame, overview strip, 5 commands |

- **New concepts: three.** A take, a replay origin, a claim (S5). Everything else is an existing concept fed from disk.

## §11 What this gives up

- **A take is not a record of what each consumer got.** Remaps, delays and drops are re-made by today's rig. Forensics needs egress taps, and none are proposed.
- **While replaying, the hub leads Link** (D5 settled): DJs in the same session are moved to the take's tempo. `--replay-link off` is the way out.
- **In S1, the live MIDI, pad and capture are off during replay**; live-over-replay waits for S5.
- **No live audio over a replay**: the take's audio replaces the capture. Mixing needs the mesh proposal's mixer.
- **Absolute beat numbers are not preserved under S4**: only bar phase.
- **Rate 1.0 only**, no scrubbing with sound.
- **~1–2.6 GB per hour** until S6 compresses the log.
- **NDI and TD's picture are not recorded** (the hub never carries NDI): devices following TD (sync point 2) line up with the live TD in the room, not the one at the venue.

## §12 Where this contradicts what exists

There is no landed design in `docs/design/`. The mesh proposal (`mesh-design`, unlanded) and module docs and guides are what this touches.

| Where | It says | Replay does | Reconciled how |
|---|---|---|---|
| `hub/src/link.rs` module doc; `README.md`; `setup.md` step 6 | "The hub never founds a session by itself"; tempo is "never imposed on a session it joins" | S4 founds when waiting, and proposes the take's tempo when alone or with `--link lead` | replaying with `--link` is a human opt-in, like *start session* (the mesh's D2 makes the same move) |
| `docs/guides/signals-websocket.md` | `src` is a source address or `"hub"` | adds `"replay"` | one new value, documented |
| `README.md` "captures and streams the music" | the RTP stream is the capture | in replay mode it is the take | same format, port and sync points |
| `hub/src/generators.rs` | "no device is named `audio`, `midi`, `pad` or `clock`", so the ownership rule never bites a generator | the replay generator publishes `/tussen/<dev>/…`, so ownership **does** apply to its output | intended: a live device never hears its own name's recorded signals back; a warning on collision (§9) |
| **mesh proposal §0, §3.2** | audio timing is the **capture** stamp, carried through the ring | replay records the **packet** stamp | both right for their purpose: the mesh places audio on a remote grid (true capture time); replay reproduces what live consumers got. A take stores both once the ring carries `capture_ns` (S6) |
| **mesh proposal §3.8** | `route` generalises to `due = origin + delay` | replay needs no such change; it injects on time | if mesh lands first, a replay could hand records ahead with an origin time instead of sleeping per message |

Aligned with the mesh where it cost nothing: the audio anchor is the mesh header's `(frame, beat, tempo)`; the correction `beat(t − signal_delay)` is the same (mesh §3.2, here §2.3); `capture_ns` through the ring is one shared change. A take could later be offered to a mesh as a site, and a mesh session recorded: remote OSC arrives at `route` as `/mesh/<site>/…` and is taken like any other. Neither depends on the other.

## Prior art, in one breath

liblo's `oscdump`/`oscsendfile` already record and resend OSC at a speed multiplier; Protokol logs with timestamps; TouchDesigner's Record CHOP records what one CHOP sees; ROS bags and MCAP are the robotics answer to "record everything with timestamps" and both treat the index as rebuildable; Ableton's Capture MIDI is always listening. Sources: [replay-numbers.md § Prior art](../research/replay-numbers.md#7-prior-art).

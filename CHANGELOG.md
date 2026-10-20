# Changelog

## Unreleased

#### Destinations say which modes they're active in · 2026-10-20 02:40 · molpnvrq

- **`modes = ["run", "replay"]` on any `[[dest]]`.** Without it, a
  network destination is active in every mode (as before) and an
  `audio_out` one in **`replay` only**: live, the room already hears the
  music, and playing it again doubles it and can feed back.
  `modes = ["run"]` monitors live on headphones. `"mesh"` is accepted and
  reserved (no mesh mode yet, so it never activates); any other value is a
  config error naming the destination.
- An inactive destination is not created: no socket, no RTP, no device
  opened, absent from the console. The startup lists it as
  `dest speakers → audio out "" — replay only, inactive in run`.

#### Local audio outputs: hear the hub's stream on this machine · 2026-10-20 02:25 · rzozrrmk

- **A `[[dest]]` with `audio_out = "<device>"`** (`""` = the default output;
  a name from `simply-tussen list`, or a unique part of one) plays the
  hub's audio stream on a local device: hear a replay in the room, or
  monitor the live capture on headphones during `run`. It needs no OSC
  fields. The startup lists it as
  `dest speakers → audio out "<device>" (<rate> Hz), kind engine`. A
  device that won't open is a warning, not a stop. During `run` the hub
  refuses the output `[audio]` taps, which would capture itself.
- It is an ordinary RTP consumer: an in-process receiver on loopback, fed
  the same stream through the same delay ring as the devices, so replay
  loops and seams sound as on RTP. Its `kind` sets the delay by the usual
  rule. `engine` (the default) is undelayed, the room; `device` is held back
  by the TD round trip.
- Inside: packets are put back in order (a lost one plays as silence),
  playback starts once 20 ms is queued, it skips back to 20 ms if device
  clock drift piles up more than 80 ms, it resamples linearly to the device
  rate, and it maps stereo to the device's channels. Device selection uses
  audiowatch's `simply-audio-device`; the stream is cpal 0.18.

#### Record a rehearsal, replay it as if live · 2026-10-20 01:09 · zwoqsnyk (first: 2026-10-20 00:59 · zxuvnzuw)

- **● Record in the console's top bar** starts a take at any time; ■ Stop
  ends it (elapsed time and take name show while on). A take goes into
  `~/Music/tussen/takes/<UTC time>[-name]/` (`$XDG_MUSIC_DIR`,
  `--takes-dir`): every signal the hub routed (OSC, MIDI, pad, with time and
  source), the audio it streamed as a WAV, the Link timeline and the sync
  state, plus a copy of the config. It starts with the last value of every
  address at the press. Valid after every flush, so a killed hub loses at
  most a quarter of a second. `run --record [NAME]` starts one at launch.
- **`simply-tussen replay <take|latest>`** starts the same hub as `run`
  (`./simply-tussen replay` opens the console too), fed from the take,
  looping: same addresses, same RTP stream (one SSRC, continuous
  sequence), same `/tussen/clock/…`. Consumers change nothing; the console
  marks replayed signals `src: "replay"`. The hub's own MIDI, pad, capture
  and Link are off while replaying; network OSC still merges; recorded
  hellos are never replayed.
- **The replay leads Ableton Link**: it founds a session at the take's
  tempo if there is none, else proposes the take's tempo and starts on that
  session's bar grid; recorded tempo changes follow. TouchDesigner's Link
  CHOP and Ableton follow a replay. `--replay-link off` leaves Link alone.
- **Bar loops**: `--loop 9..17` loops bars 9–17 of the recorded timeline,
  `--from 13` starts at bar 13. Each seam restores the last value of every
  address, lets go of notes not held at the loop start, never re-fires
  events, and keeps the beat counting up in phase.
- **Both sync points apply during a replay, as live**, so a replay can
  drive TouchDesigner and the devices to test the whole visual path (TD's
  NDI to the devices stays live; NDI is never recorded). A replay is the
  hub's ingress reproduced: audio at its recorded time relative to the
  signals, no lead. `--offset-ms` nudges it by hand.
- `simply-tussen takes` lists takes; the console's top bar shows
  `REPLAY <take> · bar x/y · loop a–b`; the `link` frame gains `replay`,
  `hello` gains `record`, and the page sends `record_start` / `record_stop`.
- User guide: [docs/guides/record-and-replay.md](docs/guides/record-and-replay.md).

#### The binary is `simply-tussen` · 2026-10-20 00:29 · rpklqlnu

- **Renamed: `tussen-hub` → `simply-tussen`** — the command, the installed
  binary (`./simply-tussen install`), and the MIDI client name. Same commands and
  flags. If you installed `tussen-hub` before, reinstall and delete the old
  one from `~/.local/bin`.

#### `./simply-tussen run`, `simply-tussen` anywhere, and a quiet hub · 2026-10-20 00:18 · ntnkpssp

- **`./simply-tussen` is `simply-tussen`, built fresh from the checkout**: every command
  and flag passes through. `./simply-tussen run` starts the hub and opens the
  console; `./simply-tussen install` puts `simply-tussen` in `~/.local/bin`, and
  `simply-tussen run` then works from any directory (no browser).
- **Outside a checkout with no config of your own, the hub runs the shipped
  example built into the binary.** `simply-tussen config path` says so; console
  Save creates `~/.config/tussen/hub.toml` from it.
- **On start the hub prints where to reach it**: console URL, OSC endpoint
  (LAN and local), config. After that it logs only what changes or goes
  wrong, so the URLs stay on screen.
- **The per-second stats line is off by default**; `run --stats` brings it
  back. Malformed packets, failed sends and dropped audio/delay/console
  data get a warning instead, at most once a minute. Link tempo following
  and console tempo changes moved to debug.

#### Your hub config lives in ~/.config, not in the repo · 2026-10-20 00:02 · quplzywy

- **The hub reads `~/.config/tussen/hub.toml`** (`$XDG_CONFIG_HOME/tussen/hub.toml`
  if set; macOS too) before the repo's `hub.toml`, which is now the shipped
  example and the fallback. Order: `--config`, your config, the repo's.
  The startup banner says which (`config → <path> (user)`).
- **Console Save never writes into the repo.** Running on the repo's example,
  Save copies it (comments and all) to your config path, applies the edit
  there and uses that file from then on; the console's "saved →" shows it.
- `simply-tussen config path` prints the file the hub would use and why;
  `simply-tussen config init` copies the repo's `hub.toml` to your config path
  (never overwrites). `./simply-tussen` reads its ports from the same file.

## 1.0.0 — 2026-10-19

#### Ableton Link: the hub waits for a session instead of founding one · 2026-10-19 20:09 · qvpsqvxx

- **The hub no longer starts a Link session by itself.** Before, it founded
  one at `[link] tempo` after a second alone, so a DJ who switched Link on
  later joined the hub's (older) session and had their track slowed to the
  hub's 120. Now the hub starts *waiting*: it listens, announces nothing,
  and joins the first session it hears **at that session's tempo** — the
  DJ's tempo and beat never move.
- While it waits the hub keeps a beat of its own at `[link] tempo`, so
  `/tussen/clock/…` never stops; the console's tempo and start/stop drive
  it. Joining a session moves the beat onto the session's.
- **Start session** in the console (next to the Link field, shown while
  waiting) founds a session now at the current tempo, carrying the beat on
  — for when nobody else will join.
- The rule for going back: when everyone else has left a session the hub
  *joined*, it waits again. A session started from the console stays until
  the hub stops.
- The console's Link field says `Link: waiting` or the peer count; its
  dropdown says which of the three the hub is in. The `link` frame gains
  `mode`. Needs `simply-link` with `found`/`leave` (music-rs, unreleased).

#### Console: click the Link peer count to see who is connected · 2026-10-19 20:00 · prsmntlt

- **The "Link N peers" field at the top of the console now opens a list**
  of every Link peer the hub hears: where it runs (its hostname when the
  network can name it, its address, or "this machine"), whether it is in
  the hub's session or another one, its tempo and transport, and how long
  ago it was last heard. Click the count again, click elsewhere or press
  Escape to close it. With nobody around it says "no Link peers".
- Link itself does not say which app a peer is, so the list can show
  where each one runs but not its name; two apps on one laptop are two
  rows with the same host, told apart by their Link node id.
- Hostnames come from reverse DNS, looked up in the background and
  cached, so a slow lookup never holds up the beat. Needs `simply-link`
  with `Link::peers` (music-rs, unreleased).

#### Ableton Link: late joiners connect, and the hub follows their tempo · 2026-10-19 19:45 · wzknykvm

- **A Link app switched on after the hub now joins it.** Before, a DJ app
  (djay Pro) that turned Link on while the hub was already running could
  stay in its own session forever: the hub's timing answers left out the
  session they belonged to, and Ableton's Link drops such answers, so it
  could never join the hub's session; and the hub only gave way to a
  session whose id happened to sort first. Now the hub answers the way
  every Link app expects, and decides which session wins by Link's own
  rule — the older one — so both sides always agree.
- **What that means for tempo:** a DJ who had Link on before the hub keeps
  their tempo and the hub joins them. A DJ who switches Link on after the
  hub joins the hub's session at its tempo, as when joining any running
  Link session. Either way, every tempo change after that reaches the hub.
- The log now names the session the hub is in and each tempo it follows
  (`link: following 124.00 BPM`).
- Needs `simply-link` with the fix (music-rs, unreleased).

## 0.1.0 — initial public release

The hub as it runs today: subscription OSC routing with remaps, MIDI and
game-controller signals, Ableton Link (founds, then follows), CoreAudio
capture streamed as RTP, the two sync delays, and the live console with its
read-only websocket feed.

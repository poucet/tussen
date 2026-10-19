# Replay: the numbers

The arithmetic and measurements behind [docs/design/replay.md](../design/replay.md):
how big a take gets, what compresses, and the prior art.
Claims carry □ (follows from the code or from arithmetic), ◇ (measured or cited,
method named, could be revised), 🚫 (impossible here; the lever named).

## 1. Message rates

What the hub's ingress sees, from the code:

| Source | Rate | Where from |
|---|---|---|
| one device (`fake-device`, the reference schema) | **40 messages per frame × 60 Hz = 2 400 msg/s** □ | `hub/src/fake.rs`: 18 head + 21 controller signals + `seq` per publish |
| hub clock | 4 × 60 Hz + `onbeat` ≈ **242 msg/s** □ | `hub/src/link.rs`: `bpm beat phase running` at `PERIOD_US` = 16 667 µs |
| MIDI | on change, plus every CC and held note every 500 ms □ | `hub/src/midi.rs`, `refresh_ms` |
| hub pad | on change, plus the full state every 500 ms □ | `hub/src/pad.rs`, `refresh_ms` |
| README's stats example (`--fake-midi --fake-pad`) | `in=420/s` ◇ | `README.md` |

A three-device show (phone, quest, editor) with clock, MIDI and a pad is
therefore about **7 200–10 000 msg/s** □.

## 2. Event log size

Encoded size of one reference-schema message: address padded to 4 bytes +
`,f\0\0` + a 4-byte float. Over the 40 addresses of `fake.rs` under
`/tussen/quest/`, the mean is **38.2 B** □ (computed by a script from the
`put("…")` calls in `fake.rs`; padding rule from the OSC 1.0 spec).

A record in the proposed log adds 15 B: `u32` length, `u8` kind, `u64` µs,
`u16` origin. **53.2 B per message** □.

| Rate | Raw log | Per hour |
|---|---|---|
| 420 msg/s (README example) | 22 kB/s | **80 MB** □ |
| 2 400 msg/s (one device) | 128 kB/s | **460 MB** □ |
| 7 200 msg/s (three devices) | 383 kB/s | **1.38 GB** □ |
| 10 000 msg/s (three devices + clock + MIDI + pad) | 532 kB/s | **1.92 GB** □ |

**The event log of a busy show is bigger than its audio** □. That is the
surprise in these numbers, and why §3 below matters.

### Compression, measured

Method: a Python script wrote 20 s of a three-device show in the record
layout above: the 40 `fake.rs` addresses per device at 60 Hz, each value a
slow sine plus Gaussian noise (σ = 0.002) so float mantissas are not
trivially repetitive, timestamps jittered by up to 3 ms. 144 000 records,
7 704 000 B. Then:

| Codec | Ratio | Per hour at 7 200 msg/s |
|---|---|---|
| none | 1.0 | 1 387 MB |
| zlib level 1 (Python `zlib`) | **4.3×** | 326 MB |
| zlib level 6 | 5.1× | 272 MB |
| `zstd -1` (CLI, Homebrew) | **6.8×** | ~200 MB |
| `zstd -9` | 8.3× | ~170 MB |

zlib level 1 took 27 ms for the 20 s sample (Python, one core): **about
0.1 % of one core** at that rate ◇.

- Synthetic data ◇: real sensor noise has more entropy in the mantissa. The
  floor is the floats themselves: 4 B × 7 200/s = **104 MB/h** that no codec
  removes if every value is noisy □.
- **Address interning** gets most of the way without a codec: a `u16` address
  id instead of a ~28-byte padded address cuts a record from ~53 B to ~23 B
  (type tags and args kept) □, **~600 MB/h** at 7 200 msg/s.

## 3. Audio size

| Format | Rate | Per hour |
|---|---|---|
| L16 stereo 48 kHz (what RTP carries) | 192 000 B/s | **691 MB** □ |
| f32 stereo 48 kHz | 384 000 B/s | 1.38 GB □ |
| FLAC, typical music | ~50–60 % of L16 ◇ (general experience, not measured here) | ~350–420 MB |

- A RIFF WAV's sizes are 32-bit, so a plain WAV holds at most 4 GiB:
  **6.2 hours** of L16 stereo at 48 kHz □. RF64 or a file split lifts it.
- L16 is what the hub already sends: recording L16 loses nothing a consumer
  ever received □ (`rtp.rs` quantises with `clamp` and `× 32767`).
- Level summaries for an overview waveform: `[min,max,rms]` f32 per 256-frame
  packet = 12 B × 187.5/s = **8.1 MB/h** □.

## 4. A take, totalled

| Show | Events (raw) | Audio (WAV) | Total per hour |
|---|---|---|---|
| hackathon rehearsal: one device + MIDI + pad + clock | ~0.5 GB | 0.69 GB | **~1.2 GB** □ |
| three devices + everything | ~1.9 GB | 0.69 GB | **~2.6 GB** □ |
| three devices, events zstd-compressed | ~0.2–0.3 GB ◇ | 0.69 GB | **~1 GB** ◇ |

A laptop SSD takes a full day of shows without compression; compression is
a nicety, not a stage-1 requirement □.

## 5. Crash safety, from the code

- **Ctrl-C never runs a destructor while the hub taps an output.**
  `generators/audio/mod.rs`, `hold()`: on SIGINT/SIGTERM it drops the tap and
  calls `std::process::exit(128 + sig)` □. A take must therefore be valid
  without any finalisation: no footer, no index written at close.
- **A process crash loses only what is still in userspace buffers** □: the
  kernel page cache survives the process. Flushing the writers every ~250 ms
  bounds the loss to that.
- **Power loss** needs `fsync`, and on macOS `fsync` does not force the drive's
  cache: `F_FULLFSYNC` does ◇ (Apple's `fsync(2)` man page). Once a second,
  for a 0.7–2.6 GB/h stream, is cheap.
- **WAV headers are written last** by most writers. A take patches its
  header's two size fields on every flush and **repairs them on open** from
  the file length □, so a killed take is a valid WAV but for the last flush.
- **A torn last record** in the event log is detected by its length running
  past the end of the file □; the reader stops there.

## 6. Clocks, from the code

- The router stamps every message with one `Instant` per receive round
  (`hub.rs`, `run`: `let now = Instant::now()` after the `select!`) □. Delays
  are computed from that same `now` (`sync::due(now, delay_ms)`) □.
- The audio sender stamps each RTP packet `Instant::now()` when it packetizes
  it (`ring.next_slot(Instant::now())`) □, and the console's waveform levels
  are stamped the same way (`LevelBatch::push`) □.
- simply-link's `HostClock` is an `Instant` plus an epoch
  (`simply-link/src/clock.rs`) □.
- Rust's `Instant` on macOS is `CLOCK_UPTIME_RAW`, "identical to the result
  of `mach_absolute_time()`" (Rust std `sys/time/unix.rs`, toolchain 1.95) □.
- The capture callback's `capture_ns` is `mach_absolute_time` in ns
  (simply-audio-capture's sink doc, `tap.rs` `mHostTime`) □, and the hub
  drops it today (`ring_sink(…, _capture_ns)`) □.

**Everything the hub stamps is one clock** □. The capture timestamp is the
same tick source, reachable with one offset read ◇.

**Macro timer precision**: an ordinary macOS thread oversleeps a 0.5–20 ms
wait by **3.0 ms mean, up to 10 ms**; with the time-constraint policy, 0.02 ms
mean, 0.08 ms max (`hub/src/timing.rs`, measured on the development Mac) ◇.
A replay thread must ask for the same policy.

## 7. Prior art

| What | What it does | What we take |
|---|---|---|
| **liblo `oscdump` / `oscsendfile`** | `oscdump` prints received messages; `oscsendfile hostname port file <speed>` resends a file "with a speed multiplier" ◇ ([oscsendfile.c](https://github.com/radarsat1/liblo/blob/master/src/tools/oscsendfile.c), [oscdump.c](https://github.com/radarsat1/liblo/blob/master/src/tools/oscdump.c)) | a zero-code stage 0 exists (design §8); record/replay at a port is old and simple |
| **Protokol** (hexler) | OSC and MIDI monitor and logger, "plus an optional timestamp" ◇ ([hexler.net/protokol](https://hexler.net/protokol)) | logging, not replaying; we found no replay on its page |
| **TouchDesigner Record CHOP** | "takes the channels coming in … records them internally, and outputs the stored channels" ◇ ([docs](https://docs.derivative.ca/Record_CHOP)) | per-patch recording exists in TD; it records what one CHOP sees, not the network, and no audio stream alongside |
| **ROS bag 2.0** | chunks of messages, per-connection *index data* after each chunk, *chunk info* records ◇ ([format](http://wiki.ros.org/Bags/Format/2.0)); `rosbag reindex` repairs "a bag not closed cleanly" ◇ ([CLI](http://wiki.ros.org/rosbag/Commandline)) | an index is derived data: rebuild it rather than trust it |
| **MCAP** | `<Magic><Header><Data>[<Summary>][<Summary Offset>]<Footer><Magic>`; summary and index optional ◇ ([spec](https://mcap.dev/spec)); `mcap recover` reads a corrupt file ◇ ([CLI](https://mcap.dev/guides/cli)) | the considered alternative to our own log (design D3); its Foxglove tooling plots channels over time |
| **Ableton Live Capture MIDI** | "Live is always listening to MIDI input … Capture MIDI lets you retrieve the material you've just played" ◇ ([manual §19.10](https://www.ableton.com/en/manual/recording-new-clips/)) | retroactive capture: the stage after record works (design S7) |
| **mesh proposal** (`mesh-design`, unlanded) | beat-stamped audio packets `(frame, beat, tempo)`, `beat = beat(t_cap − signal_delay)` | the same anchor triple and the same correction (design §2) |

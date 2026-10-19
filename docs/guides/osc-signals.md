# OSC signals

Every OSC address in the tussen signal network. The hub itself publishes
`/tussen/midi/…`, `/tussen/clock/…` and `/tussen/pad/…`; everything under
`/tussen/<dev>/…` comes from a **device** — any OSC client that registers with
the hub (`/tussen/hub/hello`) and publishes under its own name. The reference
device app uses `editor` (a laptop), `phone` and `quest`; any client speaking
the same schema joins the same way. `n/…` values are 0..1, normalized by the
ranges noted beside each (a device may configure its own).
This file is the routing contract: keep it in sync when adding a signal.

**Head / phone pose: Mac (simulated), phone, Quest**
* `/tussen/<dev>/head/pos/x,y,z` - stage position (relative to the last recenter), meters
* `/tussen/<dev>/head/quat/x,y,z,w` - stage orientation (the direction), Unity convention
* `/tussen/<dev>/head/yaw` - turn left/right, degrees (right = positive)
* `/tussen/<dev>/head/pitch` - look up/down, degrees (up = positive)
* `/tussen/<dev>/head/roll` - tilt, degrees (right = positive)
* `/tussen/<dev>/head/n/yaw` - yaw, 0..1 (−90°..+90°)
* `/tussen/<dev>/head/n/pitch` - pitch, 0..1 (−45°..+45°)
* `/tussen/<dev>/head/n/roll` - roll, 0..1 (−30°..+30°)
* `/tussen/<dev>/head/n/height` - height change since recenter, 0..1 (−0.4..+0.4 m)
* `/tussen/<dev>/head/n/x,z` - stage position, 0..1 (−1.5..+1.5 m)
* `/tussen/<dev>/head/n/speed` - movement speed, 0..1 (0..1.5 m/s)
* `/tussen/<dev>/head/n/turn` - turning speed, 0..1 (0..180 °/s)
* `/tussen/<dev>/recenter` - event: stage frame was reset
* `/tussen/<dev>/seq` - counter, +1 per publish (int)

**Gamepad: any pad, e.g. the PS5 DualSense — two wirings, same schema**

A controller plugged into the **hub laptop** is read by `simply-tussen` itself
(`[pad]` in hub.toml) and publishes under **`/tussen/pad/…`** — no device
app involved. A controller plugged into a **performance device** is read by
that device's app and publishes under
**`/tussen/<dev>/pad/…`**. Everything below exists under both prefixes; the
hub also resends its full pad state every 500 ms (self-heal), and sends
`active 0` at that cadence while no controller is connected.

* `/tussen/<dev>/pad/active` - 1 while a pad is connected
* `/tussen/<dev>/pad/lx,ly` - left stick, −1..1 (+x right, +y up)
* `/tussen/<dev>/pad/rx,ry` - right stick, −1..1
* `/tussen/<dev>/pad/n/lx,ly,rx,ry` - sticks, 0..1 (centre 0.5)
* `/tussen/<dev>/pad/l2,r2` - triggers, 0..1
* `/tussen/<dev>/pad/south,east,west,north` - face buttons (DualSense ✕ ○ □ △), 0/1
* `/tussen/<dev>/pad/l1,r1` - shoulder buttons, 0/1
* `/tussen/<dev>/pad/l3,r3` - stick clicks, 0/1
* `/tussen/<dev>/pad/up,down,left,right` - d-pad, 0/1
* `/tussen/<dev>/pad/start,select` - options / create, 0/1
* `/tussen/<dev>/pad/<button>/press` - event on press (every button, plus l2/press, r2/press)

**Phone extras**
* `/tussen/phone/touch/count` - active touches, 0..5 (int)
* `/tussen/phone/touch/0/x,y · touch/1/x,y` - first two touches, 0..1, screen-normalized, origin bottom-left (landscape)
* `/tussen/phone/touch/tap` - event: single tap
* `/tussen/phone/motion/n/shake` - shake strength, 0..1 (0..15 m/s²)
* `/tussen/phone/motion/n/spin` - rotation rate, 0..1 (0..360 °/s)
* A phone in controller mode sends exactly these plus its head signals, and renders nothing — a pure 3D controller

**MIDI controllers (from the hub)**
* `/tussen/midi/cc/<n>` - control change n, 0..1 (value/127)
* `/tussen/midi/note/<n>` - note n velocity, 0..1 (0 = off)
* `/tussen/midi/note/<n>/pressure` - poly aftertouch on held note n, 0..1 (0 on release)
* `/tussen/midi/bend` - pitch bend, −1..1
* `/tussen/midi/pressure` - channel pressure, 0..1
* `/tussen/midi/program` - program change (int)

**Beat: Ableton Link, from the hub**
* `/tussen/clock/bpm` - tempo, beats per minute
* `/tussen/clock/beat` - Link session beat (same bars as every Link peer); while the hub waits for a session, its own beat at `[link] tempo`
* `/tussen/clock/phase` - position within the beat, 0..1
* `/tussen/clock/running` - 1 while the session is playing
* `/tussen/clock/onbeat` - event on each beat (beat index, int)

**Other sources**
* External tools send to the hub on :9000 under their own top-level prefix, routed as is. A tool with a fixed address scheme uses it unchanged — djay Pro's `/djay/turntable1/fx/1/type` fans out to every subscriber with no renaming. The default subscriptions carry `/tussen/`, `/ext/` and `/djay/`; another fixed-scheme tool means adding its prefix to the subscribe lists (hub.toml; a device's own subscription).
* `/ext/<source>/...` - the convention for tools whose addresses you choose yourself (tablet apps, scripts)
* `/cmd/<dev>/preset, master, param/<name>, recenter, hud, haptic` - commands understood by the reference device app; the hub routes `/cmd/…` like any other OSC

**Device-local only (a device computes these from the hub's audio stream for its own use; never sent over the network)**
* `/tussen/audio/rms` - loudness, 0..1
* `/tussen/audio/band/0..3` - four frequency-band envelopes, 0..1
* `/tussen/audio/onset` - event on each detected hit

**The tracked controller (published by devices that have one; the reference app's laptop target drives it from the gamepad)**
* `/tussen/<dev>/controller/active` - 1 when connected and tracked
* `/tussen/<dev>/controller/pos/x,y,z` - stage position, meters
* `/tussen/<dev>/controller/quat/x,y,z,w` - stage orientation, Unity convention
* `/tussen/<dev>/controller/trigger` - trigger, 0..1
* `/tussen/<dev>/controller/grip` - grip, 0..1
* `/tussen/<dev>/controller/stick/x,y` - thumbstick, −1..1
* `/tussen/<dev>/controller/stick/click` - 0/1
* `/tussen/<dev>/controller/a,b` - buttons, 0/1
* `/tussen/<dev>/controller/n/side` - controller left/right of the body, 0..1 (−0.8..+0.8 m)
* `/tussen/<dev>/controller/n/height` - controller height relative to the head, 0..1 (−0.9..+0.3 m)
* `/tussen/<dev>/controller/n/reach` - controller distance from the body, 0..1 (0.1..0.8 m)
* `/tussen/<dev>/controller/n/roll` - wrist roll, 0..1 (−90°..+90°)
* `/tussen/<dev>/controller/n/pitch` - pointing down/up, 0..1 (−90°..+90°)
* `/tussen/<dev>/controller/n/speed` - controller speed, 0..1 (0..3 m/s)
* `/tussen/<dev>/controller/trigger/press, grip/press, stick/press` - events on press
* `/tussen/<dev>/controller/a/press, b/press` - events on press

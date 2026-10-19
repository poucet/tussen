# Changelog

## Unreleased

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

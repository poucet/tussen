# NDI: TouchDesigner → hub → WebGL

TD on the TD laptop sends NDI; the hub on your laptop receives it and serves
each frame to local pages over a WebSocket, where `ndi.js` makes it a WebGL
texture to composite in your shaders.

## Setup

1. **Both machines**: install NDI Tools (<https://ndi.video/tools/>). The hub
   needs its runtime, `libndi.dylib` (looked for in `/usr/local/lib`, the NDI
   SDK's folder, `$NDI_RUNTIME_DIR_V6`; or set `[ndi] lib`).
2. **TD**: add an **NDI Out TOP** on the final TOP. Give it a name.
3. **Hub laptop**: `./simply-tussen ndi` lists the sources it sees. If it is
   empty, the network blocks mDNS: set `extra_ips = "<TD laptop IP>"`.
4. `hub.toml`:

   ```toml
   [ndi]
   enabled = true
   source = "TouchDesigner"   # substring of the name `simply-tussen ndi` printed
   ```

5. `./simply-tussen run`. The log says `ndi: receiving …` and then
   `ndi: first frame 1920x1080 UYVY`.

## In the page

```html
<script src="http://127.0.0.1:9098/ndi.js"></script>
<script>
  const gl = canvas.getContext('webgl2');      // or 'webgl'
  const ndi = TussenNDI.connect(gl);            // reconnects on its own
  function frame() {
    ndi.update();                               // uploads the newest frame, if one arrived
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, ndi.texture); // RGBA, ndi.width × ndi.height
    // ... your shader ...
    requestAnimationFrame(frame);
  }
  frame();
</script>
```

The texture is laid out like a `<video>` upload (top row at v = 0).
`ndi.hubUs` is the frame's arrival on the hub clock, the same clock as the
signals websocket's `ts`. Any loopback page may connect (any port); nothing
off the machine can.

## The wire

`ws://127.0.0.1:<console port>/ndi`, one binary message per frame, newest
only (a slow page skips frames, it never queues them):

```text
"TNDI" · u32 fourcc · u32 width · u32 height · u64 hub_us   (24 bytes, little-endian)
pixels, tight rows: UYVY = width*2 bytes/row (opaque sources), RGBA = width*4 (sources with alpha)
```

## Numbers

Uncompressed on loopback: 1080p UYVY is 4.1 MB a frame, ~250 MB/s at 60 fps;
with alpha (RGBA) twice that. Fine on loopback for one page; if it stutters,
lower TD's NDI Out resolution or set `bandwidth = "lowest"` (NDI's proxy).

## Not yet

- NDI is not recorded into takes.
- The NDI frame is not delayed by the signal sync point.

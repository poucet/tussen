// tussen NDI → WebGL. Served by the hub at http://127.0.0.1:<console>/ndi.js
//
//   <script src="http://127.0.0.1:9098/ndi.js"></script>
//   const ndi = TussenNDI.connect(gl);            // WebGL1 or WebGL2 context
//   function frame() {
//     ndi.update();                                // upload the newest frame, if any
//     gl.bindTexture(gl.TEXTURE_2D, ndi.texture);  // RGBA, ndi.width × ndi.height
//     ...draw...
//     requestAnimationFrame(frame);
//   }
//
// The texture is laid out like a <video> upload: the image's top row at
// v = 0. ndi.hubUs is the hub-clock stamp (µs) of the frame on screen — the
// same clock as the signals websocket's `ts`. ndi.frames counts uploads.
// Restores the state it touches (framebuffer, program, texture unit 0,
// viewport, array buffer, its vertex attribute, unpack settings); disables nothing else, so it assumes blending/depth/scissor
// are off or harmless while it draws one quad into its own framebuffer.
(function (root) {
  'use strict';
  const VS = 'attribute vec2 p; varying vec2 uv; void main(){ uv = p * 0.5 + 0.5; gl_Position = vec4(p, 0.0, 1.0); }';
  // mode 0: UYVY packed as RGBA texels (U Y0 V Y1), 1: RGBA, 2: BGRA, 3: RGBX, 4: BGRX
  const FS = `precision highp float;
varying vec2 uv; uniform sampler2D src; uniform int mode; uniform float width;
void main(){
  if (mode == 0) {
    vec4 t = texture2D(src, uv);
    float x = floor(uv.x * width);
    float y = mod(x, 2.0) < 1.0 ? t.g : t.a;
    vec3 yuv = vec3((y - 16.0/255.0) * (255.0/219.0), (t.r - 0.5) * (255.0/224.0), (t.b - 0.5) * (255.0/224.0));
    vec3 rgb = vec3(yuv.x + 1.5748*yuv.z, yuv.x - 0.1873*yuv.y - 0.4681*yuv.z, yuv.x + 1.8556*yuv.y); // BT.709
    gl_FragColor = vec4(clamp(rgb, 0.0, 1.0), 1.0);
  } else {
    vec4 t = texture2D(src, uv);
    if (mode == 2 || mode == 4) t = t.bgra;
    if (mode >= 3) t.a = 1.0;
    gl_FragColor = t;
  }
}`;
  const MODES = { UYVY: 0, RGBA: 1, BGRA: 2, RGBX: 3, BGRX: 4 };

  function shader(gl, type, src) {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error('ndi.js: ' + gl.getShaderInfoLog(s));
    return s;
  }
  function tex(gl, filter) {
    const t = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, t);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    return t;
  }

  function connect(gl, opts) {
    opts = opts || {};
    const port = opts.port || 9098;
    const url = opts.url || `ws://127.0.0.1:${port}/ndi`;
    const prog = gl.createProgram();
    gl.attachShader(prog, shader(gl, gl.VERTEX_SHADER, VS));
    gl.attachShader(prog, shader(gl, gl.FRAGMENT_SHADER, FS));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error('ndi.js: ' + gl.getProgramInfoLog(prog));
    const quad = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, quad);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const src = tex(gl, gl.NEAREST);
    const out = { texture: tex(gl, gl.LINEAR), width: 0, height: 0, hubUs: 0, frames: 0, connected: false, update, close };
    gl.bindTexture(gl.TEXTURE_2D, null);
    const fb = gl.createFramebuffer();
    let pending = null, ws = null, closed = false;

    function open() {
      ws = new WebSocket(url);
      ws.binaryType = 'arraybuffer';
      ws.onopen = () => { out.connected = true; };
      ws.onmessage = (e) => { pending = e.data; };
      ws.onclose = () => { out.connected = false; if (!closed) setTimeout(open, 1000); };
    }
    open();

    function update() {
      if (!pending) return false;
      const buf = pending; pending = null;
      const h = new DataView(buf, 0, 24);
      const fourcc = String.fromCharCode(h.getUint8(4), h.getUint8(5), h.getUint8(6), h.getUint8(7));
      const w = h.getUint32(8, true), ht = h.getUint32(12, true);
      const mode = MODES[fourcc];
      if (mode === undefined || !w || !ht) return false;
      const px = new Uint8Array(buf, 24);
      const loc = gl.getAttribLocation(prog, 'p');
      const prev = {
        fb: gl.getParameter(gl.FRAMEBUFFER_BINDING), prog: gl.getParameter(gl.CURRENT_PROGRAM),
        vp: gl.getParameter(gl.VIEWPORT), ab: gl.getParameter(gl.ARRAY_BUFFER_BINDING),
        unit: gl.getParameter(gl.ACTIVE_TEXTURE),
        align: gl.getParameter(gl.UNPACK_ALIGNMENT), flip: gl.getParameter(gl.UNPACK_FLIP_Y_WEBGL),
        attr: {
          on: gl.getVertexAttrib(loc, gl.VERTEX_ATTRIB_ARRAY_ENABLED),
          buf: gl.getVertexAttrib(loc, gl.VERTEX_ATTRIB_ARRAY_BUFFER_BINDING),
          size: gl.getVertexAttrib(loc, gl.VERTEX_ATTRIB_ARRAY_SIZE),
          type: gl.getVertexAttrib(loc, gl.VERTEX_ATTRIB_ARRAY_TYPE),
          norm: gl.getVertexAttrib(loc, gl.VERTEX_ATTRIB_ARRAY_NORMALIZED),
          stride: gl.getVertexAttrib(loc, gl.VERTEX_ATTRIB_ARRAY_STRIDE),
          off: gl.getVertexAttribOffset(loc, gl.VERTEX_ATTRIB_ARRAY_POINTER),
        },
      };
      gl.activeTexture(gl.TEXTURE0);
      prev.tex = gl.getParameter(gl.TEXTURE_BINDING_2D);
      gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
      gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
      gl.bindTexture(gl.TEXTURE_2D, src);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, mode === 0 ? w / 2 : w, ht, 0, gl.RGBA, gl.UNSIGNED_BYTE, px);
      if (w !== out.width || ht !== out.height) {
        gl.bindTexture(gl.TEXTURE_2D, out.texture);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, ht, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
        gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
        gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, out.texture, 0);
        out.width = w; out.height = ht;
      }
      gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
      gl.viewport(0, 0, w, ht);
      gl.useProgram(prog);
      gl.bindTexture(gl.TEXTURE_2D, src);
      gl.uniform1i(gl.getUniformLocation(prog, 'src'), 0);
      gl.uniform1i(gl.getUniformLocation(prog, 'mode'), mode);
      gl.uniform1f(gl.getUniformLocation(prog, 'width'), w);
      gl.bindBuffer(gl.ARRAY_BUFFER, quad);
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      // restore
      gl.bindFramebuffer(gl.FRAMEBUFFER, prev.fb);
      gl.useProgram(prev.prog);
      const a = prev.attr;
      if (a.buf) {
        gl.bindBuffer(gl.ARRAY_BUFFER, a.buf);
        gl.vertexAttribPointer(loc, a.size, a.type, a.norm, a.stride, a.off);
      }
      if (!a.on) gl.disableVertexAttribArray(loc);
      gl.bindBuffer(gl.ARRAY_BUFFER, prev.ab);
      gl.bindTexture(gl.TEXTURE_2D, prev.tex);
      gl.activeTexture(prev.unit);
      gl.viewport(prev.vp[0], prev.vp[1], prev.vp[2], prev.vp[3]);
      gl.pixelStorei(gl.UNPACK_ALIGNMENT, prev.align);
      gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, prev.flip);
      out.hubUs = Number(new DataView(buf, 16, 8).getBigUint64(0, true));
      out.frames++;
      return true;
    }
    function close() { closed = true; if (ws) ws.close(); }
    return out;
  }

  root.TussenNDI = { connect };
})(typeof window !== 'undefined' ? window : globalThis);

// A small WebGL2 2D renderer: instanced rounded rectangles and thick
// anti-aliased lines in world coordinates, a pan/zoom camera, and a
// Canvas2D overlay for text. Views fill batches; the renderer draws them in
// order whenever something changes.

const RECT_VS = `#version 300 es
layout(location=0) in vec2 corner;
layout(location=1) in vec4 rect;     // x, y, w, h (world)
layout(location=2) in vec4 fill;
layout(location=3) in vec4 stroke;
layout(location=4) in vec2 style;    // corner radius (world), border (px)
uniform vec2 cam; uniform float scale; uniform vec2 view; uniform float dpr;
out vec2 vLocal; out vec2 vHalf; out vec4 vFill; out vec4 vStroke; out float vRadius; out float vBorder;
void main() {
  vec2 whPx = rect.zw * scale;
  // Keep tiny rectangles visible as at least a pixel.
  vec2 grow = max(vec2(1.0), whPx) - whPx;
  vec2 p = (rect.xy - cam) * scale - grow * 0.5 + corner * (whPx + grow);
  vLocal = (corner - 0.5) * (whPx + grow);
  vHalf = (whPx + grow) * 0.5;
  vFill = fill; vStroke = stroke;
  vRadius = min(style.x * scale, min(vHalf.x, vHalf.y));
  vBorder = style.y * dpr;
  vec2 clip = p / view * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
}`;

const RECT_FS = `#version 300 es
precision highp float;
in vec2 vLocal; in vec2 vHalf; in vec4 vFill; in vec4 vStroke; in float vRadius; in float vBorder;
uniform float dpr;
out vec4 color;
void main() {
  vec2 q = abs(vLocal) - vHalf + vRadius;
  float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - vRadius;
  float aa = 1.0;
  float a = clamp(0.5 - d / aa, 0.0, 1.0);
  vec4 c = vFill;
  if (vBorder > 0.0) {
    float b = clamp(0.5 - (d + vBorder) / aa, 0.0, 1.0);
    c = mix(vStroke, vFill, b);
  }
  color = vec4(c.rgb, c.a * a);
  if (color.a < 0.004) discard;
}`;

const LINE_VS = `#version 300 es
layout(location=0) in vec2 corner;   // x: 0..1 along, y: -1..1 across
layout(location=1) in vec4 seg;      // x0, y0, x1, y1 (world)
layout(location=2) in vec4 col;
layout(location=3) in vec2 width;    // px, world
uniform vec2 cam; uniform float scale; uniform vec2 view; uniform float dpr;
out vec4 vCol; out float vAcross; out float vHalf;
void main() {
  vec2 a = (seg.xy - cam) * scale, b = (seg.zw - cam) * scale;
  vec2 d = b - a; float len = length(d);
  vec2 dir = len > 0.0001 ? d / len : vec2(1.0, 0.0);
  vec2 n = vec2(-dir.y, dir.x);
  float w = max(width.x * dpr, width.y * scale);
  vHalf = w * 0.5;
  float ext = vHalf + 1.0;
  vec2 p = mix(a, b, corner.x) + dir * (corner.x * 2.0 - 1.0) * ext * 0.5 + n * corner.y * ext;
  vAcross = corner.y * ext;
  vCol = col;
  vec2 clip = p / view * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
}`;

const LINE_FS = `#version 300 es
precision highp float;
in vec4 vCol; in float vAcross; in float vHalf;
uniform float dpr;
out vec4 color;
void main() {
  float a = clamp(vHalf - abs(vAcross) + 0.5, 0.0, 1.0);
  color = vec4(vCol.rgb, vCol.a * a);
  if (color.a < 0.004) discard;
}`;

export const RECT_FLOATS = 14;
export const LINE_FLOATS = 10;

function compile(gl, vs, fs) {
  const p = gl.createProgram();
  for (const [type, src] of [[gl.VERTEX_SHADER, vs], [gl.FRAGMENT_SHADER, fs]]) {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    gl.attachShader(p, s);
  }
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
  return p;
}

/** Growable typed array of instances. */
export class Instances {
  constructor(floats) {
    this.floats = floats;
    this.data = new Float32Array(floats * 256);
    this.count = 0;
  }
  clear() { this.count = 0; }
  reserve() {
    if ((this.count + 1) * this.floats > this.data.length) {
      const d = new Float32Array(this.data.length * 2);
      d.set(this.data);
      this.data = d;
    }
    return this.count++ * this.floats;
  }
  /** x, y, w, h, fill rgba, stroke rgba, radius (world), border (px). */
  rect(x, y, w, h, fill, stroke = null, radius = 0, border = 0) {
    const o = this.reserve();
    const d = this.data;
    d[o] = x; d[o + 1] = y; d[o + 2] = w; d[o + 3] = h;
    d[o + 4] = fill[0]; d[o + 5] = fill[1]; d[o + 6] = fill[2]; d[o + 7] = fill[3] ?? 1;
    const s = stroke || fill;
    d[o + 8] = s[0]; d[o + 9] = s[1]; d[o + 10] = s[2]; d[o + 11] = s[3] ?? 1;
    d[o + 12] = radius; d[o + 13] = border;
  }
  /** A segment with a width in pixels and a minimum in world units. */
  line(x0, y0, x1, y1, col, widthPx = 1, widthWorld = 0) {
    const o = this.reserve();
    const d = this.data;
    d[o] = x0; d[o + 1] = y0; d[o + 2] = x1; d[o + 3] = y1;
    d[o + 4] = col[0]; d[o + 5] = col[1]; d[o + 6] = col[2]; d[o + 7] = col[3] ?? 1;
    d[o + 8] = widthPx; d[o + 9] = widthWorld;
  }
  /** A cubic Bézier as segments. */
  bezier(x0, y0, c0x, c0y, c1x, c1y, x1, y1, col, widthPx, widthWorld = 0, steps = 14) {
    let px = x0, py = y0;
    for (let i = 1; i <= steps; i++) {
      const t = i / steps, u = 1 - t;
      const x = u * u * u * x0 + 3 * u * u * t * c0x + 3 * u * t * t * c1x + t * t * t * x1;
      const y = u * u * u * y0 + 3 * u * u * t * c0y + 3 * u * t * t * c1y + t * t * t * y1;
      this.line(px, py, x, y, col, widthPx, widthWorld);
      px = x; py = y;
    }
  }
}

export class Renderer {
  constructor(wrap) {
    this.wrap = wrap;
    this.canvas = document.createElement('canvas');
    this.overlay = document.createElement('canvas');
    this.overlay.className = 'overlay';
    wrap.append(this.canvas, this.overlay);
    const gl = this.canvas.getContext('webgl2', { antialias: false, premultipliedAlpha: false, alpha: true });
    if (!gl) throw new Error('WebGL2 is not available in this browser');
    this.gl = gl;
    this.ctx = this.overlay.getContext('2d');
    this.cam = { x: 0, y: 0, scale: 1 };
    // Screen space kept clear of toolbars when fitting.
    this.insets = { top: 0, right: 0, bottom: 0, left: 0 };
    this.batches = [];
    this.onOverlay = null;
    this.background = [0, 0, 0, 0];
    this.rectProg = compile(gl, RECT_VS, RECT_FS);
    this.lineProg = compile(gl, LINE_VS, LINE_FS);
    this.quad = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, this.quad);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 0, 1, 0, 0, 1, 1, 1]), gl.STATIC_DRAW);
    this.lineQuad = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, this.lineQuad);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, -1, 1, -1, 0, 1, 1, 1]), gl.STATIC_DRAW);
    this.vaos = new Map();
    this.ro = new ResizeObserver(() => this.resize());
    this.ro.observe(wrap);
    this.resize();
  }

  /** A batch of rectangles or lines, drawn in creation order. */
  batch(kind) {
    const gl = this.gl;
    const b = { kind, inst: new Instances(kind === 'rect' ? RECT_FLOATS : LINE_FLOATS), buf: gl.createBuffer(), vao: gl.createVertexArray(), dirty: true, visible: true };
    gl.bindVertexArray(b.vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, kind === 'rect' ? this.quad : this.lineQuad);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.bindBuffer(gl.ARRAY_BUFFER, b.buf);
    const stride = b.inst.floats * 4;
    const attrs = kind === 'rect' ? [[1, 4, 0], [2, 4, 4], [3, 4, 8], [4, 2, 12]] : [[1, 4, 0], [2, 4, 4], [3, 2, 8]];
    for (const [loc, n, off] of attrs) {
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, n, gl.FLOAT, false, stride, off * 4);
      gl.vertexAttribDivisor(loc, 1);
    }
    gl.bindVertexArray(null);
    this.batches.push(b);
    return b;
  }

  /** Marks batches as changed and redraws. */
  update(...bs) {
    for (const b of bs) b.dirty = true;
    this.redraw();
  }

  resize() {
    const r = this.wrap.getBoundingClientRect();
    this.dpr = window.devicePixelRatio || 1;
    this.w = Math.max(1, r.width);
    this.h = Math.max(1, r.height);
    for (const c of [this.canvas, this.overlay]) {
      c.width = Math.round(this.w * this.dpr);
      c.height = Math.round(this.h * this.dpr);
    }
    if (this.pendingFit && this.w > 2 && this.h > 2) this.fit(...this.pendingFit);
    this.redraw();
  }

  redraw() {
    if (this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.draw();
    });
  }

  draw() {
    const gl = this.gl;
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.clearColor(...this.background);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFuncSeparate(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA, gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    const s = this.cam.scale * this.dpr;
    for (const b of this.batches) {
      if (!b.visible || b.inst.count === 0) continue;
      if (b.dirty) {
        gl.bindBuffer(gl.ARRAY_BUFFER, b.buf);
        gl.bufferData(gl.ARRAY_BUFFER, b.inst.data.subarray(0, b.inst.count * b.inst.floats), gl.DYNAMIC_DRAW);
        b.dirty = false;
      }
      const prog = b.kind === 'rect' ? this.rectProg : this.lineProg;
      gl.useProgram(prog);
      gl.uniform2f(gl.getUniformLocation(prog, 'cam'), this.cam.x, this.cam.y);
      gl.uniform1f(gl.getUniformLocation(prog, 'scale'), s);
      gl.uniform2f(gl.getUniformLocation(prog, 'view'), this.canvas.width, this.canvas.height);
      gl.uniform1f(gl.getUniformLocation(prog, 'dpr'), this.dpr);
      gl.bindVertexArray(b.vao);
      gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, b.inst.count);
    }
    gl.bindVertexArray(null);
    const ctx = this.ctx;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, this.overlay.width, this.overlay.height);
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    if (this.onOverlay) this.onOverlay(ctx, this);
  }

  // ----- camera -----
  toScreen(x, y) { return [(x - this.cam.x) * this.cam.scale, (y - this.cam.y) * this.cam.scale]; }
  toWorld(sx, sy) { return [sx / this.cam.scale + this.cam.x, sy / this.cam.scale + this.cam.y]; }
  /** The visible world rectangle. */
  viewRect() {
    const [x0, y0] = this.toWorld(0, 0);
    const [x1, y1] = this.toWorld(this.w, this.h);
    return { x0, y0, x1, y1 };
  }
  fit(x0, y0, x1, y1, margin = 40, maxScale = 4) {
    // Hidden views have no size yet: fit once they are shown.
    if (this.w <= 2 || this.h <= 2 || !this.wrap.offsetParent) {
      this.pendingFit = [x0, y0, x1, y1, margin, maxScale];
      return;
    }
    this.pendingFit = null;
    const ins = this.insets;
    const aw = Math.max(40, this.w - ins.left - ins.right - 2 * margin);
    const ah = Math.max(40, this.h - ins.top - ins.bottom - 2 * margin);
    const w = Math.max(1, x1 - x0), hh = Math.max(1, y1 - y0);
    const s = Math.min(aw / w, ah / hh, maxScale);
    this.cam.scale = Math.max(1e-4, s);
    const cx = ins.left + margin + aw / 2, cy = ins.top + margin + ah / 2;
    this.cam.x = (x0 + x1) / 2 - cx / this.cam.scale;
    this.cam.y = (y0 + y1) / 2 - cy / this.cam.scale;
    this.redraw();
  }
  zoomAt(sx, sy, f) {
    const [wx, wy] = this.toWorld(sx, sy);
    this.cam.scale = Math.min(200, Math.max(1e-3, this.cam.scale * f));
    this.cam.x = wx - sx / this.cam.scale;
    this.cam.y = wy - sy / this.cam.scale;
    this.redraw();
  }
  /** Smoothly centres the camera on a world point. */
  panTo(x, y, scale = this.cam.scale) {
    const from = { ...this.cam };
    const to = { scale, x: x - this.w / 2 / scale, y: y - this.h / 2 / scale };
    const t0 = performance.now();
    const step = (t) => {
      const k = Math.min(1, (t - t0) / 260);
      const e = 1 - (1 - k) ** 3;
      this.cam.scale = from.scale * (to.scale / from.scale) ** e;
      this.cam.x = from.x + (to.x - from.x) * e;
      this.cam.y = from.y + (to.y - from.y) * e;
      this.draw();
      if (k < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }

  /** Pan with drag, zoom with the wheel or pinch; reports clicks and hovers. */
  interact({ onHover, onClick, onDoubleClick, onLeave } = {}) {
    const el = this.canvas;
    let drag = null;
    const pointers = new Map();
    const pos = (e) => {
      const r = el.getBoundingClientRect();
      return [e.clientX - r.left, e.clientY - r.top];
    };
    el.addEventListener('wheel', (e) => {
      e.preventDefault();
      const [x, y] = pos(e);
      const f = Math.exp(-e.deltaY * (e.deltaMode === 1 ? 0.05 : 0.0018));
      this.zoomAt(x, y, f);
    }, { passive: false });
    el.addEventListener('pointerdown', (e) => {
      el.setPointerCapture(e.pointerId);
      pointers.set(e.pointerId, pos(e));
      const [x, y] = pos(e);
      drag = { x, y, cx: this.cam.x, cy: this.cam.y, moved: false };
    });
    el.addEventListener('pointermove', (e) => {
      const p = pos(e);
      if (pointers.has(e.pointerId)) {
        const prev = pointers.get(e.pointerId);
        pointers.set(e.pointerId, p);
        if (pointers.size === 2) {
          const [a, b] = [...pointers.values()];
          const others = [...pointers.entries()].find(([id]) => id !== e.pointerId)[1];
          const d0 = Math.hypot(prev[0] - others[0], prev[1] - others[1]);
          const d1 = Math.hypot(a[0] - b[0], a[1] - b[1]);
          if (d0 > 0) this.zoomAt((a[0] + b[0]) / 2, (a[1] + b[1]) / 2, d1 / d0);
          drag = null;
          return;
        }
      }
      if (drag && pointers.has(e.pointerId)) {
        const dx = p[0] - drag.x, dy = p[1] - drag.y;
        if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
        if (drag.moved) {
          el.style.cursor = 'grabbing';
          this.cam.x = drag.cx - dx / this.cam.scale;
          this.cam.y = drag.cy - dy / this.cam.scale;
          this.redraw();
        }
        return;
      }
      if (onHover) onHover(...this.toWorld(...p), e);
    });
    const up = (e) => {
      pointers.delete(e.pointerId);
      el.style.cursor = '';
      if (drag && !drag.moved && onClick && e.type === 'pointerup') onClick(...this.toWorld(...pos(e)), e);
      drag = null;
    };
    el.addEventListener('pointerup', up);
    el.addEventListener('pointercancel', up);
    el.addEventListener('dblclick', (e) => onDoubleClick && onDoubleClick(...this.toWorld(...pos(e)), e));
    el.addEventListener('pointerleave', () => onLeave && onLeave());
  }
}

/** Zoom buttons for a view, floating at its bottom right. */
export function zoomBar(r, fit, h, icon) {
  return h('div', { class: 'zoombar' },
    h('button', { class: 'icon-btn', title: 'Zoom in', onclick: () => r.zoomAt(r.w / 2, r.h / 2, 1.3) }, icon('plus')),
    h('button', { class: 'icon-btn', title: 'Zoom out', onclick: () => r.zoomAt(r.w / 2, r.h / 2, 1 / 1.3) }, icon('minus')),
    h('button', { class: 'icon-btn', title: 'Fit to view (F)', onclick: fit }, icon('fit')));
}

/** rgba with an alpha override. */
export function rgba(c, a) {
  return [c[0], c[1], c[2], a ?? c[3] ?? 1];
}

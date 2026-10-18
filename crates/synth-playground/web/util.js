// Small helpers shared by the views.

/** Creates an element: h('div', {class: 'x', onclick}, child, 'text', ...). */
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
    else if (k === 'html') el.innerHTML = v;
    else if (k in el && typeof v !== 'string') el[k] = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

/** Replaces an element's children, flattening arrays and skipping nulls. */
export function fill(el, ...children) {
  el.replaceChildren(...children.flat(Infinity).filter((c) => c !== null && c !== undefined && c !== false));
  return el;
}

/** An `<svg><use>` icon. */
export function icon(name) {
  const ns = 'http://www.w3.org/2000/svg';
  const s = document.createElementNS(ns, 'svg');
  const u = document.createElementNS(ns, 'use');
  u.setAttribute('href', `#i-${name}`);
  s.append(u);
  return s;
}

export function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);
}

export function debounce(f, ms) {
  let t = 0;
  const d = (...a) => {
    clearTimeout(t);
    t = setTimeout(() => f(...a), ms);
  };
  d.cancel = () => clearTimeout(t);
  return d;
}

/** A minimal event emitter. */
export class Emitter {
  constructor() { this.handlers = new Map(); }
  on(ev, f) {
    if (!this.handlers.has(ev)) this.handlers.set(ev, new Set());
    this.handlers.get(ev).add(f);
    return () => this.handlers.get(ev).delete(f);
  }
  emit(ev, ...a) { for (const f of this.handlers.get(ev) || []) f(...a); }
}

export const store = {
  get(k, d) {
    try {
      const v = localStorage.getItem(`synth-pg:${k}`);
      return v === null ? d : JSON.parse(v);
    } catch { return d; }
  },
  set(k, v) {
    try { localStorage.setItem(`synth-pg:${k}`, JSON.stringify(v)); } catch { /* full or disabled */ }
  },
};

// ----- colours -----

/** '#rrggbb' → [r, g, b] in 0..1. */
export function rgb(hex) {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return [0.5, 0.5, 0.5];
  const n = parseInt(m[1], 16);
  return [(n >> 16 & 255) / 255, (n >> 8 & 255) / 255, (n & 255) / 255];
}

export function hex([r, g, b]) {
  const c = (x) => Math.round(Math.max(0, Math.min(1, x)) * 255).toString(16).padStart(2, '0');
  return `#${c(r)}${c(g)}${c(b)}`;
}

export function hsl(h, s, l) {
  const k = (n) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n) => l - a * Math.max(-1, Math.min(k(n) - 3, Math.min(9 - k(n), 1)));
  return [f(0), f(8), f(4)];
}

export function mix(a, b, t) {
  return [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
}

/** A stable hue for a string. */
export function hashHue(s) {
  let x = 2166136261;
  for (let i = 0; i < s.length; i++) x = Math.imul(x ^ s.charCodeAt(i), 16777619);
  return ((x >>> 0) % 360);
}

/** Green → yellow → red for 0..1 (criticality). */
export function heat(t) {
  t = Math.max(0, Math.min(1, t));
  return t < 0.5 ? mix([0.22, 0.75, 0.45], [0.95, 0.78, 0.25], t * 2) : mix([0.95, 0.78, 0.25], [0.95, 0.3, 0.3], (t - 0.5) * 2);
}

/** CSS variable value. */
export function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

// ----- misc -----

export function toast(msg) {
  const t = h('div', { class: 'toast' }, msg);
  document.body.append(t);
  setTimeout(() => t.remove(), 2500);
}

export function fmtNum(n) {
  return n >= 10000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

/** Bytes of a base64 string. */
export function fromBase64(s) {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function toBase64Url(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function fromBase64Url(s) {
  return fromBase64(s.replace(/-/g, '+').replace(/_/g, '/'));
}

async function pipe(bytes, stream) {
  return new Uint8Array(await new Response(new Blob([bytes]).stream().pipeThrough(stream)).arrayBuffer());
}

/** Compresses a value for a URL fragment. */
export async function packState(value) {
  const bytes = new TextEncoder().encode(JSON.stringify(value));
  if (typeof CompressionStream === 'undefined') return `j${toBase64Url(bytes)}`;
  return `z${toBase64Url(await pipe(bytes, new CompressionStream('deflate-raw')))}`;
}

export async function unpackState(s) {
  const bytes = fromBase64Url(s.slice(1));
  const raw = s[0] === 'z' ? await pipe(bytes, new DecompressionStream('deflate-raw')) : bytes;
  return JSON.parse(new TextDecoder().decode(raw));
}

export function download(name, data, type = 'application/octet-stream') {
  const url = URL.createObjectURL(new Blob([data], { type }));
  const a = h('a', { href: url, download: name });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** Shows `el` as a tooltip near (x, y) in client coordinates. */
export function showTooltip(content, x, y) {
  const t = document.getElementById('tooltip');
  if (typeof content === 'string') t.innerHTML = content;
  else t.replaceChildren(content);
  t.hidden = false;
  const r = t.getBoundingClientRect();
  const left = Math.min(x + 14, window.innerWidth - r.width - 8);
  const top = y + 18 + r.height > window.innerHeight ? y - r.height - 10 : y + 18;
  t.style.left = `${Math.max(4, left)}px`;
  t.style.top = `${Math.max(4, top)}px`;
}

export function hideTooltip() {
  document.getElementById('tooltip').hidden = true;
}

/** Renders the small markdown subset of hover texts (code blocks, `code`). */
export function markdown(md, highlight) {
  const parts = md.split(/```(\w*)\n([\s\S]*?)```/g);
  let out = '';
  for (let i = 0; i < parts.length; i += 3) {
    const text = parts[i].trim();
    if (text) {
      for (const para of text.split(/\n\n+/)) {
        out += `<p>${esc(para).replace(/`([^`]+)`/g, '<code>$1</code>')}</p>`;
      }
    }
    if (i + 2 < parts.length) out += `<pre>${highlight ? highlight(parts[i + 2].trimEnd(), parts[i + 1]) : esc(parts[i + 2])}</pre>`;
  }
  return out;
}

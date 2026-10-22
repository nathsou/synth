// Syntax highlighting for Weft, Verilog and PCF sources.

import { esc } from './util.js';

export const WEFT_KW = new Set(['module', 'extern', 'fn', 'struct', 'enum', 'const', 'let', 'var', 'wire', 'reg', 'mem', 'inst', 'domain', 'default', 'if', 'else', 'match', 'for', 'in', 'out', 'inout', 'init', 'type', 'async', 'assert', 'true', 'false', 'undef']);
export const WEFT_TYPES = new Set(['bool', 'uint', 'sint', 'index', 'clock', 'int', 'string']);
export const WEFT_BUILTINS = new Set(['concat', 'from_bits', 'unsafe_cross', 'hex_file', 'bin_file', 'clog2', 'bits_for', 'min', 'max', 'sync', 'reset_sync', 'power_on_reset', 'rising', 'falling', 'divider', 'count_ones', 'onehot_to_index', 'priority']);
export const V_KW = new Set('module endmodule input output inout wire reg logic always always_ff always_comb assign begin end if else case casez casex endcase default posedge negedge or and not parameter localparam integer genvar generate endgenerate for while initial function endfunction task endtask signed unsigned'.split(' '));
// PCF, and the XDC subset the Artix-7 flow reads (both comment with `#`).
const PCF_KW = new Set(['set_io', 'set_frequency', '-nowarn', '-pullup', '-pullup_resistor', 'set_property', 'create_clock', 'get_ports', '-dict', '-period', '-name', '-waveform', 'PACKAGE_PIN', 'IOSTANDARD', 'PULLTYPE', 'DRIVE', 'SLEW']);

export function langOf(name) {
  if (name.endsWith('.weft')) return 'weft';
  if (name.endsWith('.v') || name.endsWith('.sv')) return 'verilog';
  if (name.endsWith('.pcf') || name.endsWith('.xdc')) return 'pcf';
  return 'text';
}

const SEM_CLASS = { Register: 'tok-reg', Port: 'tok-port', Module: 'tok-type', Type: 'tok-type', Function: 'tok-fn', Variant: 'tok-attr', Domain: 'tok-dom', Instance: 'tok-var', Const: 'tok-num', Generic: 'tok-num' };

/** Tokens of a source as [start, end, class]. */
export function tokenize(text, lang, sem) {
  const out = [];
  const n = text.length;
  let i = 0;
  let prevWord = '';
  const isId = (c) => /[A-Za-z0-9_$]/.test(c);
  while (i < n) {
    const c = text[i];
    const rest2 = text.slice(i, i + 2);
    if ((lang !== 'pcf' && rest2 === '//') || (lang === 'pcf' && c === '#')) {
      let j = text.indexOf('\n', i);
      if (j < 0) j = n;
      out.push([i, j, 'tok-com']);
      i = j;
      continue;
    }
    if (lang !== 'pcf' && rest2 === '/*') {
      let j = text.indexOf('*/', i + 2);
      j = j < 0 ? n : j + 2;
      out.push([i, j, 'tok-com']);
      i = j;
      continue;
    }
    if (c === '"') {
      let j = i + 1;
      while (j < n && text[j] !== '"' && text[j] !== '\n') j += text[j] === '\\' ? 2 : 1;
      out.push([i, Math.min(n, j + 1), 'tok-str']);
      i = j + 1;
      continue;
    }
    if (lang === 'weft' && rest2 === '#[') {
      let j = text.indexOf(']', i);
      j = j < 0 ? n : j + 1;
      out.push([i, j, 'tok-attr']);
      i = j;
      continue;
    }
    if (/[0-9]/.test(c) || (lang === 'verilog' && c === "'" && /[bhdoBHDO]/.test(text[i + 1] || ''))) {
      let j = i + 1;
      while (j < n && /[0-9A-Za-z_?'.]/.test(text[j])) j++;
      out.push([i, j, 'tok-num']);
      i = j;
      prevWord = '';
      continue;
    }
    if (/[A-Za-z_$`]/.test(c) || (lang === 'pcf' && c === '-')) {
      let j = i + 1;
      while (j < n && (isId(text[j]) || (lang === 'pcf' && /[\-\[\]]/.test(text[j])))) j++;
      const w = text.slice(i, j);
      let cls = null;
      const s = sem && sem.get(i);
      if (lang === 'weft') {
        if (c === '@' || text[i - 1] === '@') cls = 'tok-dom';
        else if (WEFT_KW.has(w)) cls = 'tok-kw';
        else if (WEFT_TYPES.has(w)) cls = 'tok-type';
        else if (s) cls = SEM_CLASS[s] || null;
        else if (['module', 'struct', 'enum'].includes(prevWord)) cls = 'tok-type';
        else if (prevWord === 'fn') cls = 'tok-fn';
        else if (text[i - 1] === '.' && /[A-Z]/.test(c)) cls = 'tok-attr';
        else if (WEFT_BUILTINS.has(w) || /^\s*(<[^;{}]*>)?\s*\(/.test(text.slice(j, j + 40))) cls = /^[A-Z]/.test(w) ? 'tok-type' : 'tok-fn';
      } else if (lang === 'verilog') {
        if (V_KW.has(w)) cls = 'tok-kw';
        else if (c === '$' || c === '`') cls = 'tok-fn';
        else if (prevWord === 'module') cls = 'tok-type';
      } else if (lang === 'pcf') {
        if (PCF_KW.has(w)) cls = 'tok-kw';
      }
      if (cls) out.push([i, j, cls]);
      prevWord = w;
      i = j;
      continue;
    }
    if (c === '@' && lang === 'weft') {
      let j = i + 1;
      while (j < n && isId(text[j])) j++;
      out.push([i, j, 'tok-dom']);
      i = j;
      continue;
    }
    if (/[+\-*/%=<>!&|^~:?]/.test(c)) {
      let j = i + 1;
      while (j < n && /[+\-*/%=<>!&|^~:]/.test(text[j]) && j - i < 3) j++;
      out.push([i, j, 'tok-op']);
      i = j;
      continue;
    }
    if (!/\s/.test(c)) prevWord = c === '.' ? prevWord : '';
    i++;
  }
  return out;
}

export function highlightHtml(text, lang, sem) {
  let html = '';
  let pos = 0;
  for (const [s, e, cls] of tokenize(text, lang, sem)) {
    if (s < pos) continue;
    html += esc(text.slice(pos, s));
    html += `<span class="${cls}">${esc(text.slice(s, e))}</span>`;
    pos = e;
  }
  return html + esc(text.slice(pos));
}


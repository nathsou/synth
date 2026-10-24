// Syntax highlighting for Weft, Verilog and PCF sources, and for the IR
// languages of the text views (`.rtl`, `.gates`, `.netlist`: docs/IR_TEXT.md).

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
  const ext = name.slice(name.lastIndexOf('.') + 1);
  if (IR_LANGS.has(ext)) return ext;
  return 'text';
}

// ----- the IR languages -----

/** The IR languages, by file extension. */
export const IR_LANGS = new Set(['rtl', 'gates', 'netlist']);
/** Words that begin a statement, and the other keywords. */
const IR_STMT = new Set(['module', 'input', 'output', 'inout', 'wire', 'connect', 'inst', 'cell', 'memory', 'init', 'write']);
const IR_KW = new Set(['signed', 'posedge', 'negedge', 'offset']);
const IR_FLAGS = new Set(['en_over_srst']);

/**
 * Tokens of IR text. Statements are lines: a keyword first, or a signal
 * then `=` and an operation (`$n3 = add count, 26'd1 @$binary0`). Classes:
 * keywords, operations (after `=`), argument and pin names (before `:`
 * in an argument list), cell types (after `inst name:` / `cell name:`),
 * constants, `@cell` labels, comments.
 */
function tokenizeIr(text) {
  const out = [];
  const n = text.length;
  let i = 0;
  let stmt = null; // the line's first word
  let last = null; // the last significant token: a punctuation, 'word', 'num' or 'op'
  let colons = 0;
  while (i < n) {
    const c = text[i];
    if (c === '\n') {
      stmt = null;
      last = null;
      colons = 0;
      i++;
      continue;
    }
    if (c === ' ' || c === '\t' || c === '\r') {
      i++;
      continue;
    }
    if (c === '/' && text[i + 1] === '/') {
      let j = text.indexOf('\n', i);
      if (j < 0) j = n;
      out.push([i, j, 'tok-com']);
      i = j;
      continue;
    }
    if (c === '@') {
      let j = i + 1;
      if (text[j] === '"') {
        j++;
        while (j < n && text[j] !== '"' && text[j] !== '\n') j += text[j] === '\\' ? 2 : 1;
        j++;
      } else while (j < n && /[A-Za-z0-9_$.]/.test(text[j])) j++;
      out.push([i, Math.min(j, n), 'tok-attr']);
      i = j;
      continue;
    }
    if (c === '"') {
      // A quoted name (or a string parameter): not coloured, like names.
      let j = i + 1;
      while (j < n && text[j] !== '"' && text[j] !== '\n') j += text[j] === '\\' ? 2 : 1;
      const typeHere = (stmt === 'inst' || stmt === 'cell') && last === ':' && colons === 1;
      if (typeHere) out.push([i, j + 1, 'tok-type']);
      stmt = stmt || 'name';
      last = 'word';
      i = j + 1;
      continue;
    }
    if (/[0-9]/.test(c)) {
      let j = i;
      while (j < n && /[0-9]/.test(text[j])) j++;
      if (text[j] === "'" && /[bdhBDH]/.test(text[j + 1] || '')) {
        j += 2;
        while (j < n && /[0-9a-fA-FxXzZ_]/.test(text[j])) j++;
      }
      out.push([i, j, 'tok-num']);
      last = 'num';
      i = j;
      continue;
    }
    if (/[A-Za-z_$]/.test(c)) {
      let j = i + 1;
      while (j < n && /[A-Za-z0-9_$.]/.test(text[j])) j++;
      const w = text.slice(i, j);
      let k = j;
      while (text[k] === ' ') k++;
      // An argument, parameter or pin name: before `:` (or a pin's `->`,
      // `<->`) in an argument list.
      const key = (text[k] === ':' || text.startsWith('->', k) || text.startsWith('<->', k)) && [',', '(', '{', 'op'].includes(last);
      let cls = null;
      if (stmt === null && IR_STMT.has(w)) cls = 'tok-kw';
      else if (IR_KW.has(w) || (w === 'x' && last === 'num' && stmt === 'memory')) cls = 'tok-kw';
      else if (IR_FLAGS.has(w)) cls = 'tok-attr';
      else if (last === '=' && stmt !== 'connect') cls = 'tok-fn';
      else if (key) cls = 'tok-attr';
      else if ((stmt === 'inst' || stmt === 'cell') && last === ':' && colons === 1) cls = 'tok-type';
      if (cls) out.push([i, j, cls]);
      last = cls === 'tok-fn' ? 'op' : 'word';
      if (stmt === null) stmt = IR_STMT.has(w) ? w : 'name';
      i = j;
      continue;
    }
    const three = text.slice(i, i + 3), two = text.slice(i, i + 2);
    const op = three === '<->' ? three : two === '->' ? two : '=?:~'.includes(c) ? c : null;
    if (op) {
      out.push([i, i + op.length, 'tok-op']);
      if (op === ':') colons++;
      last = op;
      i += op.length;
      continue;
    }
    last = c;
    i++;
  }
  return out;
}

const SEM_CLASS = { Register: 'tok-reg', Port: 'tok-port', Module: 'tok-type', Type: 'tok-type', Function: 'tok-fn', Variant: 'tok-attr', Domain: 'tok-dom', Instance: 'tok-var', Const: 'tok-num', Generic: 'tok-num' };

/** Tokens of a source as [start, end, class]. */
export function tokenize(text, lang, sem) {
  if (IR_LANGS.has(lang)) return tokenizeIr(text);
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


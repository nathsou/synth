// A lightweight code editor: a textarea over highlighted layers, with
// diagnostics, hover, go to definition (Ctrl+click), occurrence highlights
// and completion from the analysis of the Weft sources.

import { h, esc, markdown, showTooltip, hideTooltip, debounce } from './util.js';
import { langOf, highlightHtml, WEFT_KW, WEFT_TYPES, WEFT_BUILTINS, V_KW } from './highlight.js';
import { settings } from './settings.js';

export { langOf };

/** Completion item kinds by reference kind. */
const KIND_ICON = { Module: 'M', Function: 'ƒ', Type: 'T', Variant: '·', Field: '.', Port: 'p', Wire: 'w', Register: 'r', Memory: 'm', Instance: 'i', Domain: '@', Const: 'c', Value: 'v', Variable: 'v', Generic: 'g', keyword: 'k' };

export class Editor {
  constructor(host, { onChange, onRun, onNavigate, onFormat }) {
    this.onChange = onChange;
    this.onFormat = onFormat;
    this.onRun = onRun;
    this.onNavigate = onNavigate;
    this.lang = 'text';
    this.diags = [];
    this.refs = [];
    this.refsText = null;
    this.flashRange = null;
    this.occ = [];
    this.el = h('div', { class: 'editor' });
    this.gutter = h('div', { class: 'gutter' });
    this.gutterInner = h('div');
    this.gutter.append(this.gutterInner);
    this.scroller = h('div', { class: 'scroller' });
    this.marks = h('pre', { class: 'marks', 'aria-hidden': 'true' });
    this.marksInner = h('div');
    this.marks.append(this.marksInner);
    this.code = h('pre', { class: 'code', 'aria-hidden': 'true' });
    this.codeInner = h('div');
    this.code.append(this.codeInner);
    this.ta = h('textarea', { spellcheck: 'false', autocorrect: 'off', autocapitalize: 'off', autocomplete: 'off', wrap: 'off', 'aria-label': 'Source code' });
    this.scroller.append(this.marks, this.code, this.ta);
    this.el.append(this.gutter, this.scroller);
    host.append(this.el);
    this.measure();
    settings.on('change', (k) => { if (k === 'codeSize') { this.measure(); this.render(); } });

    this.ta.addEventListener('input', () => this.changed());
    this.ta.addEventListener('scroll', () => this.syncScroll());
    this.ta.addEventListener('keydown', (e) => this.key(e));
    this.ta.addEventListener('keyup', (e) => { if (e.key === 'Control' || e.key === 'Meta') this.el.classList.remove('link-mode'); });
    this.ta.addEventListener('mousemove', (e) => this.mouse(e));
    this.ta.addEventListener('mouseleave', () => { hideTooltip(); this.el.classList.remove('link-mode'); });
    this.ta.addEventListener('mousedown', (e) => {
      if ((e.ctrlKey || e.metaKey) && e.button === 0) {
        const off = this.offsetAt(e);
        const r = this.refAt(off);
        if (r && r.decl) {
          e.preventDefault();
          this.onNavigate(r.decl.file, r.decl.start, r.decl.end);
        }
      }
    });
    const caret = debounce(() => { this.updateOccurrences(); this.renderMarks(); this.renderGutter(); }, 60);
    this.ta.addEventListener('keyup', caret);
    this.ta.addEventListener('click', () => { this.closeCompletion(); caret(); });
    this.ta.addEventListener('blur', () => setTimeout(() => this.closeCompletion(), 150));
  }

  measure() {
    const probe = h('span', { style: { position: 'absolute', visibility: 'hidden', whiteSpace: 'pre', font: 'inherit' } }, 'M'.repeat(80));
    this.el.append(probe);
    this.cw = probe.getBoundingClientRect().width / 80 || 7.8;
    probe.remove();
    const fs = settings.get('codeSize');
    this.lh = fs * 1.55;
  }

  setDoc(name, text, state) {
    this.name = name;
    this.lang = langOf(name);
    this.ta.value = text;
    this.closeCompletion();
    this.render();
    if (state) {
      this.ta.setSelectionRange(state.s, state.e);
      this.ta.scrollTop = state.top;
      this.ta.scrollLeft = state.left;
    } else {
      this.ta.setSelectionRange(0, 0);
      this.ta.scrollTop = 0;
    }
    this.syncScroll();
  }

  state() {
    return { s: this.ta.selectionStart, e: this.ta.selectionEnd, top: this.ta.scrollTop, left: this.ta.scrollLeft };
  }

  get text() { return this.ta.value; }

  /** Diagnostics of this file: [{start, end, severity, message, helps}] (string indices). */
  setDiagnostics(d) {
    this.diags = d;
    this.renderMarks();
    this.renderGutter();
  }

  /** Name index for the current text: [{start, end, kind, hover, decl, members}]. */
  setRefs(refs, forText) {
    this.refs = refs.sort((a, b) => a.start - b.start);
    this.refsText = forText;
    this.renderCode();
    this.updateOccurrences();
    this.renderMarks();
  }

  fresh() { return this.refsText === this.ta.value; }

  changed() {
    this.render();
    this.onChange(this.ta.value);
    if (this.completion) this.updateCompletion();
  }

  render() {
    this.renderCode();
    this.renderMarks();
    this.renderGutter();
  }

  renderCode() {
    const sem = new Map();
    if (this.fresh()) for (const r of this.refs) sem.set(r.start, r.kind);
    this.codeInner.innerHTML = `${highlightHtml(this.ta.value, this.lang, sem)}\n`;
  }

  renderMarks() {
    const text = this.ta.value;
    const ranges = [];
    for (const d of this.diags) ranges.push([d.start, Math.max(d.end, d.start + 1), `d ${d.severity}`]);
    for (const [s, e] of this.occ) ranges.push([s, e, 'hl']);
    if (this.flashRange) ranges.push([...this.flashRange, 'flash']);
    ranges.sort((a, b) => a[0] - b[0]);
    let html = '';
    let pos = 0;
    for (const [s0, e0, cls] of ranges) {
      const s = Math.max(s0, pos), e = Math.min(e0, text.length);
      if (e <= s) continue;
      html += esc(text.slice(pos, s));
      html += `<span class="${cls}">${esc(text.slice(s, e))}</span>`;
      pos = e;
    }
    this.marksInner.innerHTML = `${html}${esc(text.slice(pos))}\n`;
  }

  renderGutter() {
    const text = this.ta.value;
    const lines = text.split('\n').length;
    const cur = text.slice(0, this.ta.selectionStart).split('\n').length;
    const marks = new Map();
    for (const d of this.diags) {
      const l = text.slice(0, d.start).split('\n').length;
      if (marks.get(l) !== 'error') marks.set(l, d.severity === 'error' ? 'error' : 'warning');
    }
    let html = '';
    for (let i = 1; i <= lines; i++) {
      const m = marks.get(i);
      html += `<div class="ln${i === cur ? ' cur' : ''}" style="height:${this.lh}px">${m ? `<i class="mark ${m}"></i>` : ''}${i}</div>`;
    }
    this.gutterInner.innerHTML = html;
    this.gutter.style.minWidth = `${String(lines).length * this.cw + 30}px`;
    this.syncScroll();
  }

  syncScroll() {
    const t = `translate(${-this.ta.scrollLeft}px, ${-this.ta.scrollTop}px)`;
    this.codeInner.style.transform = t;
    this.marksInner.style.transform = t;
    this.gutterInner.style.transform = `translateY(${-this.ta.scrollTop}px)`;
    if (this.completion) this.placeCompletion();
  }

  offsetAt(e) {
    const r = this.ta.getBoundingClientRect();
    const style = getComputedStyle(this.ta);
    const x = e.clientX - r.left - parseFloat(style.paddingLeft) + this.ta.scrollLeft;
    const y = e.clientY - r.top - parseFloat(style.paddingTop) + this.ta.scrollTop;
    const line = Math.floor(y / this.lh);
    const lines = this.ta.value.split('\n');
    if (line < 0 || line >= lines.length) return -1;
    let col = 0, px = 0;
    const lt = lines[line];
    while (col < lt.length) {
      const w = (lt[col] === '\t' ? 4 : 1) * this.cw;
      if (px + w / 2 > x) break;
      px += w;
      col++;
    }
    if (x > px + this.cw) return -1;
    let off = 0;
    for (let i = 0; i < line; i++) off += lines[i].length + 1;
    return off + col;
  }

  refAt(off) {
    if (off < 0 || !this.fresh()) return null;
    let best = null;
    for (const r of this.refs) {
      if (r.start <= off && off < r.end && (!best || r.end - r.start < best.end - best.start)) best = r;
      if (r.start > off) break;
    }
    return best;
  }

  mouse(e) {
    const off = this.offsetAt(e);
    const r = this.refAt(off);
    this.el.classList.toggle('link-mode', !!((e.ctrlKey || e.metaKey) && r && r.decl));
    const ds = this.diags.filter((d) => d.start <= off && off < Math.max(d.end, d.start + 1));
    if (!r && !ds.length) {
      hideTooltip();
      return;
    }
    let html = '';
    for (const d of ds) {
      html += `<div style="color:var(--${d.severity === 'error' ? 'error' : d.severity === 'warning' ? 'warning' : 'note'})">${esc(d.message)}</div>`;
      for (const x of d.helps || []) html += `<p>help: ${esc(x).replace(/`([^`]+)`/g, '<code>$1</code>')}</p>`;
    }
    if (r) {
      if (html) html += '<hr style="border:0;border-top:1px solid var(--line);margin:6px 0">';
      html += markdown(r.hover, (code, lang) => highlightHtml(code, lang === 'weft' ? 'weft' : 'text'));
      if (r.decl) html += `<div class="t">${navigator.platform.includes('Mac') ? '⌘' : 'Ctrl'}+click to go to the definition</div>`;
    }
    showTooltip(html, e.clientX, e.clientY);
  }

  updateOccurrences() {
    this.occ = [];
    const r = this.refAt(this.ta.selectionStart) || this.refAt(this.ta.selectionStart - 1);
    if (r && r.decl) {
      for (const x of this.refs) {
        if (x.decl && x.decl.file === r.decl.file && x.decl.start === r.decl.start) this.occ.push([x.start, x.end]);
      }
      if (this.occ.length < 2) this.occ = [];
    }
  }

  /** Selects a range, scrolls it into view and flashes it. */
  reveal(start, end) {
    this.ta.focus();
    this.ta.setSelectionRange(start, end);
    const line = this.ta.value.slice(0, start).split('\n').length - 1;
    const top = line * this.lh;
    if (top < this.ta.scrollTop || top > this.ta.scrollTop + this.ta.clientHeight - 2 * this.lh) this.ta.scrollTop = Math.max(0, top - this.ta.clientHeight / 3);
    this.flashRange = [start, Math.max(end, start + 1)];
    this.renderMarks();
    this.renderGutter();
    clearTimeout(this.flashTimer);
    this.flashTimer = setTimeout(() => { this.flashRange = null; this.renderMarks(); }, 1300);
  }

  // ----- editing -----

  insert(text, start = this.ta.selectionStart, end = this.ta.selectionEnd) {
    this.ta.setSelectionRange(start, end);
    if (!document.execCommand('insertText', false, text)) {
      this.ta.setRangeText(text, start, end, 'end');
      this.changed();
    }
  }

  /** Replaces the whole text, keeping the caret's line and undo history. */
  replaceAll(text) {
    const line = this.ta.value.slice(0, this.ta.selectionStart).split('\n').length - 1;
    const top = this.ta.scrollTop;
    this.insert(text, 0, this.ta.value.length);
    const lines = text.split('\n');
    let off = 0;
    for (let i = 0; i < Math.min(line, lines.length - 1); i++) off += lines[i].length + 1;
    off += /^\s*/.exec(lines[Math.min(line, lines.length - 1)])[0].length;
    this.ta.setSelectionRange(off, off);
    this.ta.scrollTop = top;
    this.syncScroll();
  }

  lineStart(pos) { return this.ta.value.lastIndexOf('\n', pos - 1) + 1; }

  key(e) {
    const ta = this.ta;
    const v = ta.value;
    const s = ta.selectionStart, en = ta.selectionEnd;
    const mod = e.ctrlKey || e.metaKey;
    if (mod && (e.key === 'Control' || e.key === 'Meta')) this.el.classList.add('link-mode');
    if (this.completion && this.completionKey(e)) return;
    if (e.shiftKey && e.altKey && (e.key === 'F' || e.key === 'f' || e.code === 'KeyF')) {
      e.preventDefault();
      this.onFormat();
      return;
    }
    if (mod && (e.key === 'Enter' || e.key === 's')) {
      e.preventDefault();
      this.onRun();
      return;
    }
    if (mod && e.key === ' ') {
      e.preventDefault();
      this.openCompletion(true);
      return;
    }
    if (mod && e.key === '/') {
      e.preventDefault();
      const marker = this.lang === 'pcf' ? '# ' : '// ';
      const a = this.lineStart(s);
      const b = v.indexOf('\n', en - (en > s && v[en - 1] === '\n' ? 1 : 0));
      const block = v.slice(a, b < 0 ? v.length : b);
      const lines = block.split('\n');
      const all = lines.every((l) => !l.trim() || l.trimStart().startsWith(marker.trim()));
      const out = lines.map((l) => {
        if (!l.trim()) return l;
        if (all) return l.replace(new RegExp(`^(\\s*)${marker.trim().replace(/[/]/g, '\\/')} ?`), '$1');
        const ind = /^\s*/.exec(l)[0];
        return ind + marker + l.slice(ind.length);
      }).join('\n');
      this.insert(out, a, a + block.length);
      ta.setSelectionRange(a, a + out.length);
      return;
    }
    if (e.key === 'Tab') {
      e.preventDefault();
      if (s === en && !e.shiftKey) {
        this.insert('    ');
        return;
      }
      const a = this.lineStart(s);
      const block = v.slice(a, en);
      const out = e.shiftKey ? block.replace(/^ {1,4}/gm, '') : block.replace(/^/gm, '    ');
      this.insert(out, a, en);
      ta.setSelectionRange(a, a + out.length);
      return;
    }
    if (e.key === 'Enter' && !mod) {
      e.preventDefault();
      const a = this.lineStart(s);
      const ind = /^[ \t]*/.exec(v.slice(a))[0];
      const before = v.slice(a, s).trimEnd();
      const opens = /[{([]$/.test(before);
      const closes = /^[ \t]*[})\]]/.test(v.slice(en));
      if (opens && closes) {
        this.insert(`\n${ind}    \n${ind}`);
        ta.setSelectionRange(s + ind.length + 5, s + ind.length + 5);
      } else {
        this.insert(`\n${ind}${opens ? '    ' : ''}`);
      }
      return;
    }
    if (e.key === '}' && s === en) {
      const a = this.lineStart(s);
      if (/^ {4,}$/.test(v.slice(a, s))) {
        e.preventDefault();
        this.insert('}', s - 4, s);
        return;
      }
    }
    const pairs = { '(': ')', '[': ']', '{': '}', '"': '"' };
    if (pairs[e.key] && !mod) {
      if (e.key === '"' && v[s] === '"' && s === en) {
        e.preventDefault();
        ta.setSelectionRange(s + 1, s + 1);
        return;
      }
      const next = v[en] || '';
      if (s !== en || /[\s)\]};,]/.test(next) || next === '') {
        e.preventDefault();
        const sel = v.slice(s, en);
        this.insert(e.key + sel + pairs[e.key], s, en);
        ta.setSelectionRange(s + 1, s + 1 + sel.length);
        return;
      }
    }
    if ([')', ']', '}'].includes(e.key) && s === en && v[s] === e.key) {
      e.preventDefault();
      ta.setSelectionRange(s + 1, s + 1);
      return;
    }
    if (e.key === 'Backspace' && s === en && s > 0 && pairs[v[s - 1]] === v[s]) {
      e.preventDefault();
      this.insert('', s - 1, s + 1);
      return;
    }
    if (e.key === '.' && this.lang === 'weft') setTimeout(() => this.openCompletion(false), 0);
  }

  // ----- completion -----

  candidates() {
    const v = this.ta.value;
    const pos = this.ta.selectionStart;
    let start = pos;
    while (start > 0 && /[A-Za-z0-9_]/.test(v[start - 1])) start--;
    const prefix = v.slice(start, pos);
    const items = new Map();
    const add = (label, kind, detail = '') => { if (!items.has(label)) items.set(label, { label, kind, detail }); };
    const refs = this.allRefs ? this.allRefs() : [];
    const textOf = (r) => r.text;
    if (v[start - 1] === '.') {
      let b = start - 1;
      while (b > 0 && /[A-Za-z0-9_]/.test(v[b - 1])) b--;
      const base = v.slice(b, start - 1);
      if (base) {
        const r = refs.filter((x) => x.file === this.name && textOf(x) === base && x.members && x.members.length)
          .sort((a, c) => Math.abs(a.start - pos) - Math.abs(c.start - pos))[0];
        for (const m of r?.members || []) add(m, r.kind === 'Type' ? 'Variant' : 'Field');
      } else {
        for (const r of refs) if (r.kind === 'Variant') add(textOf(r), 'Variant', firstLine(r.hover));
      }
    } else {
      for (const r of refs) {
        const isDecl = r.decl && r.decl.file === r.file && r.decl.start === r.start;
        if (!isDecl) continue;
        if (r.file !== this.name && !['Module', 'Function', 'Type', 'Const'].includes(r.kind)) continue;
        add(textOf(r), r.kind, firstLine(r.hover));
      }
      if (this.lang === 'weft') {
        for (const w of WEFT_BUILTINS) add(w, 'Function', 'builtin');
        for (const w of WEFT_TYPES) add(w, 'Type', 'builtin type');
        for (const w of WEFT_KW) add(w, 'keyword');
      } else if (this.lang === 'verilog') for (const w of V_KW) add(w, 'keyword');
    }
    const p = prefix.toLowerCase();
    const list = [...items.values()].filter((it) => it.label.toLowerCase().startsWith(p) && it.label !== prefix);
    list.sort((a, b) => (a.kind === 'keyword') - (b.kind === 'keyword') || a.label.localeCompare(b.label));
    return { start, list };
  }

  openCompletion(explicit) {
    const { start, list } = this.candidates();
    if (!list.length) {
      this.closeCompletion();
      return;
    }
    this.completion = { start, list, sel: 0, el: this.completion?.el || h('div', { class: 'completion' }) };
    if (!this.completion.el.isConnected) this.el.append(this.completion.el);
    this.renderCompletion();
  }

  updateCompletion() {
    const { start, list } = this.candidates();
    if (!list.length || start !== this.completion.start && this.ta.value[start - 1] !== '.') {
      if (!list.length) return this.closeCompletion();
    }
    this.completion.start = start;
    this.completion.list = list;
    this.completion.sel = Math.min(this.completion.sel, list.length - 1);
    this.renderCompletion();
  }

  renderCompletion() {
    const c = this.completion;
    c.el.replaceChildren(...c.list.slice(0, 80).map((it, i) => h('div', {
      class: i === c.sel ? 'sel' : '',
      onmousedown: (e) => { e.preventDefault(); c.sel = i; this.acceptCompletion(); },
    }, h('span', { class: 'k' }, KIND_ICON[it.kind] || '·'), h('span', {}, it.label), h('span', { class: 'd' }, it.detail))));
    this.placeCompletion();
    const selEl = c.el.children[c.sel];
    if (selEl) selEl.scrollIntoView({ block: 'nearest' });
  }

  placeCompletion() {
    const c = this.completion;
    if (!c) return;
    const v = this.ta.value;
    const line = v.slice(0, c.start).split('\n').length - 1;
    const col = c.start - this.lineStart(c.start);
    const sr = this.scroller.getBoundingClientRect(), er = this.el.getBoundingClientRect();
    const x = sr.left - er.left + 12 + col * this.cw - this.ta.scrollLeft;
    const y = sr.top - er.top + 8 + (line + 1) * this.lh - this.ta.scrollTop;
    c.el.style.left = `${Math.max(0, x - 22)}px`;
    // Above the caret when there is no room below.
    const hgt = Math.min(240, c.el.scrollHeight || 240);
    const below = y + 2 + hgt <= this.el.clientHeight;
    c.el.style.top = `${below ? y + 2 : Math.max(0, y - this.lh - hgt - 2)}px`;
  }

  completionKey(e) {
    const c = this.completion;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      c.sel = (c.sel + (e.key === 'ArrowDown' ? 1 : -1) + Math.min(80, c.list.length)) % Math.min(80, c.list.length);
      this.renderCompletion();
      return true;
    }
    if (e.key === 'Enter' || e.key === 'Tab') {
      e.preventDefault();
      this.acceptCompletion();
      return true;
    }
    if (e.key === 'Escape') {
      e.preventDefault();
      this.closeCompletion();
      return true;
    }
    if (!/^[A-Za-z0-9_]$/.test(e.key) && e.key !== 'Backspace' && e.key.length === 1) this.closeCompletion();
    return false;
  }

  acceptCompletion() {
    const c = this.completion;
    const it = c.list[c.sel];
    this.closeCompletion();
    if (it) this.insert(it.label, c.start, this.ta.selectionStart);
  }

  closeCompletion() {
    if (this.completion) {
      this.completion.el.remove();
      this.completion = null;
    }
  }
}

function firstLine(md) {
  const m = /```\w*\n([^\n]*)/.exec(md || '');
  return m ? m[1] : '';
}

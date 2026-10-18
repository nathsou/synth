// The playground: wires the editor, the flow and the views together.

import { Backend } from './backend.js';
import { Editor, langOf } from './editor.js';
import { Schematic } from './schematic.js';
import { ChipView } from './chipview.js';
import { BitstreamView, renderPipeline, renderTiming, renderProblems, renderLog } from './panels.js';
import { settings, openSettings } from './settings.js';
import { h, icon, store, debounce, toast, packState, unpackState, download, fromBase64 } from './util.js';

const $ = (id) => document.getElementById(id);

/** Byte offset (UTF-8) → string index, for positions from the flow. */
function byteMapper(text) {
  if (/^[\x00-\x7f]*$/.test(text)) return (b) => b;
  const map = new Uint32Array(new TextEncoder().encode(text).length + 1);
  let b = 0;
  for (let i = 0; i < text.length; i++) {
    const cp = text.codePointAt(i);
    const n = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    for (let k = 0; k < n; k++) map[b + k] = i;
    b += n;
    if (cp >= 0x10000) i++;
  }
  map[b] = text.length;
  return (x) => map[Math.min(x, b)];
}

class App {
  constructor() {
    this.backend = new Backend();
    this.project = null;
    this.result = null;
    this.analysis = null;
    this.examples = [];
    this.devices = [];
    this.view = 'pipeline';
    this.docState = new Map();
  }

  async start() {
    this.status('busy', 'Loading the toolchain…');
    const mode = await this.backend.init();
    try {
      [this.examples, this.devices] = await Promise.all([this.backend.once('examples'), this.backend.once('devices')]);
    } catch (e) {
      this.status('error', `Could not start: ${e.message}`);
      return;
    }
    this.editor = new Editor($('editor'), {
      onChange: (text) => this.edited(text),
      onRun: () => this.run(),
      onFormat: () => this.format(),
      onNavigate: (file, s, e) => this.revealIndex(file, s, e),
    });
    this.editor.allRefs = () => this.allRefs();
    this.schematic = new Schematic($('schematic'), this);
    this.chip = new ChipView($('chip'), this);
    this.bits = new BitstreamView($('bitstream'), this);
    this.setupChrome();
    this.project = (await this.fromHash()) || store.get('project', null) || this.exampleProject(this.examples[0]);
    this.fillDevices();
    this.renderFiles();
    this.openFile(this.project.active || this.project.files[0].name);
    this.status('idle', mode === 'wasm' ? 'Ready (running in your browser)' : 'Ready (running on the local server)');
    this.analyze();
    this.run();
  }

  exampleProject(ex) {
    return {
      example: ex.id,
      files: [...ex.files.map((f) => ({ ...f })), { name: 'pins.pcf', text: ex.pcf }],
      active: ex.files[0].name,
      part: ex.part,
      package: ex.package,
      top: '',
    };
  }

  async fromHash() {
    const m = /[#&]p=([^&]+)/.exec(location.hash);
    if (!m) return null;
    try {
      const p = await unpackState(m[1]);
      history.replaceState(null, '', location.pathname);
      return p;
    } catch {
      toast('Could not read the shared design from the link');
      return null;
    }
  }

  save() {
    store.set('project', this.project);
  }

  // ----- chrome -----

  setupChrome() {
    const ex = $('example');
    ex.replaceChildren(h('option', { value: '' }, 'Choose…'), ...this.examples.map((e) => h('option', { value: e.id, title: e.description }, e.title)));
    ex.addEventListener('change', () => {
      const e = this.examples.find((x) => x.id === ex.value);
      if (!e) return;
      if (this.project.modified && !confirm('Replace the current files with the example?')) {
        ex.value = '';
        return;
      }
      this.project = this.exampleProject(e);
      this.docState.clear();
      this.save();
      this.fillDevices();
      this.renderFiles();
      this.openFile(this.project.active);
      this.analyze();
      this.run();
      ex.value = '';
    });
    $('part').addEventListener('change', (e) => {
      this.project.part = e.target.value;
      this.fillDevices();
      this.save();
      this.run();
    });
    $('package').addEventListener('change', (e) => {
      this.project.package = e.target.value;
      this.save();
      this.run();
    });
    $('run').addEventListener('click', () => this.run());
    $('settings').addEventListener('click', () => openSettings());
    $('theme').addEventListener('click', () => settings.set('theme', settings.get('theme') === 'dark' ? 'light' : 'dark'));
    $('share').addEventListener('click', () => this.share());
    $('download').addEventListener('click', () => {
      if (this.result?.bin) download(`${this.result.top || 'design'}.bin`, fromBase64(this.result.bin));
    });
    $('run-options').addEventListener('click', (e) => this.runOptions(e.currentTarget));
    for (const b of $('view-tabs').querySelectorAll('button')) b.addEventListener('click', () => this.openView(b.dataset.view));
    for (const b of $('bottom-tabs').querySelectorAll('button')) {
      b.addEventListener('click', () => {
        for (const x of $('bottom-tabs').querySelectorAll('button')) x.classList.toggle('active', x === b);
        for (const p of $('bottom-body').children) p.hidden = p.dataset.pane !== b.dataset.tab;
      });
    }
    document.addEventListener('keydown', (e) => {
      if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
        e.preventDefault();
        this.run();
      }
    });
    // Splitters.
    const lay = store.get('layout', {});
    if (lay.left) document.documentElement.style.setProperty('--left-width', lay.left);
    if (lay.bottom) document.documentElement.style.setProperty('--bottom-height', lay.bottom);
    this.splitter($('split-main'), 'x', (dx, start) => {
      const main = $('main').getBoundingClientRect();
      const right = settings.get('layout') === 'right';
      const w = Math.min(main.width - 280, Math.max(260, start + (right ? -dx : dx)));
      const v = `${(100 * w) / main.width}%`;
      document.documentElement.style.setProperty('--left-width', v);
      store.set('layout', { ...store.get('layout', {}), left: v });
    }, () => $('left').getBoundingClientRect().width);
    this.splitter($('split-bottom'), 'y', (dy, start) => {
      const v = `${Math.max(60, Math.min(window.innerHeight * 0.7, start - dy))}px`;
      document.documentElement.style.setProperty('--bottom-height', v);
      store.set('layout', { ...store.get('layout', {}), bottom: v });
    }, () => document.querySelector('.bottom').getBoundingClientRect().height);
    settings.on('change', (k) => {
      if (k === 'layout') this.visible();
      if (['seed', 'freq', 'timingWeight', 'effort'].includes(k)) this.scheduleRun();
    });
    this.scheduleRun = debounce(() => this.run(), 900);
    this.scheduleAnalyze = debounce(() => this.analyze(), 250);
    this.saveSoon = debounce(() => this.save(), 400);
  }

  splitter(el, axis, move, size) {
    el.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      const start = size();
      const p0 = axis === 'x' ? e.clientX : e.clientY;
      el.classList.add('dragging');
      el.setPointerCapture(e.pointerId);
      const mv = (ev) => {
        move((axis === 'x' ? ev.clientX : ev.clientY) - p0, start);
        this.visible();
      };
      const up = () => {
        el.classList.remove('dragging');
        el.removeEventListener('pointermove', mv);
        el.removeEventListener('pointerup', up);
      };
      el.addEventListener('pointermove', mv);
      el.addEventListener('pointerup', up);
    });
  }

  fillDevices() {
    const part = $('part'), pkg = $('package');
    if (!this.devices.find((d) => d.part === this.project.part)) this.project.part = 'hx1k';
    part.replaceChildren(...this.devices.map((d) => h('option', { value: d.part, selected: d.part === this.project.part }, d.part.toUpperCase())));
    const dev = this.devices.find((d) => d.part === this.project.part);
    if (!dev.packages.includes(this.project.package)) this.project.package = dev.packages[0];
    pkg.replaceChildren(...dev.packages.map((p) => h('option', { value: p, selected: p === this.project.package }, p)));
  }

  runOptions(anchor) {
    const host = $('popover-host');
    if (host.firstChild) {
      host.replaceChildren();
      return;
    }
    const r = anchor.getBoundingClientRect();
    const field = (label, key, attrs) => h('div', { class: 'row' }, h('div', { class: 'lbl' }, label),
      h('input', { type: 'number', value: settings.get(key), style: { width: '100px' }, ...attrs, onchange: (e) => settings.set(key, Number(e.target.value)) }));
    const top = h('input', { type: 'text', value: this.project.top || '', placeholder: 'inferred', style: { width: '140px' },
      onchange: (e) => { this.project.top = e.target.value.trim(); this.save(); this.analyze(); this.run(); } });
    const pop = h('div', { class: 'popover', style: { left: `${r.left}px`, top: `${r.bottom + 6}px` } },
      h('div', { class: 'form' },
        h('div', { class: 'row' }, h('div', { class: 'lbl' }, 'Top module'), top),
        field('Placement seed', 'seed', { min: 0 }),
        field('Target MHz (0: none)', 'freq', { min: 0, step: 1 }),
        field('Timing weight (0–1)', 'timingWeight', { min: 0, max: 1, step: 0.05 }),
        field('Placement effort', 'effort', { min: 0.25, max: 4, step: 0.25 })),
      h('div', { style: { marginTop: '10px', display: 'flex', justifyContent: 'flex-end' } }, h('button', { onclick: () => host.replaceChildren() }, 'Close')));
    host.replaceChildren(pop);
    const off = (e) => {
      if (!pop.contains(e.target) && e.target !== anchor && !anchor.contains(e.target)) {
        host.replaceChildren();
        document.removeEventListener('pointerdown', off);
      }
    };
    setTimeout(() => document.addEventListener('pointerdown', off), 0);
  }

  async share() {
    const p = { files: this.project.files, part: this.project.part, package: this.project.package, top: this.project.top, active: this.project.active };
    const url = `${location.origin}${location.pathname}#p=${await packState(p)}`;
    try {
      await navigator.clipboard.writeText(url);
      toast('Link copied to the clipboard');
    } catch {
      prompt('Copy this link:', url);
    }
  }

  // ----- files -----

  renderFiles() {
    const tabs = $('file-tabs');
    const items = this.project.files.map((f) => {
      const close = h('span', {
        class: 'tab-close', title: 'Delete file',
        onclick: (e) => {
          e.stopPropagation();
          if (this.project.files.length === 1 || !confirm(`Delete ${f.name}?`)) return;
          this.project.files = this.project.files.filter((x) => x !== f);
          if (this.project.active === f.name) this.project.active = this.project.files[0].name;
          this.modified();
          this.renderFiles();
          this.openFile(this.project.active);
          this.analyze();
          this.scheduleRun();
        },
      }, icon('x'));
      return h('button', {
        class: f.name === this.project.active ? 'active' : '',
        title: 'Double-click to rename',
        onclick: () => this.openFile(f.name),
        ondblclick: () => this.renameFile(f),
      }, h('span', { class: `lang-${langOf(f.name)}` }, f.name), close);
    });
    tabs.replaceChildren(
      ...items,
      h('button', { class: 'add', title: 'New file', onclick: () => this.newFile() }, icon('plus')),
      h('span', { style: { flex: 1 } }),
      h('button', { class: 'add', title: 'Format the file (Shift+Alt+F)', onclick: () => this.format() }, icon('format'), 'Format'));
  }

  async format() {
    const name = this.project.active;
    if (!name.endsWith('.weft')) return toast('Only Weft files can be formatted');
    const { text } = await this.backend.format(this.editor.text);
    if (text !== this.editor.text) this.editor.replaceAll(text);
  }

  newFile() {
    const name = prompt('File name (.weft, .v or .pcf):', 'new.weft');
    if (!name) return;
    if (this.project.files.some((f) => f.name === name)) return toast(`${name} exists already`);
    const text = name.endsWith('.weft') ? '// A new module.\nmodule example(in a: bool, out y: bool) {\n    y = !a;\n}\n' : '';
    this.project.files.push({ name, text });
    this.modified();
    this.renderFiles();
    this.openFile(name);
    this.analyze();
  }

  renameFile(f) {
    const name = prompt('Rename to:', f.name);
    if (!name || name === f.name) return;
    if (this.project.files.some((x) => x.name === name)) return toast(`${name} exists already`);
    if (this.project.active === f.name) this.project.active = name;
    f.name = name;
    this.modified();
    this.renderFiles();
    this.openFile(name);
    this.analyze();
    this.scheduleRun();
  }

  file(name) {
    return this.project.files.find((f) => f.name === name);
  }

  openFile(name) {
    const f = this.file(name);
    if (!f) return;
    if (this.editor.name) this.docState.set(this.editor.name, this.editor.state());
    this.project.active = name;
    this.editor.setDoc(name, f.text, this.docState.get(name));
    for (const b of $('file-tabs').children) b.classList.toggle('active', b.firstChild?.textContent === name);
    this.applyAnalysis();
    this.saveSoon();
  }

  modified() {
    this.project.modified = true;
    this.saveSoon();
  }

  edited(text) {
    const f = this.file(this.project.active);
    if (!f) return;
    f.text = text;
    this.modified();
    if (settings.get('liveCheck')) this.scheduleAnalyze();
    if (settings.get('autoRun')) this.scheduleRun();
  }

  /** Opens a file at a range of string indices. */
  revealIndex(file, start, end) {
    if (!this.file(file)) return toast(`${file} is not one of the project's files`);
    if (this.project.active !== file) this.openFile(file);
    this.editor.reveal(start, end);
  }

  /** Opens a file at a byte range of its text as the flow saw it. */
  revealSource(file, start, end, texts) {
    const f = this.file(file);
    if (!f) return toast(`${file} is not one of the project's files`);
    if (this.project.active !== file) this.openFile(file);
    const map = byteMapper(texts?.[file] ?? f.text);
    this.editor.reveal(map(start), map(end));
  }

  // ----- analysis and runs -----

  sources() {
    return this.project.files.filter((f) => !f.name.endsWith('.pcf'));
  }

  async analyze() {
    if (!settings.get('liveCheck')) return;
    const files = this.sources().map((f) => ({ name: f.name, text: f.text }));
    const texts = Object.fromEntries(files.map((f) => [f.name, f.text]));
    try {
      const a = await this.backend.analyze({ files, top: this.project.top });
      this.analysis = { ...a, texts };
      this.applyAnalysis();
    } catch (e) {
      if (!e.superseded) console.warn('analysis failed', e);
    }
  }

  /** Name index of all files, in string indices of the analysed texts. */
  allRefs() {
    if (!this.analysis) return [];
    if (this.analysis.converted) return this.analysis.converted;
    const maps = {};
    const mapOf = (file) => (maps[file] = maps[file] || byteMapper(this.analysis.texts[file] || ''));
    this.analysis.converted = this.analysis.refs.map((r) => {
      const m = mapOf(r.file);
      const start = m(r.start), end = m(r.end);
      const text = (this.analysis.texts[r.file] || '').slice(start, end);
      const decl = r.decl ? { file: r.decl.file, start: r.decl.start, end: r.decl.end, bytes: true } : null;
      if (decl && this.analysis.texts[decl.file] !== undefined) {
        const dm = mapOf(decl.file);
        decl.start = dm(decl.start);
        decl.end = dm(decl.end);
      }
      return { ...r, start, end, text, decl };
    });
    return this.analysis.converted;
  }

  diagnostics() {
    if (settings.get('liveCheck') && this.analysis) return { list: this.analysis.diagnostics, texts: this.analysis.texts };
    return { list: this.result?.diagnostics || [], texts: this.resultTexts || {} };
  }

  applyAnalysis() {
    if (!this.editor) return;
    const name = this.project.active;
    const { list, texts } = this.diagnostics();
    const map = byteMapper(texts[name] ?? this.file(name)?.text ?? '');
    this.editor.setDiagnostics(list.filter((d) => d.at && d.at.file === name).map((d) => ({
      start: map(d.at.start), end: map(d.at.end), severity: d.severity, message: d.message, helps: d.helps,
    })));
    if (this.analysis && this.analysis.texts[name] !== undefined) {
      const refs = this.allRefs().filter((r) => r.file === name).map((r) => ({ ...r }));
      this.editor.setRefs(refs, this.analysis.texts[name]);
    } else this.editor.setRefs([], null);
    this.renderProblems(list, texts);
  }

  renderProblems(list, texts) {
    const errors = list.filter((d) => d.severity === 'error').length;
    const warnings = list.filter((d) => d.severity === 'warning').length;
    const badge = $('problem-count');
    badge.textContent = errors + warnings || '';
    badge.className = `badge ${errors ? 'error' : warnings ? 'warning' : ''}`;
    renderProblems($('problems'), list, { revealSource: (f, s, e) => this.revealSource(f, s, e, texts) });
  }

  async run() {
    if (!this.project) return;
    this.scheduleRun?.cancel();
    const pcf = this.project.files.find((f) => f.name.endsWith('.pcf'));
    const files = this.sources().map((f) => ({ name: f.name, text: f.text }));
    const req = {
      files,
      pcf: pcf ? pcf.text : '',
      part: this.project.part,
      package: this.project.package,
      top: this.project.top,
      seed: settings.get('seed'),
      freq: settings.get('freq'),
      timing_weight: settings.get('timingWeight'),
      effort: settings.get('effort'),
    };
    const t0 = performance.now();
    this.status('busy', 'Running the flow…');
    $('run').disabled = true;
    try {
      const out = await this.backend.run(req);
      this.resultTexts = Object.fromEntries(files.map((f) => [f.name, f.text]));
      this.showResult(out, performance.now() - t0);
    } catch (e) {
      if (e.superseded) return;
      this.status('error', e.message);
    } finally {
      $('run').disabled = false;
    }
  }

  showResult(out, ms) {
    this.result = out;
    $('empty').hidden = true;
    $('download').disabled = !out.bin;
    const errors = (out.diagnostics || []).filter((d) => d.severity === 'error').length;
    if (out.ok) {
      const clk = out.timing?.clocks?.[0];
      this.status('ok', `${out.top}: ${out.chip ? `${out.chip.usage.lc} LCs` : 'synthesised'}${clk ? `, ${clk.fmax_mhz.toFixed(1)} MHz` : ''} · ${Math.round(ms)} ms`);
    } else {
      this.status('error', errors ? `${errors} error${errors > 1 ? 's' : ''}` : out.error || 'failed');
    }
    renderPipeline($('pipeline'), out, this);
    renderLog($('log'), out);
    // A design that does not elaborate keeps the last good views (shown
    // as out of date) while it is being fixed.
    const stale = !out.ok && !(out.stages || []).length && this.good;
    document.querySelector('.views-body').classList.toggle('stale', !!stale);
    if (!stale) {
      renderTiming($('timing'), out, this);
      this.schematic.show(out);
      this.chip.show(out);
      this.bits.show(out);
      if (out.ok) this.good = out;
    }
    if (!settings.get('liveCheck') || !this.analysis) this.applyAnalysis();
    if (!out.ok && errors) {
      const first = $('bottom-tabs').querySelector('[data-tab="problems"]');
      first.click();
    }
  }

  status(kind, text) {
    const el = $('status');
    el.className = `status ${kind}`;
    el.replaceChildren(h('span', { class: 'dot' }), h('span', {}, text));
  }

  // ----- views -----

  openView(name) {
    this.view = name;
    for (const b of $('view-tabs').querySelectorAll('button')) b.classList.toggle('active', b.dataset.view === name);
    for (const p of document.querySelectorAll('[data-view-pane]')) p.hidden = p.dataset.viewPane !== name;
    this.visible();
  }

  visible() {
    requestAnimationFrame(() => {
      if (this.view === 'schematic') this.schematic.visible();
      if (this.view === 'chip') this.chip.visible();
      if (this.view === 'bitstream') this.bits.visible();
    });
  }

  openSchematic(stage) {
    this.openView('schematic');
    this.schematic.setStage(stage);
  }

  showBits(x, y) {
    this.openView('bitstream');
    this.bits.select(x, y);
  }

  showCriticalPath(ci) {
    this.openView('chip');
    this.chip.crit = ci;
    this.chip.renderToolbar();
    this.chip.paintHighlight();
    this.chip.r.update(this.chip.hiBatch, this.chip.hiCells);
    requestAnimationFrame(() => this.chip.fitPath(ci));
  }

  focusChipCell(ci, clock) {
    if (ci === undefined) return;
    this.openView('chip');
    if (clock !== undefined) {
      this.chip.crit = clock;
      this.chip.renderToolbar();
    }
    requestAnimationFrame(() => this.chip.focusCell(ci));
  }
}

const app = new App();
window.playground = app;
app.start().catch((e) => {
  console.error(e);
  const el = document.getElementById('status');
  el.className = 'status error';
  el.textContent = `Failed to start: ${e.message}`;
});

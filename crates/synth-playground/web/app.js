// The playground: wires the editor, the flow and the views together.

import { Backend } from './backend.js';
import { Editor, langOf } from './editor.js';
import { Schematic } from './schematic.js';
import { ChipView } from './chipview.js';
import { BitstreamView, renderSummary, renderTiming, renderProblems, renderLog, flowSteps, stageInfo } from './panels.js';
import { openList, closeMenu, matches } from './menus.js';
import { settings, openSettings, toggle } from './settings.js';
import { requestFtdi, supported as usbSupported } from './ftdi.js';
import { programBoard } from './program.js';
import { SimView } from './simview.js';
import { layout as progressLayout, describe as describeProgress, learn as learnProgress } from './progress.js';
import { h, icon, store, debounce, toast, packState, unpackState, download, fromBase64, isPins, fmtNum } from './util.js';

const $ = (id) => document.getElementById(id);
const MAC = /Mac|iPhone|iPad/.test(navigator.platform);
const MOD = MAC ? '⌘' : 'Ctrl';

/** The step of the rail that a progress report's step belongs to. */
const PROGRESS_STEP = { elaborate: 'elaborated', synthesize: 'mapped', device: 'place', pack: 'packed', place: 'place', route: 'route', timing: 'timing', bitstream: 'bitstream' };
const PROGRESS_STEP_XC7 = { elaborate: 'elaborated', synthesize: 'mapped_xc7', device: 'pnr', pack: 'pnr', place: 'pnr', route: 'pnr', timing: 'timing', bitstream: 'bitstream' };

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
    // The step of the rail that is open, and Flow or Simulate.
    this.step = 'summary';
    this.mode = 'flow';
    this.docState = new Map();
    // The run in progress: {family, start, p}; its elapsed time ticks on
    // between reports.
    this.running = null;
    setInterval(() => this.running && this.renderProgress(), 500);
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
    this.sim = new SimView($('simulation'), this);
    this.setupChrome();
    this.project = (await this.fromHash()) || store.get('project', null) || this.exampleProject(this.examples[0]);
    this.backendMode = mode;
    this.fillDevices();
    this.renderProject();
    this.renderFiles();
    this.openFile(this.project.active || this.project.files[0].name);
    this.renderRail();
    this.renderStatusBar();
    this.status('idle', 'Ready');
    this.analyze();
    this.run();
  }

  exampleProject(ex) {
    return {
      example: ex.id,
      files: [...ex.files.map((f) => ({ ...f })), { name: ex.pins_file || 'pins.pcf', text: ex.pins ?? ex.pcf }],
      pcfAuto: true,
      active: ex.files[0].name,
      part: ex.part,
      package: ex.package,
      top: ex.top || '',
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
    $('project').addEventListener('click', (e) => this.openExamples(e.currentTarget));
    $('board').addEventListener('click', (e) => this.openBoards(e.currentTarget));
    $('run').addEventListener('click', () => this.run());
    $('settings').addEventListener('click', () => openSettings());
    $('share').addEventListener('click', () => this.share());
    $('download').addEventListener('click', () => this.downloadBitstream());
    $('program').addEventListener('click', () => this.program());
    $('run-options').addEventListener('click', (e) => this.runOptions(e.currentTarget));
    $('palette').addEventListener('click', () => this.openPalette());
    $('run-kbd').textContent = `${MOD} ↵`;
    $('palette-kbd').textContent = `${MOD} K`;
    $('run').title = `Run the whole flow (${MOD}+Enter)`;
    for (const b of $('mode').querySelectorAll('button')) b.addEventListener('click', () => this.setMode(b.dataset.mode));
    for (const b of $('bottom-tabs').querySelectorAll('button')) {
      b.addEventListener('click', () => {
        this.showBottomTab(b.dataset.tab);
        this.setBottom(true);
      });
    }
    $('bottom-toggle').addEventListener('click', () => this.setBottom($('bottom').classList.contains('collapsed')));
    $('problem-peek').addEventListener('click', () => this.showProblems());
    $('sb-problems').addEventListener('click', () => this.showProblems());
    this.setBottom(store.get('bottomOpen', false), true);
    document.addEventListener('keydown', (e) => {
      const mod = e.ctrlKey || e.metaKey;
      if (mod && e.key === 'Enter') {
        e.preventDefault();
        this.run();
      } else if (mod && (e.key === 'k' || e.key === 'K')) {
        e.preventDefault();
        this.openPalette();
      } else if (mod && (e.key === 'b' || e.key === 'B') && !e.shiftKey && !e.altKey) {
        e.preventDefault();
        this.toggleEditor();
      }
    });
    // The cursor's line and column, in the status bar.
    const cursor = () => requestAnimationFrame(() => this.renderCursor());
    for (const ev of ['keyup', 'pointerup', 'input', 'focusin']) $('editor').addEventListener(ev, cursor);
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
      if (k === 'layout') {
        this.renderFiles();
        this.visible();
      }
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
    if (!this.devices.find((d) => d.part === this.project.part)) this.project.part = 'hx1k';
    const dev = this.device();
    // A new part starts on a package a board uses, when there is one.
    if (!dev.packages.includes(this.project.package)) {
      this.project.package = Object.keys(dev.boards || {})[0] || dev.packages[0];
    }
    this.renderBoard();
  }

  device() {
    return this.devices.find((d) => d.part === this.project.part);
  }

  /** The board of the current device and package, if one uses them. */
  boardName() {
    return this.device()?.boards?.[this.project.package] || '';
  }

  renderBoard() {
    const dev = this.device();
    const board = this.boardName();
    const hz = dev?.clock_hz?.[this.project.package];
    $('board-name').textContent = board || `${this.project.part.toUpperCase()} · ${this.project.package.toUpperCase()}`;
    $('board-meta').textContent = board
      ? `${this.project.part.toUpperCase()} · ${this.project.package.toUpperCase()}${hz ? ` · ${hz / 1e6} MHz` : ''}`
      : 'no board';
    $('board').title = `${board ? `${board}: ` : ''}${dev?.family || ''} ${this.project.part.toUpperCase()}, package ${this.project.package.toUpperCase()}. Change the board`;
  }

  /** The board picker: the boards by family, then the device and package
   *  for a design that runs on none of them. */
  openBoards(anchor) {
    const boards = [];
    for (const d of this.devices) {
      for (const [pkg, name] of Object.entries(d.boards || {})) {
        const hz = d.clock_hz?.[pkg];
        boards.push({ d, pkg, name, detail: `${d.part.toUpperCase()} · ${pkg.toUpperCase()}${hz ? ` · ${hz / 1e6} MHz` : ''}` });
      }
    }
    const pick = (part, pkg) => {
      if (part === this.project.part && pkg === this.project.package) return;
      this.project.part = part;
      this.project.package = pkg;
      this.fillDevices();
      this.devicePins();
    };
    const part = h('select', { 'aria-label': 'Device', onchange: (e) => { closeMenu(); pick(e.target.value, null); } },
      this.devices.map((d) => h('option', { value: d.part, selected: d.part === this.project.part }, `${d.part.toUpperCase()} · ${d.family}`)));
    const dev = this.device();
    const pkg = h('select', { 'aria-label': 'Package', onchange: (e) => { closeMenu(); pick(this.project.part, e.target.value); } },
      dev.packages.map((p) => h('option', { value: p, selected: p === this.project.package }, dev.boards?.[p] ? `${p} · ${dev.boards[p]}` : p)));
    const foot = h('div', { class: 'foot' },
      h('span', { class: 'h4' }, 'Device and package'),
      h('div', { style: { display: 'flex', gap: '8px' } }, part, pkg),
      h('span', { class: 'muted', style: { fontSize: '11.5px' } }, 'Without a board, the placer chooses the pins of an iCE40; give every port a pin on an Artix-7.'));
    openList({
      anchor, placeholder: 'Find a board or device', width: 400, foot,
      items: (q) => boards.map((b) => ({
        group: b.d.family, title: b.name, detail: b.detail,
        tag: b.d.family === 'Artix-7' && this.backendMode === 'wasm' ? '+4 MB' : null,
        check: b.d.part === this.project.part && b.pkg === this.project.package,
        run: () => pick(b.d.part, b.pkg),
      })).filter((it) => matches(it, q)),
    });
  }

  /** The example menu. */
  openExamples(anchor) {
    openList({
      anchor, placeholder: 'Find an example', width: 420,
      items: (q) => {
        const groups = ['Examples', 'RISC-V', 'Project Euler'];
        return this.examples.map((e) => ({
          group: /^euler/.test(e.id) ? 'Project Euler' : /^rv_/.test(e.id) ? 'RISC-V' : 'Examples',
          title: e.title, detail: e.description, check: e.id === this.project.example,
          run: () => this.loadExample(e),
        })).filter((it) => matches(it, q)).sort((a, b) => groups.indexOf(a.group) - groups.indexOf(b.group));
      },
    });
  }

  loadExample(e) {
    if (this.project.modified && !confirm(`Replace the current files with the example “${e.title}”?`)) return;
    this.project = this.exampleProject(e);
    this.docState.clear();
    this.sim.designChanged(true);
    this.save();
    this.fillDevices();
    this.renderProject();
    this.renderFiles();
    this.openFile(this.project.active);
    this.analyze();
    this.run();
  }

  /** The project's name in the top bar: its example, and whether it was edited. */
  renderProject() {
    const ex = this.examples.find((e) => e.id === this.project.example);
    $('project-name').textContent = ex ? ex.title : 'My design';
    $('project-dot').hidden = !this.project.modified;
    $('project').title = ex ? `${ex.description}. Open another example` : 'Open an example';
  }

  /** After a device change: the pins of the new package's board, then run. */
  devicePins() {
    const dev = this.devices.find((d) => d.part === this.project.part);
    const pins = dev?.pins?.[this.project.package];
    const file = this.project.files.find((f) => isPins(f.name));
    if (pins !== undefined) {
      const where = dev.boards?.[this.project.package] || `${this.project.part.toUpperCase()} ${this.project.package}`;
      // Another family's constraints (.pcf or .xdc) are replaced without
      // asking: they cannot apply.
      const other = file && file.name.split('.').pop() !== dev.pins_file.split('.').pop();
      if (!file) {
        this.project.files.push({ name: dev.pins_file, text: pins });
        this.project.pcfAuto = true;
        this.renderFiles();
      } else if (file.text !== pins && (other || this.project.pcfAuto || confirm(`Replace ${file.name} with the pins for the ${where}?`))) {
        const was = file.name;
        file.text = pins;
        if (other) file.name = dev.pins_file;
        this.project.pcfAuto = true;
        if (this.project.active === was) {
          this.project.active = file.name;
          this.editor.setDoc(file.name, pins);
        }
        if (other) this.renderFiles();
        toast(`Pins: ${dev.boards?.[this.project.package] ? `the ${where}` : dev.family === 'Artix-7' ? 'none (give every port a pin)' : 'chosen by the placer'}`);
      }
    }
    this.save();
    this.run();
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
    const pop = h('div', { class: 'popover', style: { left: `${Math.max(8, r.right - 320)}px`, top: `${r.bottom + 6}px`, width: '320px' } },
      h('div', { class: 'form' },
        h('h3', {}, 'Run options'),
        h('div', { class: 'row' }, h('div', { class: 'lbl' }, 'Run after edits'), toggle('runOnEdit')),
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
    const lang = (name) => ({ weft: 'W', v: 'V', sv: 'SV', pcf: 'P', xdc: 'X' })[name.split('.').pop()] || '·';
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
        class: f.name === this.project.active ? 'active' : '', 'data-file': f.name,
        title: 'Double-click to rename',
        onclick: () => this.openFile(f.name),
        ondblclick: () => this.renameFile(f),
      }, h('span', { class: 'lang' }, lang(f.name)), h('span', { class: `lang-${langOf(f.name)}` }, f.name), close);
    });
    tabs.replaceChildren(
      ...items,
      h('button', { class: 'add', title: 'New file', 'aria-label': 'New file', onclick: () => this.newFile() }, icon('plus')),
      h('span', { style: { flex: 1 } }),
      h('button', { class: 'add', title: 'Format the file (Shift+Alt+F)', 'aria-label': 'Format the file', onclick: () => this.format() }, icon('format')),
      h('button', { class: 'add', title: `Hide the editor (${MOD}+B)`, 'aria-label': 'Hide the editor', onclick: () => this.toggleEditor() }, icon('panel')));
    // With the editor hidden, a rail of the files.
    $('files-rail').replaceChildren(
      h('button', { title: `Show the editor (${MOD}+B)`, 'aria-label': 'Show the editor', onclick: () => this.toggleEditor() }, icon('panel')),
      h('hr'),
      ...this.project.files.map((f) => h('button', {
        class: f.name === this.project.active ? 'active' : '', title: f.name, 'aria-label': f.name,
        onclick: () => { this.openFile(f.name); this.toggleEditor(); },
      }, lang(f.name))));
  }

  toggleEditor() {
    const hidden = settings.get('layout') === 'hidden';
    if (!hidden) store.set('editorSide', settings.get('layout'));
    settings.set('layout', hidden ? store.get('editorSide', 'left') : 'hidden');
    if (hidden) requestAnimationFrame(() => this.editor?.ta.focus());
  }

  async format() {
    const name = this.project.active;
    if (!name.endsWith('.weft')) return toast('Only Weft files can be formatted');
    const { text } = await this.backend.format(this.editor.text);
    if (text !== this.editor.text) this.editor.replaceAll(text);
  }

  newFile() {
    const name = prompt('File name (.weft, .v, .pcf or .xdc):', 'new.weft');
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
    for (const b of $('file-tabs').children) b.classList.toggle('active', b.dataset.file === name);
    for (const b of $('files-rail').children) b.classList.toggle('active', b.title === name);
    $('sb-lang').textContent = { weft: 'Weft', v: 'Verilog', sv: 'SystemVerilog', pcf: 'PCF', xdc: 'XDC' }[name.split('.').pop()] || '';
    this.renderCursor();
    this.applyAnalysis();
    this.saveSoon();
  }

  modified() {
    if (!this.project.modified) {
      this.project.modified = true;
      this.renderProject();
    }
    this.saveSoon();
  }

  edited(text) {
    const f = this.file(this.project.active);
    if (!f) return;
    f.text = text;
    if (isPins(f.name)) this.project.pcfAuto = false;
    this.modified();
    if (settings.get('liveCheck')) this.scheduleAnalyze();
    if (settings.get('runOnEdit')) this.scheduleRun();
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
    return this.project.files.filter((f) => !isPins(f.name));
  }

  /** The design to simulate: the sources and the top. */
  simRequest() {
    return { files: this.sources().map((f) => ({ name: f.name, text: f.text })), top: this.project.top };
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
    const first = list.find((d) => d.severity === 'error') || list.find((d) => d.severity === 'warning');
    $('problem-peek').textContent = first ? `${first.message}${first.at ? ` · ${first.at.file}:${first.at.line}` : ''}` : '';
    const sb = $('sb-problems');
    sb.replaceChildren(h('span', { class: errors ? 'e' : '' }, `${errors}`), ` error${errors === 1 ? '' : 's'} · `, h('span', { class: warnings ? 'w' : '' }, `${warnings}`), ` warning${warnings === 1 ? '' : 's'}`);
  }

  async run() {
    if (!this.project) return;
    this.scheduleRun?.cancel();
    const pins = this.project.files.find((f) => isPins(f.name));
    const files = this.sources().map((f) => ({ name: f.name, text: f.text }));
    const dev = this.devices.find((d) => d.part === this.project.part);
    const req = {
      files,
      pins: pins ? pins.text : '',
      // The board's clock, for a top with a CLK_HZ parameter.
      clock_hz: dev?.clock_hz?.[this.project.package],
      part: this.project.part,
      package: this.project.package,
      top: this.project.top,
      seed: settings.get('seed'),
      freq: settings.get('freq'),
      timing_weight: settings.get('timingWeight'),
      effort: settings.get('effort'),
    };
    const t0 = performance.now();
    this.status('busy', 'Starting…');
    this.setRunBusy(true);
    const state = { family: dev?.family || 'iCE40', start: t0, p: null, steps: [] };
    const onProgress = (p) => {
      const now = performance.now();
      // Null when the run starts (it may have waited for the previous one).
      if (p === null) Object.assign(state, { start: now, p: null, steps: [] });
      else {
        if (p.step !== state.p?.step) state.steps.push([p.step, now]);
        state.p = p;
      }
      this.running = state;
      this.renderProgress();
    };
    try {
      const out = await this.backend.run(req, onProgress);
      if (this.running === state) this.running = null;
      if (out.ok && state.steps.length) this.learnSteps(state);
      this.resultTexts = Object.fromEntries(files.map((f) => [f.name, f.text]));
      this.showResult(out, performance.now() - t0);
    } catch (e) {
      if (e.superseded) return;
      this.status('error', e.message);
    } finally {
      if (this.running === state) this.running = null;
      if (!this.running) {
        // The line fills up, fades out, then goes back to the start.
        const line = $('progress-line');
        if (line.classList.contains('active')) line.style.width = '100%';
        line.classList.remove('active');
        setTimeout(() => { if (!this.running) line.style.width = '0'; }, 400);
      }
      if (!this.running) this.setRunBusy(false);
    }
  }

  /** The run button while a run goes: a spinner, the step and a bar. */
  setRunBusy(busy) {
    const split = $('run-split');
    split.classList.toggle('busy', busy);
    $('run').querySelector('use').setAttribute('href', busy ? '#i-spinner' : '#i-play');
    $('run').querySelector('span').textContent = busy ? 'Running' : 'Run';
    $('run').title = busy ? 'Run again with the latest changes' : `Run the whole flow (${MOD}+Enter)`;
    if (!busy) {
      this.progressView?.bar.remove();
      this.progressView = null;
      this.busyStep = null;
      this.renderRail();
    }
  }

  /** Learns the share of each step from a finished run, so that the next
   *  runs' bars move about evenly. */
  learnSteps(state) {
    const end = performance.now();
    const durations = {};
    state.steps.forEach(([step, at], i) => {
      durations[step] = (durations[step] || 0) + ((state.steps[i + 1]?.[1] ?? end) - at);
    });
    const key = `progress:${state.family}`;
    store.set(key, learnProgress(state.family, store.get(key, {}), durations));
  }

  /** The status bar during a run: a bar with a segment per step of the
   *  flow, the step, what it is doing and the time so far; and the whole
   *  run's progress as a line under the top bar. */
  renderProgress() {
    const state = this.running;
    state.learned ??= store.get(`progress:${state.family}`, {});
    const { segments, fraction } = progressLayout(state.family, state.p, state.learned);
    const split = $('run-split');
    // Built once per run, then updated, so that the bars move smoothly.
    if (this.progressView?.state !== state || !split.contains(this.progressView.bar)) {
      this.progressView?.bar.remove();
      const fills = segments.map(() => h('div', { class: 'fill' }));
      const bar = h('div', { class: 'progress' },
        segments.map((s, i) => h('div', { class: 'segment', style: { flexGrow: s.share }, title: s.label }, fills[i])));
      split.append(bar);
      this.progressView = { state, bar, fills };
    }
    const v = this.progressView;
    segments.forEach((s, i) => {
      v.fills[i].classList.toggle('indeterminate', s.fill === null);
      v.fills[i].style.width = `${100 * (s.fill ?? 1)}%`;
    });
    this.status('busy', describeProgress(state.p, performance.now() - state.start));
    const step = state.p ? (state.family === 'Artix-7' ? PROGRESS_STEP_XC7 : PROGRESS_STEP)[state.p.step] : 'elaborated';
    if (step !== this.busyStep) {
      this.busyStep = step;
      this.renderRail();
    }
    const line = $('progress-line');
    line.classList.add('active');
    line.style.width = `${100 * fraction}%`;
  }

  showResult(out, ms) {
    this.result = out;
    $('empty').hidden = true;
    $('download').disabled = !out.bin;
    $('program').disabled = !out.bin;
    const errors = (out.diagnostics || []).filter((d) => d.severity === 'error').length;
    const el = $('status');
    if (out.ok) {
      el.className = 'status ok';
      el.title = '';
      el.replaceChildren(h('span', { class: 'ok-ico' }, icon('check')),
        h('span', {}, 'Built in ', h('b', {}, ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(2)} s`), settings.get('runOnEdit') ? ' · auto' : ''));
    } else {
      // Where the flow stopped: the last step that has a result.
      const steps = flowSteps(out, this.device()?.family).flatMap((g) => g.steps);
      const reached = steps.filter((x) => x.metric && x.id !== 'summary').pop();
      const at = reached ? steps[steps.indexOf(reached) + 1] : steps[1];
      el.className = 'status error';
      el.title = out.error || '';
      el.replaceChildren(h('button', { class: 'err-pill', onclick: () => this.showProblems() }, icon('error'),
        `${errors ? `${errors} error${errors > 1 ? 's' : ''}` : 'Failed'}${at ? ` · stopped at ${at.label}` : ''}`));
    }
    this.lastRun = out;
    renderSummary($('pipeline'), out, this);
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
      if (out.ok) this.sim.designChanged(false);
    }
    if (!settings.get('liveCheck') || !this.analysis) this.applyAnalysis();
    if (!out.ok && errors) this.showProblems();
    this.renderRail();
    this.renderStatusBar();
  }

  downloadBitstream() {
    const r = this.result;
    if (r?.bin) download(r.bin_name || `${r.top || 'design'}.bin`, fromBase64(r.bin));
  }

  // ----- problems and the log -----

  setBottom(open, quiet) {
    $('bottom').classList.toggle('collapsed', !open);
    $('left').classList.toggle('bottom-collapsed', !open);
    store.set('bottomOpen', open);
    if (!quiet) this.editor?.measure?.();
  }

  showBottomTab(tab) {
    for (const x of $('bottom-tabs').querySelectorAll('button')) x.classList.toggle('active', x.dataset.tab === tab);
    for (const p of $('bottom-body').children) p.hidden = p.dataset.pane !== tab;
  }

  showProblems() {
    if (settings.get('layout') === 'hidden') this.toggleEditor();
    this.showBottomTab('problems');
    this.setBottom(true);
  }

  // ----- the status bar -----

  renderStatusBar() {
    const mode = $('sb-mode');
    mode.lastChild.textContent = this.backendMode === 'wasm' ? 'In your browser · WebAssembly' : 'On the local server';
    const r = this.good;
    const chip = r?.chip;
    $('sb-usage').textContent = chip ? `${fmtNum(chip.usage.lc)} / ${fmtNum(chip.usage.lc_total)} LC` : r?.placed ? `${fmtNum(r.placed.slices)} slices` : '';
    const clk = r?.timing?.clocks?.[0];
    const fmax = $('sb-fmax');
    fmax.textContent = clk ? `${clk.fmax_mhz.toFixed(1)} MHz${clk.target_mhz ? ` ${clk.met ? '≥' : '<'} ${clk.target_mhz} MHz` : ''}` : '';
    fmax.classList.toggle('bad', !!clk && !!clk.target_mhz && !clk.met);
    fmax.title = clk ? `Fmax of ${clk.clock}` : '';
  }

  renderCursor() {
    const ta = this.editor?.ta;
    if (!ta) return;
    const before = ta.value.slice(0, ta.selectionStart);
    const line = before.split('\n').length;
    const col = ta.selectionStart - before.lastIndexOf('\n');
    $('sb-cursor').textContent = `Ln ${line}, Col ${col}`;
  }

  /** Programs the board connected over USB with the last bitstream. */
  async program() {
    const r = this.result;
    if (!r?.bin || this.programming) return;
    if (!usbSupported()) {
      toast('Programming a board needs WebUSB: open the playground in Chrome, Edge or another Chromium-based browser');
      return;
    }
    const dev = this.devices.find((d) => d.part === this.project.part);
    const board = dev?.boards?.[this.project.package] || '';
    this.programming = true;
    let ftdi = null;
    try {
      ftdi = await requestFtdi();
      this.status('busy', `Programming ${board || ftdi.name}…`);
      const res = await programBoard(ftdi, r, board, fromBase64(r.bin), (stage, f) => this.status('busy', `${stage}… ${Math.round(100 * f)}%`));
      if (res.done) this.status('ok', `Programmed ${board || ftdi.name}: the FPGA is running ${r.top}`);
      else this.status('error', 'Programmed, but the FPGA did not start (DONE stayed low)');
    } catch (e) {
      if (e.message === 'no board chosen') this.status('idle', 'Programming cancelled');
      else {
        this.status('error', 'Programming failed');
        toast(`Programming failed: ${e.message}`, 15000);
      }
    } finally {
      await ftdi?.close().catch(() => {});
      this.programming = false;
    }
  }

  status(kind, text) {
    const el = $('status');
    el.className = `status ${kind}`;
    el.title = text;
    el.replaceChildren(h('span', { class: 'dot' }), h('span', {}, text));
  }

  // ----- views -----

  openView(name) {
    this.view = name;
    for (const p of document.querySelectorAll('[data-view-pane]')) p.hidden = p.dataset.viewPane !== name;
    const mode = name === 'simulation' ? 'simulate' : 'flow';
    if (mode !== this.mode) {
      this.mode = mode;
      this.renderRail();
    }
    if (name === 'chip') {
      this.step = this.step === 'route' ? 'route' : 'place';
      this.chip.setStep(this.step);
    } else if (name === 'timing') this.step = 'timing';
    else if (name === 'bitstream') this.step = 'bitstream';
    else if (name === 'pipeline' && !['pnr', 'bitstream'].includes(this.step)) this.step = 'summary';
    this.markRail();
    this.visible();
  }

  /** The run the rail and the views show: the last one, or while a design
   *  does not elaborate, the last good one (`stale`). */
  shownRun() {
    const r = this.running ? this.lastRun : this.result;
    if (r && !r.ok && !(r.stages || []).length && this.good) return { run: this.good, stale: true };
    return { run: r, stale: false };
  }

  /** Opens a step of the flow: its view, at its stage for the schematic. */
  openStep(id) {
    const step = flowSteps(this.shownRun().run, this.device()?.family).flatMap((g) => g.steps).find((x) => x.id === id);
    if (!step) return;
    this.step = id;
    if (step.view === 'schematic') {
      this.openView('schematic');
      this.schematic.setStage(id);
    } else this.openView(step.view);
    this.step = id;
    this.markRail();
  }

  setMode(mode) {
    if (mode === 'simulate') this.openView('simulation');
    else this.openStep(this.step || 'summary');
  }

  /** The rail: Flow or Simulate, then the steps of the flow with their
   *  headline numbers. */
  renderRail() {
    for (const b of $('mode').querySelectorAll('button')) {
      b.classList.toggle('on', b.dataset.mode === this.mode);
      b.setAttribute('aria-selected', b.dataset.mode === this.mode ? 'true' : 'false');
    }
    const host = $('rail-steps');
    if (this.mode === 'simulate') {
      host.replaceChildren(h('span', { class: 'muted', style: { fontSize: '12px' } }, 'The design running on a virtual board: its clock, LEDs, buttons and serial port.'));
      return;
    }
    const { run, stale } = this.shownRun();
    const groups = flowSteps(run, this.device()?.family);
    if (stale) Object.assign(groups[0].steps[0], { metric: 'failed', bad: true });
    let reached = true;
    host.replaceChildren(...groups.map((g) => h('div', { class: 'rail-group' },
      h('span', { class: 'h4' }, g.title),
      h('div', {}, g.steps.flatMap((st, i) => {
        const busy = this.running && st.id === this.busyStep;
        const pending = !st.metric || (this.running && !reached);
        if (busy) reached = false;
        const delta = st.delta ? h('span', { class: st.delta < 0 ? 'down' : 'up' }, ` ${st.delta < 0 ? '−' : '+'}${fmtNum(Math.abs(st.delta))}`) : null;
        const btn = h('button', {
          class: `step${pending && !busy ? ' pending' : ''}${busy ? ' busy' : ''}${(this.running || stale) && !busy && st.id !== 'summary' ? ' stale' : ''}${st.bad ? ' bad' : ''}`, 'data-step': st.id,
          title: `${stageInfo(st.id)[1] || st.label}${st.view === 'schematic' && st.metric && !st.has ? ' (too large to draw)' : ''}`,
          disabled: !st.metric || (st.view === 'schematic' && !st.has),
          onclick: () => this.openStep(st.id),
        }, h('span', { class: 'l' }, st.label), h('span', { class: 'm' }, busy ? 'running' : st.metric ?? '—', busy ? null : delta));
        return i ? [h('span', { class: 'rail-sep', 'aria-hidden': 'true' }), btn] : [btn];
      })))));
    this.markRail();
  }

  markRail() {
    for (const b of $('rail-steps').querySelectorAll('.step')) {
      const on = b.dataset.step === this.step && this.mode === 'flow';
      b.classList.toggle('on', on);
      if (on) {
        b.setAttribute('aria-current', 'step');
        b.scrollIntoView({ block: 'nearest', inline: 'nearest' });
      } else b.removeAttribute('aria-current');
    }
  }

  // ----- the palette -----

  openPalette() {
    const steps = flowSteps(this.shownRun().run, this.device()?.family).flatMap((g) => g.steps).filter((x) => x.metric && (x.view !== 'schematic' || x.has));
    const theme = settings.get('theme');
    const actions = [
      ['Run the flow', 'play', `${MOD} ↵`, () => this.run()],
      ['Simulate the design', 'wave', null, () => this.setMode('simulate')],
      ['Format the file', 'format', 'Shift Alt F', () => this.format()],
      [settings.get('layout') === 'hidden' ? 'Show the editor' : 'Hide the editor', 'panel', `${MOD} B`, () => this.toggleEditor()],
      ['Show problems', 'warning', null, () => this.showProblems()],
      ['Show the log', 'file', null, () => { this.showProblems(); this.showBottomTab('log'); }],
      ['Copy a link to this design', 'link', null, () => this.share()],
      this.result?.bin ? [`Download ${this.result.bin_name || 'the bitstream'}`, 'download', null, () => this.downloadBitstream()] : null,
      this.result?.bin ? ['Program the board over USB', 'usb', null, () => this.program()] : null,
      this.result?.timing?.clocks?.length && this.result.chip ? ['Show the critical path on the chip', 'clock', null, () => this.showCriticalPath(0)] : null,
      [theme === 'dark' ? 'Switch to the light theme' : 'Switch to the dark theme', theme === 'dark' ? 'sun' : 'moon', null, () => settings.set('theme', theme === 'dark' ? 'light' : 'dark')],
      ['Open the settings', 'gear', null, () => openSettings()],
      ['Change the board', 'board', null, () => this.openBoards($('board'))],
    ].filter(Boolean);
    // The cells and signals of the schematic's stage, once the query is
    // long enough to be specific.
    const cells = () => {
      const g = this.schematic.stage?.graph;
      if (!g) return [];
      const seen = new Set();
      return g.nodes.filter((n) => n.name && !seen.has(n.name) && seen.add(n.name)).map((n) => ({
        group: `Cells · ${stageInfo(this.schematic.stage.id)[0]}`, kind: n.t || n.k || 'cell', title: n.name,
        run: () => { this.openStep(this.schematic.stage.id); this.schematic.find(n.name); },
      }));
    };
    openList({
      placeholder: 'Jump to a step, file, cell, example or action',
      items: (q) => {
        const out = [
          ...steps.map((x) => ({ group: 'Steps', ico: x.view === 'schematic' ? 'graph' : x.view === 'chip' ? 'chip' : x.view === 'timing' ? 'clock' : x.view === 'bitstream' ? 'bits' : 'flow', title: x.label, detail: x.metric, keywords: stageInfo(x.id)[1], run: () => this.openStep(x.id) })),
          ...this.project.files.map((f) => ({ group: 'Files', ico: 'file', title: f.name, run: () => { if (settings.get('layout') === 'hidden') this.toggleEditor(); this.openFile(f.name); } })),
          ...actions.map(([title, ico, kbd, run]) => ({ group: 'Actions', ico, title, kbd, run })),
          ...this.examples.map((e) => ({ group: 'Examples', ico: 'chip', title: e.title, detail: e.description, run: () => this.loadExample(e) })),
        ].filter((it) => matches(it, q));
        if (q.length >= 2) out.push(...cells().filter((it) => it.title.toLowerCase().includes(q)).slice(0, 30));
        return out;
      },
    });
  }

  visible() {
    requestAnimationFrame(() => {
      if (this.view === 'schematic') this.schematic.visible();
      if (this.view === 'chip') this.chip.visible();
      if (this.view === 'bitstream') this.bits.visible();
      if (this.view === 'simulation') this.sim.visible();
    });
  }

  openSchematic(stage) {
    this.openStep(stage);
  }

  showBits(x, y) {
    this.openView('bitstream');
    this.bits.select(x, y);
  }

  showCriticalPath(ci) {
    this.step = 'route';
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
  el.title = e.message;
});

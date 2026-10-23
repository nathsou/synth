// User settings: defaults, persistence, application to the page and the
// settings dialog.

import { h, icon, store, Emitter } from './util.js';

export const ACCENTS = ['#5ed3b3', '#8db4ff', '#7aa2ff', '#f2b54b', '#ff8a65', '#ff6b9a', '#b18cff'];

const DEFAULTS = {
  theme: 'dark',
  accent: '#5ed3b3',
  layout: 'left', // editor position: left, right or hidden
  codeSize: 13,
  // Off by default: a long flow on every pause in typing gets in the way.
  // (A new key, so the old default saved in browsers does not carry over.)
  runOnEdit: false,
  liveCheck: true,
  // Run options.
  seed: 1,
  freq: 0,
  timingWeight: 0.5,
  effort: 1,
  // Schematic.
  schDepth: 0,
  schEdges: 'curved',
  schPortNames: true,
  schNetLabels: true,
  schMaxNodes: 2500,
  // Stages as text rather than as graphs.
  schText: false,
  colLogic: '#3b4a6b',
  colReg: '#5b3f72',
  colMux: '#2f5d62',
  colMem: '#6b4f2a',
  colInst: '#33415c',
  colPort: '#2d5a3d',
  // Chip.
  chipColorBy: 'net',
  chipRouting: true,
  chipGlobals: false,
  chipPins: true,
  chipUnusedDim: true,
  chipWireAlpha: 0.85,
  colTileLogic: '#1f2433',
  colTileRam: '#2a2433',
  colTileIo: '#1d2b2b',
  colLc: '#4a6fd8',
  colFf: '#c46edc',
  colCarry: '#e0a03c',
  colIo: '#3fbf8a',
  colRam: '#d9774a',
  colSpan4: '#5aa9ff',
  colSpan12: '#ff7ab6',
  colLocal: '#9be36b',
  colGlobal: '#ffd166',
};

class Settings extends Emitter {
  constructor() {
    super();
    this.v = { ...DEFAULTS, ...store.get('settings', {}) };
  }
  get(k) { return this.v[k]; }
  set(k, val) {
    if (this.v[k] === val) return;
    this.v[k] = val;
    store.set('settings', this.v);
    apply(this.v);
    this.emit('change', k, val);
  }
  reset() {
    this.v = { ...DEFAULTS };
    store.set('settings', this.v);
    apply(this.v);
    for (const k of Object.keys(DEFAULTS)) this.emit('change', k, this.v[k]);
  }
}

export const settings = new Settings();

function apply(v) {
  const root = document.documentElement;
  root.dataset.theme = v.theme;
  root.style.setProperty('--accent', v.accent);
  root.style.setProperty('--code-size', `${v.codeSize}px`);
  const main = document.getElementById('main');
  if (main) {
    main.classList.toggle('editor-right', v.layout === 'right');
    main.classList.toggle('editor-hidden', v.layout === 'hidden');
  }
}
apply(settings.v);

// ----- controls -----

export function toggle(key) {
  const input = h('input', { type: 'checkbox', checked: settings.get(key), onchange: (e) => settings.set(key, e.target.checked) });
  return h('label', { class: 'switch' }, input, h('span'));
}

function seg(key, options) {
  const wrap = h('div', { class: 'seg' });
  const render = () => {
    wrap.replaceChildren(
      ...options.map(([val, label]) =>
        h('button', { class: settings.get(key) === val ? 'on' : '', onclick: () => { settings.set(key, val); render(); } }, label)
      )
    );
  };
  render();
  return wrap;
}

function range(key, min, max, step, fmt = (x) => x) {
  const out = h('span', { class: 'muted mono', style: { minWidth: '42px', textAlign: 'right' } }, fmt(settings.get(key)));
  const input = h('input', {
    type: 'range', min, max, step, value: settings.get(key),
    oninput: (e) => { const x = Number(e.target.value); out.textContent = fmt(x); settings.set(key, x); },
  });
  return h('span', { style: { display: 'flex', gap: '8px', alignItems: 'center' } }, input, out);
}

function color(key) {
  return h('input', { type: 'color', value: settings.get(key), oninput: (e) => settings.set(key, e.target.value) });
}

function number(key, min, max) {
  return h('input', {
    type: 'number', min, max, value: settings.get(key), style: { width: '90px' },
    onchange: (e) => settings.set(key, Number(e.target.value)),
  });
}

function row(label, hint, control) {
  return h('div', { class: 'row' }, h('div', { class: 'lbl' }, label, hint ? h('small', {}, hint) : null), control);
}

function accentPicker() {
  const wrap = h('div', { class: 'swatches' });
  const render = () => {
    wrap.replaceChildren(
      ...ACCENTS.map((c) =>
        h('button', { class: settings.get('accent') === c ? 'on' : '', style: { background: c }, title: c, onclick: () => { settings.set('accent', c); render(); } })
      ),
      color('accent')
    );
  };
  render();
  return wrap;
}

const SECTIONS = [
  ['Appearance', () => [
    row('Theme', null, seg('theme', [['dark', 'Dark'], ['light', 'Light']])),
    row('Accent colour', null, accentPicker()),
    row('Editor position', 'Where the source editor sits', seg('layout', [['left', 'Left'], ['right', 'Right'], ['hidden', 'Hidden']])),
    row('Code font size', null, range('codeSize', 10, 20, 1, (x) => `${x}px`)),
  ]],
  ['Flow', () => [
    row('Run after edits', 'Re-run the flow shortly after you stop typing', toggle('runOnEdit')),
    row('Check as you type', 'Diagnostics, hover and go to definition in the editor', toggle('liveCheck')),
    row('Placement seed', null, number('seed', 0, 1e9)),
    row('Target frequency', 'MHz, for clocks without `set_frequency` (0: none)', number('freq', 0, 1000)),
    row('Timing weight', 'Placement: 0 = wire length only, 1 = timing first', range('timingWeight', 0, 1, 0.05, (x) => x.toFixed(2))),
    row('Placement effort', null, range('effort', 0.25, 4, 0.25, (x) => `${x}×`)),
  ]],
  ['Schematic', () => [
    row('Hierarchy depth', 'Instances deeper than this start collapsed', range('schDepth', 0, 6, 1)),
    row('Wires', null, seg('schEdges', [['curved', 'Curved'], ['orthogonal', 'Orthogonal'], ['straight', 'Straight']])),
    row('Port names', null, toggle('schPortNames')),
    row('Signal names', 'On wires, when zoomed in', toggle('schNetLabels')),
    row('Node limit', 'Larger graphs collapse into instances', number('schMaxNodes', 200, 20000)),
    h('h3', {}, 'Node colours'),
    row('Logic', null, color('colLogic')),
    row('Registers', null, color('colReg')),
    row('Multiplexers', null, color('colMux')),
    row('Memories', null, color('colMem')),
    row('Instances', null, color('colInst')),
    row('Ports', null, color('colPort')),
  ]],
  ['Chip', () => [
    row('Colour wires by', null, seg('chipColorBy', [['net', 'Net'], ['crit', 'Criticality'], ['class', 'Wire type']])),
    row('Routing', null, toggle('chipRouting')),
    row('Global networks', 'Clock and reset distribution', toggle('chipGlobals')),
    row('Package pins', null, toggle('chipPins')),
    row('Dim unused tiles', null, toggle('chipUnusedDim')),
    row('Wire opacity', null, range('chipWireAlpha', 0.2, 1, 0.05, (x) => x.toFixed(2))),
    h('h3', {}, 'Tiles'),
    row('Logic tiles', null, color('colTileLogic')),
    row('RAM tiles', null, color('colTileRam')),
    row('IO tiles', null, color('colTileIo')),
    h('h3', {}, 'Cells'),
    row('LUT', null, color('colLc')),
    row('Flip-flop', null, color('colFf')),
    row('Carry', null, color('colCarry')),
    row('IO', null, color('colIo')),
    row('Block RAM', null, color('colRam')),
    h('h3', {}, 'Wires (by type)'),
    row('Span 4', null, color('colSpan4')),
    row('Span 12', null, color('colSpan12')),
    row('Local', null, color('colLocal')),
    row('Global', null, color('colGlobal')),
  ]],
  ['Shortcuts', () => {
    const mod = navigator.platform.includes('Mac') ? '⌘' : 'Ctrl';
    const keys = [
      [`${mod} Enter`, 'Run the flow'],
      [`${mod} S`, 'Run the flow'],
      [`${mod} K`, 'Jump to a step, file, cell, example or action'],
      [`${mod} B`, 'Show or hide the editor'],
      [`${mod} click`, 'Go to the definition of a name'],
      [`${mod} Space`, 'Complete a name'],
      ['.', 'Complete ports, fields and variants'],
      [`${mod} /`, 'Comment or uncomment lines'],
      ['Shift Alt F', 'Format the file'],
      ['Tab / Shift Tab', 'Indent or dedent'],
      ['F', 'Fit the schematic or the chip to the view'],
      ['Esc', 'Clear the selection'],
      ['Double-click', 'Expand or collapse an instance in the schematic'],
      ['Drag, wheel, pinch', 'Pan and zoom'],
    ];
    return keys.map(([k, what]) => row(what, null, h('kbd', {}, k)));
  }],
];

export function openSettings() {
  let active = 0;
  const body = h('div', { class: 'form' });
  const nav = h('nav');
  const render = () => {
    nav.replaceChildren(...SECTIONS.map(([name], i) => h('a', { class: i === active ? 'active' : '', onclick: () => { active = i; render(); } }, name)));
    body.replaceChildren(h('h3', {}, SECTIONS[active][0]), ...SECTIONS[active][1]());
  };
  render();
  const close = () => { back.remove(); document.removeEventListener('keydown', onKey); };
  const onKey = (e) => { if (e.key === 'Escape') close(); };
  const back = h('div', { class: 'modal-back', onclick: (e) => { if (e.target === back) close(); } },
    h('div', { class: 'modal', role: 'dialog', 'aria-label': 'Settings' },
      h('header', {}, h('h2', {}, 'Settings'), h('button', { class: 'icon-btn', onclick: close, title: 'Close' }, icon('x'))),
      h('div', { class: 'body' }, nav, body),
      h('footer', {},
        h('button', { onclick: () => { settings.reset(); render(); } }, 'Reset to defaults'),
        h('button', { class: 'primary', onclick: close }, 'Done'))));
  document.body.append(back);
  document.addEventListener('keydown', onKey);
}

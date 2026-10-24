// The text of a stage: its IR in the IR language of its level (`rtl`,
// `gates` or `netlist`, see docs/IR_TEXT.md), highlighted. Only the visible
// lines are in the page (a stage can have tens of thousands).

import { h } from './util.js';
import { highlightHtml } from './highlight.js';

const LH = 19;
const MARGIN = 30;

export class TextView {
  constructor() {
    this.el = h('div', { class: 'irtext', hidden: true });
    this.gutter = h('div', { class: 'gutter' });
    this.code = h('pre', { class: 'code' });
    this.win = h('div', { class: 'win' }, this.gutter, this.code);
    this.sizer = h('div', { class: 'sizer' }, this.win);
    this.scroller = h('div', { class: 'scroller' }, this.sizer);
    this.el.append(this.scroller);
    this.text = null;
    this.lines = [];
    this.range = null;
    this.scroller.addEventListener('scroll', () => {
      if (this.frame) return;
      this.frame = requestAnimationFrame(() => {
        this.frame = 0;
        this.render();
      });
    });
  }

  setText(text, lang) {
    if (text === this.text && lang === this.lang) return;
    this.text = text;
    this.lang = lang;
    this.lines = (text || '').replace(/\n$/, '').split('\n');
    this.lower = null;
    this.hit = -1;
    this.query = '';
    const longest = this.lines.reduce((m, l) => Math.max(m, l.length), 0);
    const digits = String(this.lines.length).length;
    this.el.style.setProperty('--digits', `${digits}ch`);
    this.sizer.style.height = `${this.lines.length * LH + 16}px`;
    this.sizer.style.minWidth = `calc(${longest + digits}ch + 64px)`;
    this.scroller.scrollTop = 0;
    this.scroller.scrollLeft = 0;
    this.render(true);
  }

  render(force) {
    if (this.el.hidden) return;
    const top = this.scroller.scrollTop;
    const first = Math.max(0, Math.floor(top / LH) - MARGIN);
    const last = Math.min(this.lines.length, Math.ceil((top + this.scroller.clientHeight) / LH) + MARGIN);
    if (!force && this.range && this.range[0] === first && this.range[1] === last) return;
    this.range = [first, last];
    this.win.style.transform = `translateY(${first * LH}px)`;
    let nums = '', code = '';
    const q = this.query;
    for (let i = first; i < last; i++) {
      const line = this.lines[i];
      const cls = i === this.hit ? ' class="hit"' : q && line.toLowerCase().includes(q) ? ' class="match"' : '';
      nums += `<div${cls}>${i + 1}</div>`;
      code += `<div${cls}>${line ? highlightHtml(line, this.lang) : ' '}</div>`;
    }
    this.gutter.innerHTML = nums;
    this.code.innerHTML = code;
  }

  /** Scrolls to the next line containing `q` (case-insensitive); false
   *  when no line does. */
  find(q) {
    q = q.toLowerCase();
    if (!q) return false;
    this.lower = this.lower || this.lines.map((l) => l.toLowerCase());
    const n = this.lines.length;
    const start = this.query === q && this.hit >= 0 ? this.hit + 1 : 0;
    for (let k = 0; k < n; k++) {
      const i = (start + k) % n;
      if (this.lower[i].includes(q)) {
        this.query = q;
        this.hit = i;
        this.scroller.scrollTop = Math.max(0, i * LH - this.scroller.clientHeight / 3);
        this.render(true);
        return true;
      }
    }
    return false;
  }
}

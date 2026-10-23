// Searchable lists: the menus of the top bar (examples, boards) and the
// command palette. Arrow keys move, Enter picks, Escape closes.

import { h, icon } from './util.js';

let open = null;

function checkIcon() {
  const s = icon('check');
  s.classList.add('check');
  return s;
}

/** Closes the open menu or palette, if any. */
export function closeMenu() {
  if (!open) return;
  const o = open;
  open = null;
  o.close();
}

/**
 * Opens a list under `anchor`, or as a palette in the middle of the page
 * when `anchor` is null. `items(query)` gives the entries: {group, title,
 * detail, tag, kbd, ico, check, kind, run}. `foot` is an element shown under
 * the list.
 */
export function openList({ anchor = null, placeholder = 'Search', items, foot = null, width }) {
  const again = open?.anchor === anchor && anchor;
  closeMenu();
  if (again) return;
  const input = h('input', { type: 'text', placeholder, 'aria-label': placeholder, autocomplete: 'off', spellcheck: false });
  const area = h('div', { class: 'list-area', role: 'listbox' });
  const menu = h('div', { class: `menu${anchor ? '' : ' palette'}`, role: 'dialog' },
    h('div', { class: 'search' }, anchor ? h('label', {}, icon('search'), input) : [icon('search'), input, h('kbd', {}, 'esc')]),
    area, foot);
  let shown = [], sel = 0;
  const render = () => {
    shown = items(input.value.trim().toLowerCase());
    sel = Math.max(0, Math.min(sel, shown.length - 1));
    const rows = [];
    let group = null;
    shown.forEach((it, i) => {
      if (it.group && it.group !== group) rows.push(h('div', { class: 'grp' }, (group = it.group)));
      rows.push(h('button', {
        class: `opt${i === sel ? ' sel' : ''}`, role: 'option', 'aria-selected': i === sel ? 'true' : 'false',
        onclick: () => pick(i),
        onpointermove: () => { if (sel !== i) { sel = i; mark(); } },
      },
      it.kind ? h('span', { class: 'k' }, it.kind) : it.ico ? h('span', { class: 'ico' }, icon(it.ico)) : null,
      h('span', { class: 'tx' }, h('span', { class: 't' }, it.title), it.detail ? h('span', { class: 'd' }, it.detail) : null),
      it.tag ? h('span', { class: 'tag' }, it.tag) : null,
      it.check ? checkIcon() : null,
      it.kbd ? h('kbd', {}, it.kbd) : null));
    });
    if (!shown.length) rows.push(h('div', { class: 'empty' }, 'Nothing matches.'));
    area.replaceChildren(...rows);
  };
  const mark = () => {
    area.querySelectorAll('.opt').forEach((b, i) => {
      b.classList.toggle('sel', i === sel);
      b.setAttribute('aria-selected', i === sel ? 'true' : 'false');
    });
    area.querySelectorAll('.opt')[sel]?.scrollIntoView({ block: 'nearest' });
  };
  const pick = (i) => {
    const it = shown[i];
    if (!it) return;
    closeMenu();
    it.run?.();
  };
  input.addEventListener('input', () => { sel = 0; render(); });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      sel = (sel + (e.key === 'ArrowDown' ? 1 : -1) + shown.length) % Math.max(1, shown.length);
      mark();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      pick(sel);
    }
  });
  const onKey = (e) => { if (e.key === 'Escape') { e.preventDefault(); closeMenu(); } };
  const onDown = (e) => { if (!menu.contains(e.target) && !(anchor && anchor.contains(e.target))) closeMenu(); };
  let host = menu;
  if (anchor) {
    const r = anchor.getBoundingClientRect();
    const w = width || 400;
    menu.style.width = `${w}px`;
    menu.style.left = `${Math.max(8, Math.min(window.innerWidth - w - 8, r.left))}px`;
    menu.style.top = `${r.bottom + 6}px`;
    anchor.setAttribute('aria-expanded', 'true');
  } else {
    host = h('div', { class: 'palette-back' }, menu);
  }
  document.body.append(host);
  document.addEventListener('keydown', onKey, true);
  setTimeout(() => document.addEventListener('pointerdown', onDown), 0);
  open = {
    anchor,
    close: () => {
      host.remove();
      anchor?.setAttribute('aria-expanded', 'false');
      document.removeEventListener('keydown', onKey, true);
      document.removeEventListener('pointerdown', onDown);
    },
  };
  render();
  input.focus();
}

/** Keeps the entries whose title, detail or group contain every word of `q`. */
export function matches(it, q) {
  if (!q) return true;
  const hay = `${it.title} ${it.detail || ''} ${it.group || ''} ${it.keywords || ''}`.toLowerCase();
  return q.split(/\s+/).every((w) => hay.includes(w));
}

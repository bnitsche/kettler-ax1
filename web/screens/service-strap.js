// Service page, tab "Brustgurt": registered chest straps, search and registration.

import { el } from '../lib/dom.js';
import { api } from '../lib/api.js';
import { strapStateText } from '../lib/pulse.js';
import { bluetoothText } from '../lib/service-format.js';
import { openConfirmDialog } from './confirm-dialog.js';

const POLL_MS = 3000;

export function strapTab() {
  const availability = el('p', { class: 'muted' });
  const setupHint = el('p', { class: 'muted', hidden: true }, 'Einrichtung siehe README, Abschnitt „Brustgurt (Bluetooth)“.');
  const list = el('div', { class: 'strap-list' });
  const error = el('p', { class: 'error', role: 'alert', hidden: true });
  const status = el('p', { class: 'muted', role: 'status' });
  const scanBtn = el('button', { type: 'button', class: 'btn primary', onclick: () => scan() }, 'Gurt suchen');
  const found = el('div', { class: 'strap-list' });
  const searchPart = el('div', {},
    el('h2', { class: 'service-gap' }, 'Neuen Gurt anmelden'),
    el('p', {}, 'Gurt anlegen und die Elektroden anfeuchten, dann suchen. Der Server verbindet sich nur mit angemeldeten Gurten.'),
    el('div', { class: 'service-actions' }, scanBtn),
    found);
  const panel = el('section', { class: 'panel tab-panel', role: 'tabpanel' },
    el('h2', {}, 'Brustgurte'),
    availability,
    setupHint,
    list,
    searchPart,
    error,
    status);

  let timer = null;
  let running = false;
  let usable = false;

  function show(msg, isError) {
    error.textContent = isError ? msg : '';
    error.hidden = !isError;
    status.textContent = isError ? '' : msg;
  }

  function strapRow(s) {
    return el('div', { class: 'strap-row' },
      el('div', { class: 'strap-info' },
        el('b', {}, s.name), ' ',
        el('span', { class: 'mono muted' }, s.address),
        el('div', { class: `strap-state ${s.link}` }, strapStateText(s))),
      el('div', { class: 'strap-actions' },
        el('button', { type: 'button', class: 'btn small', onclick: () => rename(s) }, 'Umbenennen'),
        el('button', { type: 'button', class: 'btn small danger', onclick: () => remove(s) }, 'Entfernen')));
  }

  function render(data) {
    const a = data.availability;
    usable = a.state === 'ready' || a.state === 'sim';
    availability.textContent = `Bluetooth: ${bluetoothText(a)}`;
    setupHint.hidden = usable;
    searchPart.hidden = !usable;
    list.replaceChildren(...(data.straps.length
      ? data.straps.map(strapRow)
      : [el('p', { class: 'muted' }, 'Noch kein Gurt angemeldet.')]));
  }

  async function refresh() {
    try {
      render(await api('/api/straps'));
      if (!error.textContent) error.hidden = true;
    } catch (e) {
      show(e.message, true);
    }
  }

  // The next poll is set after the answer, so requests never overlap.
  async function poll() {
    await refresh();
    if (running) timer = setTimeout(poll, POLL_MS);
  }

  async function scan() {
    scanBtn.disabled = true;
    found.replaceChildren();
    show('Suche läuft (bis 15 s) …');
    try {
      const results = await api('/api/straps/scan', { method: 'POST' });
      show(results.length ? '' : 'Kein Gurt gefunden. Ist er angelegt und nicht mit einem Handy verbunden?');
      found.replaceChildren(...results.map((f) => el('div', { class: 'strap-row' },
        el('div', { class: 'strap-info' },
          el('b', {}, f.name || 'Ohne Namen'), ' ',
          el('span', { class: 'mono muted' }, f.address),
          f.rssi != null ? el('div', { class: 'muted' }, `Signal ${f.rssi} dBm`) : ''),
        f.known
          ? el('span', { class: 'muted' }, 'angemeldet')
          : el('button', { type: 'button', class: 'btn small', onclick: () => register(f) }, 'Anmelden'))));
    } catch (e) {
      show(e.message, true);
    } finally {
      scanBtn.disabled = false;
    }
  }

  async function register(f) {
    const name = prompt('Name des Gurts', f.name || 'Brustgurt');
    if (name == null) return;
    try {
      await api('/api/straps', { method: 'POST', body: { address: f.address, name } });
      found.replaceChildren();
      show(`„${name.trim()}“ angemeldet. Beim Fahrer unter „Profil“ zuordnen, wenn es mehrere Gurte gibt.`);
      refresh();
    } catch (e) {
      show(e.message, true);
    }
  }

  async function rename(s) {
    const name = prompt('Neuer Name', s.name);
    if (name == null || name.trim() === s.name) return;
    try {
      await api(`/api/straps/${s.id}`, { method: 'PUT', body: { name } });
      show('');
      refresh();
    } catch (e) {
      show(e.message, true);
    }
  }

  function remove(s) {
    openConfirmDialog({
      title: `${s.name} entfernen`,
      text: 'Der Server verbindet sich nicht mehr mit diesem Gurt, und Fahrer verlieren die Zuordnung. Aufgezeichnete Pulswerte bleiben erhalten.',
      confirmLabel: 'Entfernen',
      onConfirm: async () => {
        await api(`/api/straps/${s.id}`, { method: 'DELETE' });
        show('');
        refresh();
      },
    });
  }

  return {
    panel,
    start() {
      running = true;
      poll();
    },
    stop() {
      running = false;
      clearTimeout(timer);
      timer = null;
    },
  };
}

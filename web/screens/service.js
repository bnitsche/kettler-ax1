// Service page (#/service, not in the menu): device state, live device data, chest straps, backup.

import { el } from '../lib/dom.js';
import { api } from '../lib/api.js';
import * as live from '../lib/live.js';
import { mmss } from '../lib/format.js';
import { backupName, restoreText } from '../lib/backup-name.js';
import { durationText, bytesText, traceLine, keysText, countText, bluetoothText } from '../lib/service-format.js';
import { openConfirmDialog } from './confirm-dialog.js';
import { strapTab } from './service-strap.js';

const TAB_KEY = 'kettler.serviceTab';
const STATUS_MS = 3000;
const RETRY_MS = 2000;
const MAX_LINES = 200;

/** Sends a file as the raw request body; returns the JSON reply or throws with the server's message. */
async function postFile(path, file) {
  let resp;
  try {
    resp = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/octet-stream' }, body: file });
  } catch {
    throw new Error('Server nicht erreichbar');
  }
  let data = null;
  try {
    data = await resp.json();
  } catch {
    data = null;
  }
  if (!resp.ok) throw new Error((data && data.error) || `Fehler ${resp.status}`);
  return data;
}

function loadTab() {
  try {
    return localStorage.getItem(TAB_KEY) || 'state';
  } catch {
    return 'state';
  }
}

function saveTab(tab) {
  try {
    localStorage.setItem(TAB_KEY, tab);
  } catch {
    // private mode: the tab is just not remembered
  }
}

export function mount(root) {
  const tabs = [
    { id: 'state', label: 'Zustand', ...stateTab() },
    { id: 'live', label: 'Live', ...liveTab() },
    { id: 'strap', label: 'Brustgurt', ...strapTab() },
    { id: 'backup', label: 'Backup', ...backupTab() },
  ];
  let active = null;
  const bar = el('div', { class: 'tabs', role: 'tablist' });
  const holder = el('div');
  for (const t of tabs) {
    t.button = el('button', { type: 'button', class: 'tab', role: 'tab', 'aria-selected': 'false', onclick: () => show(t.id) }, t.label);
    bar.append(t.button);
  }
  root.append(el('section', { class: 'page service' }, el('h1', {}, 'Service'), bar, holder));

  function show(id) {
    const next = tabs.find((t) => t.id === id) || tabs[0];
    if (active === next) return;
    if (active) active.stop();
    active = next;
    for (const t of tabs) {
      t.button.classList.toggle('active', t === next);
      t.button.setAttribute('aria-selected', t === next ? 'true' : 'false');
    }
    holder.replaceChildren(next.panel);
    next.start();
    saveTab(next.id);
  }

  show(loadTab());
  return () => {
    if (active) active.stop();
  };
}

// ---- Zustand ------------------------------------------------------------------------------

function stateTab() {
  const rows = el('dl', { class: 'service-grid' });
  const error = el('p', { class: 'error', role: 'alert', hidden: true });
  const panel = el('section', { class: 'panel tab-panel', role: 'tabpanel' }, rows, error);
  let timer = null;
  let names = new Map();

  function row(label, value, cls) {
    return [el('dt', {}, label), el('dd', cls ? { class: cls } : {}, value)];
  }

  function rideText() {
    const ride = live.current()?.ride;
    if (!ride) return 'keine';
    const name = names.get(ride.person_id) || `Fahrer ${ride.person_id}`;
    return `läuft · ${name} · ${mmss(ride.session.ride_secs)}`;
  }

  function render(s) {
    const now = Date.now();
    const d = s.device;
    const since = d.since_ms ? ` · seit ${durationText(now - d.since_ms)}` : '';
    const lastDrop = d.last_disconnect
      ? `${new Date(d.last_disconnect.at_ms).toLocaleString('de-DE')} · ${d.last_disconnect.reason}`
      : 'keiner';
    const adapter = s.adapter
      ? [s.adapter.manufacturer, s.adapter.product, s.adapter.serial && `Seriennummer ${s.adapter.serial}`].filter(Boolean).join(' · ')
      : null;
    const view = live.current();
    rows.replaceChildren(
      ...row('Gerät', d.connected ? `● verbunden${d.id ? ` · ${d.id}` : ''}${since}` : `● getrennt${since}`, d.connected ? 'ok' : 'bad'),
      ...row('Firmware', d.firmware || 'unbekannt'),
      ...row('Verbindung', s.device_url, 'mono'),
      ...(adapter ? row('Adapter', adapter) : []),
      ...row('Letzter Abbruch', lastDrop),
      ...row('Tasten', keysText(s.keys, Boolean(view && view.keypad))),
      ...row('Bluetooth', bluetoothText(s.bluetooth)),
      ...row('Fahrt', rideText()),
      ...row('Server', `Version ${s.server.version} (${s.server.commit}) · läuft seit ${durationText(now - s.server.started_at_ms)}`),
      ...row('Datenbank', `${bytesText(s.db.size_bytes)} · Schema v${s.db.schema_version} · ${countText(s.db.persons, 'Fahrer', 'Fahrer')} · ${countText(s.db.programs, 'Programm', 'Programme')} · ${countText(s.db.rides, 'Fahrt', 'Fahrten')}`),
      ...row('Datei', s.db.path, 'mono'),
    );
  }

  async function refresh() {
    try {
      const [status, persons] = await Promise.all([api('/api/service/status'), api('/api/persons')]);
      names = new Map(persons.map((p) => [p.id, p.name]));
      error.hidden = true;
      render(status);
    } catch (e) {
      error.textContent = e.message;
      error.hidden = false;
    }
  }

  return {
    panel,
    start() {
      refresh();
      timer = setInterval(refresh, STATUS_MS);
    },
    stop() {
      clearInterval(timer);
      timer = null;
    },
  };
}

// ---- Live ---------------------------------------------------------------------------------

const VALUES = [
  ['Puls', (s) => s.pulse],
  ['U/min', (s) => s.rpm],
  ['km/h', (s) => (s.speed_x10 / 10).toFixed(1).replace('.', ',')],
  ['Strecke', (s) => s.distance],
  ['Soll W', (s) => s.target_watts],
  ['Ist W', (s) => s.actual_watts],
  ['Energie', (s) => s.energy],
  ['Zeit', (s) => mmss(s.elapsed_secs)],
];
const LINES = ['cts', 'dsr', 'ri', 'dcd'];

function liveTab() {
  const tiles = VALUES.map(([label]) => {
    const value = el('b', {}, '–');
    return { value, node: el('div', { class: 'svc-tile' }, label, value) };
  });
  const recovery = el('span', { class: 'svc-mark', hidden: true }, 'Erholungsmessung');
  const leds = LINES.map((name) => el('span', { class: 'led' }, name.toUpperCase()));
  const modemNote = el('span', { class: 'muted' }, 'nicht verfügbar (nur mit eigener serieller Schnittstelle)');
  const lastKey = el('span', { class: 'svc-key' });
  const log = el('div', { class: 'svc-log', role: 'log', 'aria-live': 'off' });
  const pauseBtn = el('button', { type: 'button', class: 'btn small', onclick: () => togglePause() }, 'Anhalten');
  const status = el('p', { class: 'muted' });
  const panel = el('section', { class: 'panel tab-panel', role: 'tabpanel' },
    el('h2', {}, 'Messwerte ', recovery),
    el('div', { class: 'svc-tiles' }, ...tiles.map((t) => t.node)),
    el('h2', { class: 'service-gap' }, 'Modem-Leitungen'),
    el('div', { class: 'svc-leds' }, ...leds, modemNote, lastKey),
    el('div', { class: 'panel-head service-gap' }, el('h2', {}, 'Rohverkehr'), pauseBtn),
    log,
    status);

  let entries = [];
  let paused = false;
  let socket = null;
  let retry = null;
  let running = false;
  let unsubscribe = null;

  function renderValues(view) {
    const s = view && view.last;
    tiles.forEach((t, i) => { t.value.textContent = s ? String(VALUES[i][1](s)) : '–'; });
    recovery.hidden = !(view && view.recovery);
  }

  function renderModem(lines) {
    modemNote.hidden = Boolean(lines);
    leds.forEach((led, i) => {
      led.hidden = !lines;
      led.classList.toggle('on', Boolean(lines && lines[LINES[i]]));
    });
  }

  function lineNode(entry) {
    const line = traceLine(entry);
    return el('div', { class: `svc-line ${line.cls}` }, el('span', { class: 'svc-time' }, line.time), ' ', line.text);
  }

  function renderLog() {
    log.replaceChildren(...entries.map(lineNode));
    log.scrollTop = log.scrollHeight;
  }

  function add(entry) {
    entries.push(entry);
    if (entries.length > MAX_LINES) entries.shift();
    if (entry.kind === 'key') lastKey.textContent = `Taste: ${entry.text}`;
    if (paused) return;
    log.append(lineNode(entry));
    while (log.childElementCount > MAX_LINES) log.firstElementChild.remove();
    log.scrollTop = log.scrollHeight;
  }

  function togglePause() {
    paused = !paused;
    pauseBtn.textContent = paused ? 'Weiter' : 'Anhalten';
    if (!paused) renderLog();
  }

  function connect() {
    if (!running) return;
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${scheme}://${location.host}/ws/service`);
    socket = ws;
    ws.addEventListener('open', () => { status.textContent = ''; });
    ws.addEventListener('message', (ev) => {
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (msg.type === 'history') {
        entries = msg.entries.slice(-MAX_LINES);
        const key = [...entries].reverse().find((e) => e.kind === 'key');
        if (key) lastKey.textContent = `Taste: ${key.text}`;
        if (!paused) renderLog();
      } else if (msg.type === 'trace') {
        add(msg.entry);
      } else if (msg.type === 'modem') {
        renderModem(msg.lines);
      }
    });
    ws.addEventListener('close', () => {
      if (socket !== ws) return;
      socket = null;
      if (!running) return;
      status.textContent = 'Verbindung unterbrochen, neuer Versuch …';
      retry = setTimeout(connect, RETRY_MS);
    });
    ws.addEventListener('error', () => ws.close());
  }

  return {
    panel,
    start() {
      running = true;
      renderValues(live.current());
      renderModem(null);
      unsubscribe = live.onLive(renderValues);
      connect();
    },
    stop() {
      running = false;
      if (unsubscribe) unsubscribe();
      clearTimeout(retry);
      const ws = socket;
      socket = null;
      if (ws) ws.close();
    },
  };
}

// ---- Backup -------------------------------------------------------------------------------

function backupTab() {
  const error = el('p', { class: 'error', role: 'alert', hidden: true });
  const status = el('p', { class: 'muted', role: 'status' });
  const fileInput = el('input', { type: 'file', accept: '.db,.sqlite,application/vnd.sqlite3', hidden: true, onchange: () => pick() });
  const downloadBtn = el('button', { type: 'button', class: 'btn primary', onclick: () => download() }, 'Backup herunterladen');
  const restoreBtn = el('button', { type: 'button', class: 'btn danger', onclick: () => fileInput.click() }, 'Backup einspielen …');
  const panel = el('section', { class: 'panel tab-panel', role: 'tabpanel' },
    el('h2', {}, 'Backup'),
    el('p', {}, 'Sichert alle Fahrer, Programme und Fahrten in eine Datei. Das geht auch während einer Fahrt.'),
    el('div', { class: 'service-actions' }, downloadBtn),
    el('h2', { class: 'service-gap' }, 'Einspielen'),
    el('p', {}, 'Ersetzt alle Daten dieses Servers durch ein Backup. Während einer Fahrt nicht möglich.'),
    el('div', { class: 'service-actions' }, restoreBtn, fileInput),
    error,
    status);

  function show(msg, isError) {
    error.textContent = isError ? msg : '';
    error.hidden = !isError;
    status.textContent = isError ? '' : msg;
  }

  async function download() {
    downloadBtn.disabled = true;
    show('');
    try {
      let resp;
      try {
        resp = await fetch('/api/service/backup');
      } catch {
        throw new Error('Server nicht erreichbar');
      }
      if (!resp.ok) throw new Error(`Fehler ${resp.status}`);
      const url = URL.createObjectURL(await resp.blob());
      const a = el('a', { href: url, download: backupName(new Date()), hidden: true });
      document.body.append(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 10000);
      show('Backup heruntergeladen.');
    } catch (e) {
      show(e.message, true);
    } finally {
      downloadBtn.disabled = false;
    }
  }

  async function pick() {
    const file = fileInput.files && fileInput.files[0];
    fileInput.value = '';
    if (!file) return;
    restoreBtn.disabled = true;
    show('Prüfe Backup …');
    try {
      const info = await postFile('/api/service/restore/check', file);
      show('');
      openConfirmDialog({
        title: 'Backup einspielen',
        text: `Alle Fahrer, Programme und Fahrten auf diesem Server werden durch das Backup ersetzt. Das Backup enthält ${restoreText(info)}.`,
        confirmLabel: 'Ersetzen',
        onConfirm: async () => {
          const done = await postFile('/api/service/restore', file);
          // The page reloads on the changed data_epoch; until then the result is shown.
          show(`Backup eingespielt: ${restoreText(done)}.`);
        },
      });
    } catch (e) {
      show(e.message, true);
    } finally {
      restoreBtn.disabled = false;
    }
  }

  return { panel, start() {}, stop() {} };
}

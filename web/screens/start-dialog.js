// Preview dialog shown when a program is chosen: curve, blocks, rider, then start.

import { api } from '../lib/api.js';
import { el } from '../lib/dom.js';
import { duration } from '../lib/format.js';
import { programPath, scaleMax } from '../lib/chart.js';
import { chartGrid, elevationStrip } from '../lib/chart-dom.js';
import { describeBlocks, statsLine, historyText } from '../lib/start-preview.js';
import { gainText } from '../lib/elevation.js';
import { expectRide } from './ride.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const CHART_W = 1000;
const CHART_H = 240;
const STRIP_H = 60;

/** The program's target watts with labelled grid lines; returns the svg and the label layer. */
function curve(segments, totalSecs) {
  const chart = document.createElementNS(SVG_NS, 'svg');
  chart.setAttribute('viewBox', `0 0 ${CHART_W} ${CHART_H}`);
  chart.setAttribute('preserveAspectRatio', 'none');
  chart.setAttribute('class', 'chart-svg');
  const area = document.createElementNS(SVG_NS, 'path');
  area.setAttribute('class', 'program-area');
  const max = scaleMax(segments, []);
  area.setAttribute('d', programPath(segments, totalSecs, CHART_W, CHART_H, max));
  const grid = chartGrid(CHART_W, CHART_H);
  grid.update(max);
  chart.append(grid.lines, area);
  return [chart, grid.labels];
}

/** Opens the dialog for `program` ({id, name}) and rider `personId`. */
export function openStartDialog(program, personId) {
  const opener = document.activeElement;
  let busy = false;
  let closed = false;
  const main = document.getElementById('app');

  const meta = el('p', { class: 'start-meta muted' }, '');
  const preview = el('div', { class: 'chart-body start-preview' });
  const strip = elevationStrip({ width: CHART_W, height: STRIP_H });
  strip.node.hidden = true;
  const chart = el('div', { class: 'start-chart' }, preview, strip.node);
  const blocks = el('ul', { class: 'start-blocks' });
  const riderName = el('span', {});
  const history = el('span', { class: 'start-history muted' });
  const rider = el('p', { class: 'start-rider' }, riderName, history);
  const error = el('p', { class: 'error', role: 'alert', hidden: true });
  const startBtn = el('button', { type: 'button', class: 'btn primary start-go', onclick: () => start() }, 'Training starten');
  const cancelBtn = el('button', { type: 'button', class: 'btn', onclick: () => close() }, 'Abbrechen');
  const box = el('div', { class: 'dialog start-dialog', role: 'dialog', 'aria-modal': 'true', 'aria-label': `Programm ${program.name}` },
    el('h2', { class: 'start-title' }, program.name), meta, chart, blocks, rider, error,
    el('div', { class: 'dialog-actions' }, startBtn, cancelBtn));
  const backdrop = el('div', { class: 'dialog-backdrop' }, box);

  function close() {
    if (busy && !closed) return;
    closed = true;
    if (main) main.inert = false;
    document.removeEventListener('keydown', onKey);
    window.removeEventListener('hashchange', onHash);
    backdrop.remove();
    if (opener && opener.isConnected) opener.focus();
  }
  function onKey(ev) {
    if (ev.key === 'Escape') close();
  }
  function onHash() {
    busy = false;
    close();
  }
  backdrop.addEventListener('click', (ev) => { if (ev.target === backdrop) close(); });

  async function start() {
    if (busy) return;
    busy = true;
    startBtn.disabled = true;
    cancelBtn.disabled = true;
    error.hidden = true;
    try {
      const { id } = await api('/api/rides', { method: 'POST', body: { person_id: personId, program_id: program.id } });
      busy = false;
      if (closed) return;
      expectRide(id);
      location.hash = '#/ride';
      close();
    } catch (e) {
      busy = false;
      if (closed) return;
      error.textContent = e.message;
      error.hidden = false;
      startBtn.disabled = false;
      cancelBtn.disabled = false;
      startBtn.focus();
    }
  }

  async function load() {
    const [detail, persons, rides] = await Promise.all([
      api(`/api/programs/${program.id}?person_id=${encodeURIComponent(personId)}`).catch((e) => e),
      api('/api/persons').catch(() => []),
      api(`/api/rides?person_id=${encodeURIComponent(personId)}`).catch(() => null),
    ]);
    const person = persons.find((p) => p.id === personId);
    riderName.textContent = person ? `Fahrer: ${person.name}` : '';
    history.textContent = rides ? historyText(rides, program.name) : '';
    if (detail instanceof Error) {
      error.textContent = detail.message;
      error.hidden = false;
      startBtn.disabled = true;
      return;
    }
    const elev = detail.elevation_preview;
    meta.replaceChildren(
      el('span', {}, `Gesamtdauer ${duration(detail.total_secs)}${elev ? ` · ≈ ${gainText(elev.gain_m)} bergauf` : ''}`),
      el('span', { class: 'start-stats' }, statsLine(detail.segments)));
    preview.replaceChildren(...curve(detail.segments, detail.total_secs));
    if (elev) {
      strip.update({ preview: elev, points: [], total: detail.total_secs });
      strip.node.hidden = false;
    }
    blocks.replaceChildren(...describeBlocks(detail.program).map((line) => el('li', {}, line)));
  }

  document.body.append(backdrop);
  if (main) main.inert = true;
  document.addEventListener('keydown', onKey);
  window.addEventListener('hashchange', onHash);
  startBtn.focus();
  load();
}

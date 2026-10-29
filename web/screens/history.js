// Ride history: list with comparison choice, ride detail and two-ride comparison.

import { api } from '../lib/api.js';
import * as live from '../lib/live.js';
import { el } from '../lib/dom.js';
import { km, whole, dateTime, duration, energy, energyLine, cockpitNote, rpmPair, rpmValue } from '../lib/format.js';
import { programPath, tracePath, scaleMax, cadenceMax, compareScale, downsample, nearestPoint, medianSpacing } from '../lib/chart.js';
import { svgEl, chartGrid, cadenceAxis, legendToggles, attachScrub, recoveryMarks, elevationStrip } from '../lib/chart-dom.js';
import { heightText } from '../lib/elevation.js';
import { gradeText, storedRecoveryText, recoveryNear } from '../lib/recovery.js';
import { loadLines } from '../lib/lines.js';
import { rideState } from '../lib/history.js';
import { loadSelected, saveSelected } from '../lib/selection.js';
import { rideDeleteText } from '../lib/delete-text.js';
import { openConfirmDialog } from './confirm-dialog.js';
import { samplePulse, pulseBySource } from '../lib/pulse.js';

const CHART_W = 1000;
const CHART_H = 300;
const STRIP_H = 60;
const MAX_POINTS = 2000;
const svg = svgEl;

// `pulse` is the effective one (strap, else cockpit); `cockpit` the ear clip's alone, for comparing.
const pointOf = (s, elevation) => ({
  t: s.ride_secs, watts: s.status.actual_watts, pulse: samplePulse(s) || 0, cockpit: s.status.pulse || null,
  rpm: s.status.rpm, target: s.target_watts, elevation,
});
/** The ride's samples as chart points, each with the server's height (`elevation_m[i]`). */
const pointsOf = (d) => d.samples.map((s, i) => pointOf(s, (d.elevation_m || [])[i]));
const gainValue = (r) => whole(r.elevation_gain_m);
const programName = (r) => r.program_name || 'Frei';
const pulsePair = (r) => (r.avg_pulse == null ? '–' : `${whole(r.avg_pulse)} / ${whole(r.max_pulse)}`);
const wattPair = (r) => `${r.avg_watts} / ${r.max_watts}`;

/** What the live view says: whether a frame was seen at all, and which ride runs. */
function liveInfo() {
  const view = live.current();
  return { known: view != null, id: view && view.ride ? view.ride.ride_id : null };
}

function statusBadge(r) {
  const { known, id } = liveInfo();
  const state = rideState(r, id, known);
  if (state === 'running') return el('a', { href: '#/ride', class: 'badge running' }, 'läuft');
  if (state === 'aborted') return el('span', { class: 'badge aborted' }, 'abgebrochen');
  return null;
}

/** Calls onEnd when the live ride goes away and onChange when the running id or liveKnown changes. */
function watchLive(onChange, onEnd) {
  let prev = liveInfo();
  return live.onLive(() => {
    const now = liveInfo();
    const changed = now.known !== prev.known || now.id !== prev.id;
    const ended = prev.id != null && now.id == null;
    prev = now;
    if (ended) onEnd();
    else if (changed) onChange();
  });
}

function pointsFor(points, key) {
  return downsample(points, MAX_POINTS, key);
}

/** Person names by id, fetched once; a failure just leaves the names out. */
async function loadPersonNames() {
  try {
    return new Map((await api('/api/persons')).map((p) => [p.id, p.name]));
  } catch {
    return new Map();
  }
}

/** The name to show for a ride, only when it is not the selected person's. */
function foreignName(names, personId) {
  return personId !== loadSelected() && names.has(personId) ? names.get(personId) : null;
}

function page(...children) {
  return el('section', { class: 'page history' }, ...children);
}

// ---- List ------------------------------------------------------------------
export const list = {
  mount(root) {
    let persons = [];
    let selected = loadSelected();
    let picking = selected == null;
    let rides = [];
    let ticked = [];
    let disposed = false;
    let loadToken = 0;
    const slots = new Map(); // ride id -> badge slot

    const error = el('p', { class: 'error', role: 'alert', hidden: true });
    const body = el('div', { class: 'history-body' });
    root.append(page(el('h1', {}, 'Verlauf'), error, body));

    function showError(msg) {
      error.textContent = msg || '';
      error.hidden = !msg;
    }

    function renderPicker() {
      body.replaceChildren(
        el('h2', {}, 'Wessen Fahrten?'),
        el('div', { class: 'choice-list' }, ...persons.map((p) => el('button', {
          type: 'button',
          class: `choice${p.id === selected ? ' selected' : ''}`,
          onclick: () => { selected = p.id; saveSelected(p.id); picking = false; loadRides(); },
        }, p.name))),
        persons.length === 0 ? el('p', { class: 'muted' }, 'Noch keine Person angelegt.') : '');
    }

    function toggle(id) {
      if (ticked.includes(id)) ticked = ticked.filter((x) => x !== id);
      else ticked = [...ticked, id].slice(-2);
      renderList();
    }

    function row(r) {
      const on = ticked.includes(r.id);
      const check = el('button', {
        type: 'button',
        class: `check${on ? ' on' : ''}`,
        role: 'checkbox',
        'aria-checked': on ? 'true' : 'false',
        'aria-label': `Fahrt vom ${dateTime(r.started_at)} zum Vergleich wählen`,
        onclick: () => toggle(r.id),
      }, on ? '✓' : '');
      const cell = (label, value) => el('div', { class: 'cell' }, el('span', { class: 'lbl' }, label), el('span', { class: 'val' }, value));
      const slot = el('span', { class: 'badge-slot' });
      slots.set(r.id, { ride: r, slot });
      const badge = statusBadge(r);
      if (badge) slot.append(badge);
      return el('div', { class: `ride-row${on ? ' ticked' : ''}` },
        check,
        el('a', { href: `#/history/${r.id}`, class: 'ride-link' },
          el('div', { class: 'cell wide' },
            el('span', { class: 'val' }, dateTime(r.started_at)),
            el('span', { class: 'sub' }, programName(r), ' ', slot)),
          cell('Dauer', duration(r.ride_secs)),
          cell('Ø / max Watt', wattPair(r)),
          cell('Ø / max Puls', pulsePair(r)),
          cell('Ø U/min', rpmValue(r.avg_rpm)),
          cell('km', km(r.distance_m)),
          cell('Hm', gainValue(r)),
          el('div', { class: 'cell energy-cell' },
            el('span', { class: 'lbl' }, 'Arbeit · Verbrauch'),
            el('span', { class: 'val' }, energyLine(r.energy_kj)),
            el('span', { class: 'sub' }, cockpitNote(r.energy_kj)))));
    }

    function renderList() {
      const person = persons.find((p) => p.id === selected);
      slots.clear();
      const compareBtn = el('button', {
        type: 'button',
        class: 'btn primary',
        disabled: ticked.length !== 2,
        onclick: () => { location.hash = `#/history/compare/${ticked[0]}/${ticked[1]}`; },
      }, 'Vergleichen');
      body.replaceChildren(
        el('div', { class: 'page-head' },
          el('h2', {}, person ? person.name : 'Fahrten'),
          el('div', { class: 'row-actions' },
            el('span', { class: 'muted' }, ticked.length === 2 ? '2 Fahrten gewählt' : 'Zwei Fahrten zum Vergleichen ankreuzen'),
            compareBtn,
            el('button', { type: 'button', class: 'btn', onclick: () => { picking = true; renderPicker(); } }, 'Person wechseln'))),
        rides.length === 0
          ? el('p', { class: 'muted' }, 'Noch keine Fahrten.')
          : el('div', { class: 'ride-rows' }, ...rides.map(row)));
    }

    function updateBadges() {
      for (const { ride, slot } of slots.values()) {
        const badge = statusBadge(ride);
        slot.replaceChildren(...(badge ? [badge] : []));
      }
    }

    async function loadRides(keepView = false) {
      const token = ++loadToken;
      showError('');
      if (!keepView) {
        ticked = [];
        body.replaceChildren(el('p', { class: 'muted' }, 'Lade …'));
      }
      try {
        const r = await api(`/api/rides?person_id=${selected}`);
        if (disposed || token !== loadToken) return;
        rides = r;
        renderList();
      } catch (e) {
        if (disposed || token !== loadToken) return;
        if (!keepView) body.replaceChildren();
        showError(e.message);
      }
    }

    async function init() {
      try {
        persons = await api('/api/persons');
      } catch (e) {
        if (!disposed) showError(e.message);
        return;
      }
      if (disposed) return;
      if (selected != null && !persons.some((p) => p.id === selected)) { selected = null; picking = true; }
      if (picking) renderPicker();
      else loadRides();
    }

    // Badges of unfinished rides follow the live state; a ride that just ended is fetched again.
    const off = watchLive(
      () => { if (!picking) updateBadges(); },
      () => { if (!picking) loadRides(true); });
    init();
    return () => { disposed = true; off(); };
  },
};

// ---- Detail ----------------------------------------------------------------
const sourcePair = (p) => (p ? `${p.avg} / ${p.max}` : '–');

/** `bySource` (from `pulseBySource`) splits the pulse into strap and ear clip when the ride had a strap. */
function summaryGrid(s, bySource) {
  const item = (label, value, unit) => el('div', { class: 'tile sum-item' },
    el('span', { class: 'lbl' }, label), el('span', { class: 'mid' }, value, unit ? el('small', {}, ` ${unit}`) : ''));
  const e = energy(s.energy_kj);
  return el('div', { class: 'summary-grid' },
    item('Dauer', duration(s.ride_secs)),
    item('Ø / max Watt', wattPair(s), 'W'),
    ...(bySource && bySource.strap
      ? [item('Ø / max Puls Gurt', sourcePair(bySource.strap), '/min'),
        item('Ø / max Puls Ohr', sourcePair(bySource.cockpit), bySource.cockpit ? '/min' : '')]
      : [item('Ø / max Puls', pulsePair(s), s.avg_pulse == null ? '' : '/min')]),
    item('Ø / max U/min', rpmPair(s), s.avg_rpm == null ? '' : 'U/min'),
    item('Strecke', km(s.distance_m), 'km'),
    item('Hm bergauf', gainValue(s)),
    el('div', { class: 'tile sum-item sum-energy' },
      el('span', { class: 'lbl' }, 'Arbeit · Verbrauch'),
      el('span', { class: 'mid' }, el('span', { class: 'e-line' }, `${e.kj} kJ`), el('span', { class: 'e-line' }, `${e.kcal} kcal`)),
      el('span', { class: 'muted energy-note' }, cockpitNote(s.energy_kj))));
}

function rideTitle(s) {
  return `${dateTime(s.started_at)} · ${programName(s)}`;
}

function backLink() {
  return el('a', { href: '#/history', class: 'btn' }, 'Zurück zum Verlauf');
}

export const detail = {
  mount(root, params) {
    const id = Number(params[0]);
    let disposed = false;
    let summary = null;
    let titleBadge = null;
    let deleteBtn = null;
    let deleteHint = null;
    const holder = el('div', {}, el('p', { class: 'muted' }, 'Lade …'));
    root.append(page(el('div', { class: 'page-head' }, el('h1', {}, 'Fahrt'), backLink()), holder));

    function updateBadge() {
      if (!titleBadge || !summary) return;
      const badge = statusBadge(summary);
      titleBadge.replaceChildren(...(badge ? [badge] : []));
      const running = badge != null && badge.classList.contains('running');
      deleteBtn.disabled = running;
      deleteHint.hidden = !running;
    }

    function askDelete(s) {
      openConfirmDialog({
        title: 'Fahrt löschen',
        text: rideDeleteText(s),
        confirmLabel: 'Fahrt löschen',
        onConfirm: async () => {
          await api(`/api/rides/${s.id}`, { method: 'DELETE' });
          location.hash = '#/history';
        },
      });
    }

    async function load() {
      let d;
      let names;
      try {
        [d, names] = await Promise.all([api(`/api/rides/${id}`), loadPersonNames()]);
      } catch (e) {
        if (!disposed) holder.replaceChildren(el('p', { class: 'error' }, e.message));
        return;
      }
      if (disposed) return;
      const s = d.summary;
      summary = s;
      const points = pointsOf(d);
      const segments = d.program_segments;
      const last = points.length ? points[points.length - 1].t : 0;
      const programEnd = segments && segments.length
        ? segments[segments.length - 1].start_secs + segments[segments.length - 1].duration_secs : 0;
      const total = Math.max(60, last, programEnd);
      const max = scaleMax(segments, points);
      const chartSvg = svg('svg', { viewBox: `0 0 ${CHART_W} ${CHART_H}`, preserveAspectRatio: 'none', class: 'chart-svg' });
      const grid = chartGrid(CHART_W, CHART_H);
      grid.update(max);
      chartSvg.append(grid.lines);
      if (segments && segments.length) chartSvg.append(svg('path', { class: 'program-area', d: programPath(segments, total, CHART_W, CHART_H, max) }));
      // Cadence is drawn first (behind) on its own right-hand axis.
      const cadMax = cadenceMax(points);
      const cadence = cadenceAxis();
      cadence.update(cadMax);
      chartSvg.append(svg('polyline', { class: 'trace rpm', 'vector-effect': 'non-scaling-stroke', points: tracePath(pointsFor(points, 'rpm'), 'rpm', total, CHART_W, CHART_H, cadMax) }));
      const bySource = pulseBySource(d.samples);
      // With a strap the ear clip's pulse is drawn too, dashed, for comparing.
      if (bySource.strap && bySource.cockpit) {
        chartSvg.append(svg('polyline', { class: 'trace pulse pulse-cockpit', 'vector-effect': 'non-scaling-stroke', points: tracePath(pointsFor(points, 'cockpit'), 'cockpit', total, CHART_W, CHART_H, max) }));
      }
      for (const key of ['target', 'watts', 'pulse']) {
        chartSvg.append(svg('polyline', { class: `trace ${key}`, 'vector-effect': 'non-scaling-stroke', points: tracePath(pointsFor(points, key), key, total, CHART_W, CHART_H, max) }));
      }
      const recoveries = d.recoveries || [];
      const marks = recoveryMarks();
      marks.update(recoveries, total);
      const chartBody = el('div', { class: 'chart-body scrub-on' }, chartSvg, grid.labels, cadence.labels, marks.layer);
      const shown = pointsFor(points, 'watts');
      const lines = loadLines();
      attachScrub(chartBody, {
        enabled: () => true,
        total: () => total,
        lookup: (t) => {
          const p = nearestPoint(shown, t);
          // A measurement after the ride sits at its end, past the last point.
          const rec = recoveryNear(recoveries, t, Math.max(3, total / 80));
          if (!p && !rec) return null;
          return { t: rec ? rec.ride_secs : p.t, rows: [
            ...(p ? [
              { label: 'Watt', value: String(p.watts) },
              { cls: 'tgt', label: 'Ziel', value: p.target == null ? '–' : String(p.target) },
              ...(lines.pulse ? [{ cls: 'hr', label: 'Puls', value: p.pulse ? String(p.pulse) : '–' }] : []),
              ...(lines.pulse && bySource.strap && bySource.cockpit ? [{ cls: 'hr', label: 'Puls Ohr', value: p.cockpit ? String(p.cockpit) : '–' }] : []),
              ...(lines.rpm ? [{ cls: 'cad', label: 'U/min', value: rpmValue(p.rpm) }] : []),
              ...(lines.elevation && p.elevation != null ? [{ cls: 'elev', label: 'Höhe', value: heightText(p.elevation) }] : []),
            ] : []),
            ...(rec ? [{ cls: 'hr', label: 'Erholung', value: `${gradeText(rec.grade_x10)} (−${rec.difference})` }] : []),
          ] };
        },
      });
      const chartTile = el('div', { class: 'tile chart detail-chart has-elev' });
      const legend = legendToggles(chartTile, lines);
      const strip = elevationStrip({ width: CHART_W, height: STRIP_H });
      strip.update({
        preview: d.elevation_preview,
        points: pointsFor(points, 'elevation').map((p) => ({ t: p.t, h: p.elevation })),
        total,
        gain: s.elevation_gain_m,
      });
      chartTile.append(
        el('div', { class: 'chart-head' },
          el('span', { class: 'lbl' }, legend.node),
          el('span', { class: 'lbl chart-time' }, duration(total))),
        chartBody,
        strip.node);
      const recoveryList = el('div', { class: 'tile recovery-list', hidden: !recoveries.length },
        ...recoveries.map((r) => el('div', {},
          el('span', { class: 'rec-key' }, `Erholung bei ${duration(r.ride_secs)}`), ` · ${storedRecoveryText(r)}`)));
      const who = foreignName(names, s.person_id);
      titleBadge = el('span', { class: 'badge-slot' });
      deleteBtn = el('button', { type: 'button', class: 'btn danger', onclick: () => askDelete(s) }, 'Fahrt löschen');
      deleteHint = el('span', { class: 'muted delete-hint', hidden: true }, 'Fahrt läuft noch');
      holder.replaceChildren(
        el('p', { class: 'muted ride-title' }, who ? `${who} · ` : '', rideTitle(s), ' ', titleBadge),
        summaryGrid(s, bySource),
        chartTile,
        recoveryList,
        el('div', { class: 'actions' },
          el('button', { type: 'button', class: 'btn', disabled: true }, 'Export (kommt später)')),
        el('div', { class: 'actions danger-zone' }, deleteBtn, deleteHint));
      updateBadge();
    }

    load();
    const off = watchLive(updateBadge, load);
    return () => { disposed = true; off(); };
  },
};

// ---- Comparison ------------------------------------------------------------
export const compare = {
  mount(root, params) {
    const ids = [Number(params[0]), Number(params[1])];
    let disposed = false;
    const holder = el('div', {}, el('p', { class: 'muted' }, 'Lade …'));
    root.append(page(el('div', { class: 'page-head' }, el('h1', {}, 'Vergleich'), backLink()), holder));

    if (ids[0] === ids[1]) {
      holder.replaceChildren(
        el('p', { class: 'error' }, 'Bitte zwei verschiedene Fahrten wählen.'),
        el('div', { class: 'actions' }, backLink()));
      return () => {};
    }

    (async () => {
      const [results, names] = await Promise.all([
        Promise.allSettled(ids.map((i) => api(`/api/rides/${i}`))),
        loadPersonNames(),
      ]);
      if (disposed) return;
      const failed = results
        .map((r, i) => (r.status === 'rejected' ? el('p', { class: 'error' }, `Fahrt ${ids[i]}: ${r.reason.message}`) : null))
        .filter(Boolean);
      if (failed.length) {
        holder.replaceChildren(...failed, el('div', { class: 'actions' }, backLink()));
        return;
      }
      const [a, b] = results.map((r) => r.value);
      const label = (d) => {
        const who = foreignName(names, d.summary.person_id);
        return who ? `${who} · ${rideTitle(d.summary)}` : rideTitle(d.summary);
      };
      const head = (d) => {
        const who = foreignName(names, d.summary.person_id);
        return who ? `${who}, ${dateTime(d.summary.started_at)}` : dateTime(d.summary.started_at);
      };
      const pa = pointsOf(a);
      const pb = pointsOf(b);
      const { total, max } = compareScale(pa, pb);
      const chartSvg = svg('svg', { viewBox: `0 0 ${CHART_W} ${CHART_H}`, preserveAspectRatio: 'none', class: 'chart-svg' });
      const grid = chartGrid(CHART_W, CHART_H);
      grid.update(max);
      const shownA = pointsFor(pa, 'watts');
      const shownB = pointsFor(pb, 'watts');
      chartSvg.append(
        grid.lines,
        svg('polyline', { class: 'trace cmp-a', 'vector-effect': 'non-scaling-stroke', points: tracePath(pointsFor(pa, 'watts'), 'watts', total, CHART_W, CHART_H, max) }),
        svg('polyline', { class: 'trace cmp-b', 'vector-effect': 'non-scaling-stroke', points: tracePath(pointsFor(pb, 'watts'), 'watts', total, CHART_W, CHART_H, max) }));
      const chartBody = el('div', { class: 'chart-body scrub-on' }, chartSvg, grid.labels);
      // A ride shows a value only within 1.5 median sample spacings of the pointer (not past its end).
      const tolA = 1.5 * Math.max(1, medianSpacing(shownA));
      const tolB = 1.5 * Math.max(1, medianSpacing(shownB));
      attachScrub(chartBody, {
        enabled: () => true,
        total: () => total,
        lookup: (t) => {
          const value = (pts, tolerance, text) => {
            const p = nearestPoint(pts, t);
            return p && Math.abs(p.t - t) <= tolerance ? text(p) : '–';
          };
          const watts = (p) => `${p.watts} W`;
          const cadence = (p) => rpmValue(p.rpm);
          return { t, rows: [
            { cls: 'cmp-a', label: 'A', value: value(shownA, tolA, watts) },
            { cls: 'cmp-b', label: 'B', value: value(shownB, tolB, watts) },
            { cls: 'cad', label: 'U/min A', value: value(shownA, tolA, cadence) },
            { cls: 'cad', label: 'U/min B', value: value(shownB, tolB, cadence) },
          ] };
        },
      });
      const legend = (cls, d) => el('span', { class: 'legend-item' }, el('span', { class: `swatch ${cls}` }), label(d));
      const rows = [
        ['Programm', programName],
        ['Dauer', (s) => duration(s.ride_secs)],
        ['Ø / max Watt', wattPair],
        ['Ø / max Puls', pulsePair],
        ['Ø / max U/min', rpmPair],
        ['Strecke (km)', (s) => km(s.distance_m)],
        ['Hm bergauf', gainValue],
        ['Arbeit (kJ)', (s) => String(energy(s.energy_kj).kj)],
        ['Verbrauch (kcal)', (s) => String(energy(s.energy_kj).kcal)],
        ['Cockpit (kJ)', (s) => `(${energy(s.energy_kj).cockpitKj})`, 'muted-row'],
      ];
      holder.replaceChildren(
        el('div', { class: 'tile chart detail-chart with-legend' },
          el('div', { class: 'chart-head' },
            el('span', { class: 'lbl' }, 'Watt über Fahrzeit'),
            el('span', { class: 'lbl chart-time' }, duration(total))),
          chartBody,
          el('div', { class: 'legend' }, legend('cmp-a', a), legend('cmp-b', b))),
        el('div', { class: 'table-scroll' }, el('table', { class: 'compare-table' },
          el('thead', {}, el('tr', {},
            el('th', {}),
            el('th', {}, el('span', { class: 'swatch cmp-a' }), head(a)),
            el('th', {}, el('span', { class: 'swatch cmp-b' }), head(b)))),
          el('tbody', {}, ...rows.map(([label, fn, cls]) => el('tr', cls ? { class: cls } : {},
            el('th', { scope: 'row' }, label), el('td', {}, fn(a.summary)), el('td', {}, fn(b.summary))))))));
    })();
    return () => { disposed = true; };
  },
};

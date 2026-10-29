// DOM parts shared by the charts: SVG helper, grid lines with labels and the scrub crosshair.

import { el } from './dom.js';
import { gridSteps, cadenceSteps, scrubTime, tooltipLeft, isHorizontalDrag, isTap } from './chart.js';
import { duration } from './format.js';
import { saveLines } from './lines.js';
import { gradeText } from './recovery.js';
import { elevationRange, elevationSteps, elevationPath, elevationArea, heightText, gainText } from './elevation.js';

const SVG_NS = 'http://www.w3.org/2000/svg';

export function svgEl(tag, attrs = {}) {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  return node;
}

/**
 * Horizontal grid for a chart drawn in a `width` x `height` viewBox. Put `lines` (an SVG group) first in the
 * chart svg and `labels` (an HTML layer) next to the svg inside `.chart-body`. `update(max)` redraws for a new
 * y maximum. Without `labels: true` the lines are drawn lighter and no labels appear (small previews).
 */
export function chartGrid(width, height, { labels = true } = {}) {
  const lines = svgEl('g', { class: labels ? 'grid' : 'grid grid-light' });
  const layer = el('div', { class: 'grid-labels', 'aria-hidden': 'true' });
  let drawn = 0;
  function update(max) {
    if (max === drawn) return;
    drawn = max;
    const steps = gridSteps(max);
    lines.replaceChildren(...steps.map((v) => {
      const y = String(Math.round((height - (v / max) * height) * 10) / 10);
      return svgEl('line', { x1: 0, x2: width, y1: y, y2: y, 'vector-effect': 'non-scaling-stroke' });
    }));
    if (!labels) return;
    layer.replaceChildren(...steps.map((v, i) => {
      const highest = i === steps.length - 1;
      // The label of a line at the very top goes below the line.
      const top = highest && v >= max * 0.9;
      const span = el('span', { class: top ? 'grid-label top' : 'grid-label' }, highest ? `${v} W` : String(v));
      span.style.bottom = `${(v / max) * 100}%`;
      return span;
    }));
  }
  return { lines, labels: layer, update };
}

/**
 * Right-hand cadence axis: a few small labels in the cadence colour, no lines. Put `labels` (an HTML layer)
 * next to the svg inside `.chart-body`. `update(max)` redraws for a new axis maximum (see `cadenceMax`).
 */
export function cadenceAxis() {
  const layer = el('div', { class: 'cad-labels', 'aria-hidden': 'true' });
  let drawn = 0;
  function update(max) {
    if (max === drawn) return;
    drawn = max;
    const steps = cadenceSteps(max);
    layer.replaceChildren(...steps.map((v, i) => {
      const highest = i === steps.length - 1;
      const top = highest && v >= max * 0.9;
      const span = el('span', { class: top ? 'cad-label top' : 'cad-label' }, highest ? `${v} U/min` : String(v));
      span.style.bottom = `${(v / max) * 100}%`;
      return span;
    }));
  }
  return { labels: layer, update };
}

/**
 * Vertical lines at the recovery measurements of a ride, each with its fitness grade on top. Put `layer` (an
 * HTML layer) next to the svg inside `.chart-body`. `update(recoveries, total)` redraws them for a chart
 * spanning `total` seconds.
 */
export function recoveryMarks() {
  const layer = el('div', { class: 'recovery-marks', 'aria-hidden': 'true' });
  let drawn = '';
  function update(recoveries, total) {
    const key = JSON.stringify([recoveries || [], total]);
    if (key === drawn) return;
    drawn = key;
    layer.replaceChildren(...(recoveries || []).map((r) => {
      const mark = el('div', { class: 'recovery-mark' }, el('span', { class: 'recovery-flag' }, gradeText(r.grade_x10)));
      const pos = total > 0 ? Math.min(1, Math.max(0, r.ride_secs / total)) : 0;
      mark.style.left = `${pos * 100}%`;
      // Near the right edge the flag goes to the left of the line.
      mark.classList.toggle('flip', pos > 0.9);
      return mark;
    }));
  }
  return { layer, update };
}

/**
 * The chart legend "Ziel · Watt · Puls · U/min · Höhe" with Puls, U/min and Höhe as toggle buttons. `lines`
 * ({rpm, pulse, elevation}, see lib/lines.js) is changed in place and saved; `target` (the chart tile) gets the
 * classes `hide-rpm`, `hide-pulse` and `hide-elevation`, which the stylesheet uses to hide those lines and the
 * elevation strip. `onChange` runs after each toggle.
 */
export function legendToggles(target, lines, onChange = () => {}) {
  const buttons = {};
  const make = (key, cls, text) => {
    const b = el('button', { type: 'button', class: `leg-btn ${cls}`, onclick: () => {
      lines[key] = !lines[key];
      saveLines(lines);
      apply();
      onChange();
    } }, text);
    buttons[key] = b;
    return b;
  };
  const node = el('span', { class: 'legend-lines' },
    el('span', { class: 'tgt' }, 'Ziel'), ' · Watt · ', make('pulse', 'hr', 'Puls'), ' · ', make('rpm', 'cad', 'U/min'),
    ' · ', make('elevation', 'elev', 'Höhe'));
  function apply() {
    for (const key of ['rpm', 'pulse', 'elevation']) {
      target.classList.toggle(`hide-${key}`, !lines[key]);
      buttons[key].setAttribute('aria-pressed', lines[key] ? 'true' : 'false');
    }
  }
  apply();
  return { node };
}

/**
 * Elevation strip under a chart, in a `width` x `height` viewBox with the chart's time axis: the program preview
 * dashed, the ridden height solid with a light fill, a "now" line, metre labels in an HTML layer and a small
 * label "62 m · 40 Hm bergauf · geplant ≈ 121 Hm". `update({preview, points, total, now, height, gain})` redraws:
 * `preview` is the server's `{points: [[secs, height], …], gain_m}` or null, `points` are `{t, h}`, `now` the
 * ride second for the "now" line (null: none), `height`/`gain` the current values (default: the last point, no gain).
 */
export function elevationStrip({ width, height }) {
  const previewLine = svgEl('polyline', { class: 'elev-preview', 'vector-effect': 'non-scaling-stroke' });
  const area = svgEl('path', { class: 'elev-area' });
  const line = svgEl('polyline', { class: 'elev-line', 'vector-effect': 'non-scaling-stroke' });
  const nowLine = svgEl('line', { class: 'now', y1: 0, y2: height, 'vector-effect': 'non-scaling-stroke' });
  const gridLines = svgEl('g', { class: 'grid' });
  const svgNode = svgEl('svg', { viewBox: `0 0 ${width} ${height}`, preserveAspectRatio: 'none', class: 'chart-svg' });
  svgNode.append(gridLines, area, previewLine, line, nowLine);
  const labels = el('div', { class: 'grid-labels', 'aria-hidden': 'true' });
  const label = el('div', { class: 'elev-label' });
  const node = el('div', { class: 'elev-strip' }, label, el('div', { class: 'elev-body' }, svgNode, labels));
  let drawnRange = '';

  function drawAxis(range) {
    const key = `${range.min},${range.max}`;
    if (key === drawnRange) return;
    drawnRange = key;
    const steps = elevationSteps(range);
    const y = (v) => String(Math.round((height - ((v - range.min) / (range.max - range.min)) * height) * 10) / 10);
    gridLines.replaceChildren(...steps.map((v) => svgEl('line', { x1: 0, x2: width, y1: y(v), y2: y(v), 'vector-effect': 'non-scaling-stroke' })));
    labels.replaceChildren(...steps.map((v, i) => {
      const highest = i === steps.length - 1;
      const pos = (v - range.min) / (range.max - range.min);
      const span = el('span', { class: highest && pos >= 0.85 ? 'grid-label top' : 'grid-label' }, highest ? `${v} m` : String(v));
      span.style.bottom = `${pos * 100}%`;
      return span;
    }));
  }

  function update({ preview, points, total, now = null, height: current, gain }) {
    const planned = preview ? preview.points.map(([t, h]) => ({ t, h })) : [];
    const range = elevationRange(preview ? preview.points : [], points);
    drawAxis(range);
    previewLine.setAttribute('points', elevationPath(planned, total, width, height, range));
    line.setAttribute('points', elevationPath(points, total, width, height, range));
    area.setAttribute('d', elevationArea(points, total, width, height, range));
    const x = now == null || !(total > 0) ? null : String(Math.min(1, now / total) * width);
    nowLine.toggleAttribute('hidden', x == null);
    if (x != null) {
      nowLine.setAttribute('x1', x);
      nowLine.setAttribute('x2', x);
    }
    const last = points.length ? points[points.length - 1].h : null;
    const parts = [heightText(current ?? last)];
    if (gain != null) parts.push(`${gainText(gain)} bergauf`);
    if (preview) parts.push(`geplant ≈ ${gainText(preview.gain_m)}`);
    label.textContent = parts.join(' · ');
  }

  return { node, update };
}

/**
 * Scrub crosshair on a `.chart-body`: a vertical line and a tooltip follow the pointer (mouse hover or
 * touch drag) while `enabled()` is true. `lookup(t)` gets the time under the pointer and returns
 * `{t, rows: [{cls, label, value}]}` (t = where to draw the line) or null. `format` renders the time.
 * Touch: the page keeps scrolling vertically (touch-action: pan-y); scrubbing starts with a clearly
 * horizontal drag, and a cancelled pointer (the browser took over for scrolling) hides it. Returns `{hide}`.
 */
export function attachScrub(body, { enabled, total, lookup, format = duration }) {
  const line = el('div', { class: 'scrub-line', hidden: true });
  const tip = el('div', { class: 'scrub-tip', hidden: true });
  body.append(line, tip);
  let dragging = false;
  let start = null; // touch pointer down position, until the gesture is decided

  function hide() {
    dragging = false;
    start = null;
    line.hidden = true;
    tip.hidden = true;
  }

  function show(ev) {
    if (!enabled()) { hide(); return; }
    const rect = body.getBoundingClientRect();
    const secs = total();
    if (rect.width <= 0 || !(secs > 0)) { hide(); return; }
    const found = lookup(scrubTime(ev.clientX - rect.left, rect.width, secs));
    if (!found) { hide(); return; }
    const x = Math.min(1, Math.max(0, found.t / secs)) * rect.width;
    tip.replaceChildren(
      el('div', { class: 'scrub-time' }, format(found.t)),
      ...found.rows.map((r) => el('div', { class: 'scrub-row' },
        el('span', { class: `scrub-key ${r.cls || ''}` }, r.label), el('span', { class: 'scrub-val' }, r.value))));
    line.style.left = `${x}px`;
    line.hidden = false;
    tip.hidden = false;
    tip.style.left = `${tooltipLeft(x, rect.width, tip.offsetWidth)}px`;
  }

  body.addEventListener('pointerdown', (ev) => {
    if (!enabled()) return;
    if (ev.pointerType === 'mouse') show(ev);
    else start = { x: ev.clientX, y: ev.clientY };
  });
  body.addEventListener('pointermove', (ev) => {
    if (ev.pointerType === 'mouse' || dragging) { show(ev); return; }
    if (start && isHorizontalDrag(ev.clientX - start.x, ev.clientY - start.y)) {
      dragging = true;
      try { body.setPointerCapture(ev.pointerId); } catch { /* pointer already gone */ }
      show(ev);
    }
  });
  for (const type of ['pointerup', 'pointercancel', 'pointerleave']) body.addEventListener(type, hide);

  return { hide };
}

/**
 * Calls `onTap()` after a short press on `body` that did not move (see `isTap`): mouse click or finger tap.
 * Independent of `attachScrub`, which is off while riding. A press that becomes a horizontal drag (the gesture
 * that starts the scrub crosshair, see `isHorizontalDrag`) is no tap, even when it ends within the tap limits.
 */
export function attachTap(body, onTap) {
  let start = null;
  body.addEventListener('pointerdown', (ev) => {
    start = ev.isPrimary && ev.button === 0 ? { x: ev.clientX, y: ev.clientY, at: performance.now(), id: ev.pointerId } : null;
  });
  body.addEventListener('pointermove', (ev) => {
    if (start && start.id === ev.pointerId && isHorizontalDrag(ev.clientX - start.x, ev.clientY - start.y)) start = null;
  });
  body.addEventListener('pointerup', (ev) => {
    const s = start;
    start = null;
    if (s && s.id === ev.pointerId && isTap(ev.clientX - s.x, ev.clientY - s.y, performance.now() - s.at)) onTap();
  });
  body.addEventListener('pointercancel', () => { start = null; });
}

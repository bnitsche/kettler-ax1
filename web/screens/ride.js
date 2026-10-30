// Live training screen (design H1) and the summary after the ride.

import { api } from '../lib/api.js';
import * as live from '../lib/live.js';
import { el } from '../lib/dom.js';
import { mmss, duration, kmh, km, whole, energy, cockpitText, cockpitNote, rpmPair, rpmValue } from '../lib/format.js';
import { programPath, tracePath, scaleMax, cadenceMax, appendPoint, mergeTrace, downsample, nearestPoint } from '../lib/chart.js';
import { svgEl, chartGrid, cadenceAxis, legendToggles, attachScrub, attachTap, recoveryMarks, elevationStrip } from '../lib/chart-dom.js';
import { heightText } from '../lib/elevation.js';
import { bandValues, pausedHint } from '../lib/big-view.js';
import { gradeText, recoveryNear, recoveryFinished } from '../lib/recovery.js';
import { loadLines } from '../lib/lines.js';
import { loadFinished } from '../lib/history.js';
import { samplePulse, pulseSourceText } from '../lib/pulse.js';
import { planCues, loadCueMode, saveCueMode, nextCueMode } from '../lib/cues.js';
import * as sound from '../lib/sound.js';

const CHART_W = 1000;
const CHART_H = 200;
const STRIP_H = 60;
const RING_R = 84;
const RING_C = 2 * Math.PI * RING_R;
const STEP_WATTS = 10;
const START_TIMEOUT_MS = 10000;
// Points drawn per trace at most; long rides are downsampled.
const TRACE_POINTS = 2000;

let expectedRide = null;

/** Called by the rider screen after POST /api/rides succeeded. */
export function expectRide(id) {
  expectedRide = id;
}

function takeExpectedRide() {
  const id = expectedRide;
  expectedRide = null;
  return id;
}

const svg = svgEl;

const CUE_TITLES = { all: 'Ansage und Töne', beeps: 'Nur Töne', off: 'Ton aus' };

/** Speaker icon: waves for "all", a single wave for "beeps", a cross for "off" and for locked sound. */
function speakerPaths(mode, locked) {
  const body = 'M4 9h4l5-4v14l-5-4H4z';
  if (locked || mode === 'off') return [body, 'M17 9l5 6M22 9l-5 6'];
  if (mode === 'beeps') return [body, 'M16.5 9.5a3.5 3.5 0 0 1 0 5'];
  return [body, 'M16.5 9.5a3.5 3.5 0 0 1 0 5', 'M19 7a7 7 0 0 1 0 10'];
}

/** A stored sample as a chart point; `elevation` is the server's height for it (`RideDetail.elevation_m[i]`). */
function sampleToPoint(s, elevation) {
  return { t: s.ride_secs, watts: s.status.actual_watts, pulse: samplePulse(s) || 0, rpm: s.status.rpm, target: s.target_watts, elevation };
}

/** Chart points as `{t, h}` for the elevation strip, downsampled like the other traces. */
function elevationPoints(points) {
  return downsample(points, TRACE_POINTS, 'elevation').map((p) => ({ t: p.t, h: p.elevation }));
}

function pulseText(pulse) {
  return pulse ? String(pulse) : '–';
}

export function mount(root) {
  let rideId = null;
  let ride = null; // latest RideLive
  let view = null; // latest LiveView
  let points = [];
  let recoveries = []; // stored recovery measurements of the ride
  let recoveryStates = null;
  let loading = false;
  let pending = []; // live points waiting for a trace reload
  let reloadAgain = false;
  let mode = 'idle'; // idle | live | summary
  let disposed = false;
  // A ride just started from the rider screen; shown as "Starte …" until its first live view.
  let starting = takeExpectedRide();
  const startTimer = starting ? setTimeout(() => {
    starting = null;
    if (!disposed && mode === 'idle') showIdle();
  }, START_TIMEOUT_MS) : null;

  // ---- Live layout ------------------------------------------------------
  const chartTitle = el('span', { class: 'lbl' });
  const chartTime = el('span', { class: 'lbl chart-time' });
  const programArea = svg('path', { class: 'program-area' });
  const targetLine = svg('polyline', { class: 'trace target', 'vector-effect': 'non-scaling-stroke' });
  const wattsLine = svg('polyline', { class: 'trace watts', 'vector-effect': 'non-scaling-stroke' });
  const pulseLine = svg('polyline', { class: 'trace pulse', 'vector-effect': 'non-scaling-stroke' });
  const rpmLine = svg('polyline', { class: 'trace rpm', 'vector-effect': 'non-scaling-stroke' });
  const nowLine = svg('line', { class: 'now', y1: 0, y2: CHART_H, 'vector-effect': 'non-scaling-stroke' });
  // Darkens the part of the program still to come (large view only).
  const future = svg('rect', { class: 'future', x: CHART_W, y: 0, width: 0, height: CHART_H });
  const chartSvg = svg('svg', { viewBox: `0 0 ${CHART_W} ${CHART_H}`, preserveAspectRatio: 'none', class: 'chart-svg' });
  const grid = chartGrid(CHART_W, CHART_H);
  const cadence = cadenceAxis();
  chartSvg.append(grid.lines, programArea, future, rpmLine, targetLine, wattsLine, pulseLine, nowLine);

  const marks = recoveryMarks();
  const chartBody = el('div', { class: 'chart-body' }, chartSvg, grid.labels, cadence.labels, marks.layer);
  const lines = loadLines();
  // A touch on the chart while riding is an accident; the crosshair works when paused or finished.
  const scrubbing = () => ride != null && ['Paused', 'AutoPaused', 'Finished'].includes(ride.session.phase);
  // Downsampled once per trace while the chart is frozen (paused or finished), not on every pointer move.
  let scrubCache = null;
  const scrub = attachScrub(chartBody, {
    format: mmss,
    enabled: scrubbing,
    total: () => (ride ? totalSecs() : 0),
    lookup: (t) => {
      if (!scrubCache || scrubCache.source !== points || scrubCache.n !== points.length) {
        scrubCache = { source: points, n: points.length, shown: downsample(points, TRACE_POINTS, 'watts') };
      }
      const p = nearestPoint(scrubCache.shown, t);
      if (!p) return null;
      const rec = recoveryNear(recoveries, t, Math.max(3, totalSecs() / 80));
      return { t: p.t, rows: [
        { label: 'Watt', value: String(p.watts) },
        { cls: 'tgt', label: 'Ziel', value: p.target == null ? '–' : String(p.target) },
        ...(lines.pulse ? [{ cls: 'hr', label: 'Puls', value: pulseText(p.pulse) }] : []),
        ...(lines.rpm ? [{ cls: 'cad', label: 'U/min', value: rpmValue(p.rpm) }] : []),
        ...(lines.elevation && p.elevation != null ? [{ cls: 'elev', label: 'Höhe', value: heightText(p.elevation) }] : []),
        ...(rec ? [{ cls: 'hr', label: 'Erholung', value: `${gradeText(rec.grade_x10)} (−${rec.difference})` }] : []),
      ] };
    },
  });

  // A tap on the chart switches between the normal and the large view on all devices.
  attachTap(chartBody, () => command({ type: 'toggle_view' }));

  const chart = el('div', { class: 'tile chart' });
  const legend = legendToggles(chart, lines);
  const strip = elevationStrip({ width: CHART_W, height: STRIP_H });
  const keypadMark = el('span', { class: 'lbl keypad-mark', hidden: true }, 'Tasten');
  // The menu is hidden while riding; the service page (e.g. chest strap state) stays reachable.
  const serviceIcon = svg('svg', { class: 'ico', viewBox: '0 0 24 24', 'aria-hidden': 'true' });
  serviceIcon.append(svg('path', { d: 'M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.8-3.8a6 6 0 0 1-7.9 7.9l-6.9 6.9a2.1 2.1 0 0 1-3-3l6.9-6.9a6 6 0 0 1 7.9-7.9l-3.8 3.8z' }));
  const serviceLink = el('a', { href: '#/service', class: 'ride-service', title: 'Service', 'aria-label': 'Service' }, serviceIcon);
  // Cues before power changes (program rides only); a tap cycles the mode and unlocks sound on iOS.
  let cueMode = loadCueMode();
  let cueState = null;
  const soundIcon = svg('svg', { class: 'ico', viewBox: '0 0 24 24', 'aria-hidden': 'true' });
  const soundBtn = el('button', { type: 'button', class: 'ride-sound', hidden: true, onclick: onSoundTap });
  soundBtn.append(soundIcon);
  chart.append(
    el('div', { class: 'chart-head' },
      el('span', { class: 'lbl' }, chartTitle, ' – ', legend.node),
      el('span', { class: 'chart-head-end' }, keypadMark, chartTime, soundBtn, serviceLink)),
    chartBody,
    strip.node);

  const rpm = el('span', { class: 'big' }, '–');
  const pulse = el('span', { class: 'big hr' }, '–');
  // "Gurt · Ohr 70": where the pulse comes from.
  const pulseSource = el('span', { class: 'pulse-src' });
  // "45,0 · 0,1"; on phones the distance moves to a small second line.
  const speedKmh = document.createTextNode('–');
  const speedKm = el('span', { class: 's-km' }, '0,0');
  const speed = el('span', { class: 'mid speed' }, speedKmh, el('span', { class: 's-sep' }, ' · '), speedKm);
  const energyKj = el('span', { class: 'mid e-kj' }, '0 kJ');
  const energyKcalNum = document.createTextNode('0');
  const energyKcal = el('span', { class: 'e-kcal' }, energyKcalNum, el('span', { class: 'e-unit' }, ' kcal'));
  const energyCockpit = el('span', { class: 'e-cockpit' }, cockpitText(0));
  const left = el('div', { class: 'stats' },
    el('div', { class: 'tile stat' }, el('span', { class: 'lbl' }, 'U/min'), rpm),
    el('div', { class: 'tile stat' }, el('span', { class: 'lbl' }, 'Puls'), pulse, pulseSource),
    el('div', { class: 'tile stat' }, el('span', { class: 'lbl' }, el('span', { class: 'l-wide' }, 'km/h · km'), el('span', { class: 'l-phone' }, 'km/h')), speed),
    el('div', { class: 'tile stat energy' }, el('span', { class: 'lbl' }, el('span', { class: 'l-wide' }, 'Energie'), el('span', { class: 'l-phone' }, 'kcal')), energyKj, energyKcal, energyCockpit));

  const ringBg = svg('circle', { cx: 100, cy: 100, r: RING_R, class: 'ring-bg' });
  const ringFg = svg('circle', { cx: 100, cy: 100, r: RING_R, class: 'ring-fg', transform: 'rotate(-90 100 100)', 'stroke-dasharray': `0 ${RING_C}` });
  const ringWatts = svg('text', { x: 100, y: 104, class: 'ring-watts' });
  const ringUnit = svg('text', { x: 100, y: 50, class: 'ring-unit' });
  ringUnit.textContent = 'Watt';
  const ringTarget = svg('text', { x: 100, y: 132, class: 'ring-target' });
  const ringSegment = svg('text', { x: 100, y: 154, class: 'ring-segment' });
  const ringSvg = svg('svg', { viewBox: '0 0 200 200', class: 'ring-svg' });
  ringSvg.append(ringBg, ringFg, ringUnit, ringWatts, ringTarget, ringSegment);
  const center = el('div', { class: 'ring' }, ringSvg);

  const plus = el('button', { type: 'button', class: 'btn ctl plus', 'aria-label': 'Ziel plus 10 Watt', onclick: () => command({ type: 'adjust', delta: STEP_WATTS }) }, '+');
  const pauseBtn = el('button', { type: 'button', class: 'btn ctl pause', onclick: togglePause }, 'Pause');
  const minus = el('button', { type: 'button', class: 'btn ctl minus', 'aria-label': 'Ziel minus 10 Watt', onclick: () => command({ type: 'adjust', delta: -STEP_WATTS }) }, '−');
  const stopBtn = el('button', { type: 'button', class: 'btn stop', onclick: () => { dialog.hidden = false; } }, 'Beenden');
  // A recovery measurement with the chest strap; the server says when it is possible.
  const recoveryBtn = el('button', { type: 'button', class: 'btn recovery-start', hidden: true, onclick: () => command({ type: 'recovery_start' }) }, 'Erholung messen');
  const right = el('div', { class: 'controls' }, plus, pauseBtn, minus, stopBtn, recoveryBtn);

  const hintDevice = el('div', { class: 'banner danger', hidden: true }, 'Keine Verbindung zum Ergometer – verbinde neu …');
  const hintServer = el('div', { class: 'banner danger', hidden: true }, 'Keine Verbindung zum Server – verbinde neu …');
  const hintStorage = el('div', { class: 'banner danger', hidden: true }, 'Speicherfehler – Fahrt wird nicht gespeichert');
  const hintPhase = el('div', { class: 'banner info', hidden: true });
  const hintStop = el('div', { class: 'banner info', hidden: true }, 'Weiter halten zum Beenden');
  const hintStraps = el('div', { class: 'banner info', hidden: true }, 'Mehrere Brustgurte – bitte beim Fahrer einen zuordnen');
  const hints = el('div', { class: 'hints' }, hintServer, hintDevice, hintStorage, hintPhase, hintStop, hintStraps);

  // ---- Large view: value band under the chart ----------------------------
  const bandNum = (cls) => el('span', { class: `band-num ${cls}` }, '–');
  const bandWatts = bandNum('b-watts');
  const bandTarget = el('span', {}, '–');
  const bandTargetLine = el('span', { class: 'band-sub tgt' }, 'Ziel ', bandTarget);
  const bandLabel = el('span', { class: 'lbl band-label' }, '–');
  const bandRemaining = bandNum('b-time');
  const bandPulse = bandNum('hr');
  const bandRpm = bandNum('cad');
  const band = el('div', { class: 'big-band' },
    el('div', { class: 'tile band-tile band-watts' }, el('span', { class: 'lbl' }, 'Watt'), bandWatts, bandTargetLine),
    el('div', { class: 'tile band-tile' }, bandLabel, bandRemaining),
    el('div', { class: 'tile band-tile' }, el('span', { class: 'lbl' }, 'Puls'), bandPulse),
    el('div', { class: 'tile band-tile' }, el('span', { class: 'lbl' }, 'U/min'), bandRpm));
  // The hints move here in the large view (the ride-main with the ring is hidden then).
  const bandWrap = el('div', { class: 'big-wrap' }, band);
  const rideMain = el('div', { class: 'ride-main' }, left, center, right, hints);

  const dialog = el('div', { class: 'dialog-backdrop', hidden: true },
    el('div', { class: 'dialog', role: 'dialog', 'aria-modal': 'true' },
      el('p', { class: 'dialog-text' }, 'Fahrt beenden?'),
      el('div', { class: 'dialog-actions' },
        el('button', { type: 'button', class: 'btn', onclick: () => { dialog.hidden = true; } }, 'Weiterfahren'),
        el('button', { type: 'button', class: 'btn danger', onclick: () => { dialog.hidden = true; command({ type: 'stop' }); } }, 'Beenden'))));

  const toast = el('div', { class: 'toast', role: 'status', hidden: true });

  const liveScreen = el('section', { class: 'ride' },
    chart,
    rideMain,
    bandWrap,
    dialog,
    toast);

  const message = el('section', { class: 'page center' });

  // ---- Commands ---------------------------------------------------------
  function command(cmd) {
    if (rideId == null) return;
    if (!live.send({ ...cmd, ride_id: rideId })) showToast('Keine Verbindung – Befehl nicht gesendet');
  }

  let toastTimer = null;
  function showToast(text) {
    toast.textContent = text;
    toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { toast.hidden = true; }, 3000);
  }

  function onSoundTap() {
    // A tap on locked sound only unlocks it; otherwise it switches to the next mode.
    if (cueMode !== 'off' && !sound.isUnlocked()) {
      sound.unlock();
    } else {
      cueMode = nextCueMode(cueMode);
      saveCueMode(cueMode);
      if (cueMode !== 'off') sound.unlock();
    }
    renderSound();
  }

  function renderSound() {
    const locked = cueMode !== 'off' && !sound.isUnlocked();
    soundIcon.replaceChildren(...speakerPaths(cueMode, locked).map((d) => svg('path', { d })));
    const title = locked ? 'Ton gesperrt – tippen zum Einschalten' : CUE_TITLES[cueMode];
    soundBtn.title = title;
    soundBtn.setAttribute('aria-label', title);
    soundBtn.classList.toggle('locked', locked);
    soundBtn.classList.toggle('off', cueMode === 'off');
    soundBtn.classList.toggle('beeps', cueMode === 'beeps');
  }

  function playCues() {
    const planned = planCues(cueState, ride.segments, ride.session);
    cueState = planned.state;
    if (cueMode === 'off') return;
    for (const cue of planned.cues) {
      if (cue.type === 'beep') sound.beep(cue.last);
      else if (cueMode === 'all') sound.speak(cue.text);
    }
  }

  function togglePause() {
    if (!ride) return;
    command({ type: ride.session.phase === 'Paused' ? 'resume' : 'pause' });
  }

  // ---- Trace ------------------------------------------------------------
  async function loadTrace() {
    if (rideId == null) return;
    if (loading) { reloadAgain = true; return; }
    loading = true;
    const id = rideId;
    let history = points;
    let stored = recoveries;
    try {
      const detail = await api(`/api/rides/${id}`);
      const heights = detail.elevation_m || [];
      history = detail.samples.map((s, i) => sampleToPoint(s, heights[i]));
      stored = detail.recoveries || [];
    } catch {
      // Keep the current trace; pending live points are still merged in.
    }
    loading = false;
    if (disposed) return;
    if (id === rideId) {
      points = mergeTrace(history, pending);
      recoveries = stored;
      pending = [];
      renderChart();
    }
    if (reloadAgain) { reloadAgain = false; loadTrace(); }
  }

  function addLivePoint() {
    const s = ride.session;
    const last = s.last || (view && view.last);
    if (!last) return;
    const p = { t: s.ride_secs, watts: last.actual_watts, pulse: (view && view.pulse) || 0, rpm: last.rpm, target: s.target_watts, elevation: ride.elevation_m };
    if (loading) { pending.push(p); return; }
    if (appendPoint(points, p) === 'gap') {
      // Keep the point: storage may not have it, the reload merges it in.
      pending.push(p);
      loadTrace();
    }
  }

  // ---- Rendering --------------------------------------------------------
  function show(node) {
    if (root.firstChild !== node) root.replaceChildren(node);
  }

  function showStarting() {
    message.replaceChildren(el('h1', {}, 'Starte …'));
    show(message);
  }

  function showIdle() {
    mode = 'idle';
    message.replaceChildren(
      el('h1', {}, 'Keine laufende Fahrt'),
      el('p', { class: 'muted' }, live.isOnline() ? 'Wähle eine Person und ein Programm.' : 'Verbinde mit dem Server …'),
      el('a', { href: '#/riders', class: 'btn primary' }, 'Neue Fahrt'));
    show(message);
  }

  async function showSummary(id) {
    mode = 'summary';
    message.replaceChildren(el('h1', {}, 'Fahrt beendet'), el('p', { class: 'muted' }, 'Lade Zusammenfassung …'));
    show(message);
    let detail;
    try {
      // The summary is written right after the ride ends; retry briefly until it is there.
      detail = await loadFinished(() => api(`/api/rides/${id}`));
    } catch (e) {
      if (disposed || mode !== 'summary') return;
      if (e.status === 404) {
        // The server does not keep a ride without any sample.
        message.replaceChildren(el('h1', {}, 'Fahrt beendet'),
          el('p', { class: 'muted' }, 'Keine Messpunkte – die Fahrt wurde nicht gespeichert.'),
          el('div', { class: 'actions' }, el('a', { href: '#/riders', class: 'btn primary' }, 'Neue Fahrt')));
        return;
      }
      message.replaceChildren(el('h1', {}, 'Fahrt beendet'), el('p', { class: 'error' }, e.message), summaryActions(id));
      return;
    }
    if (disposed || mode !== 'summary') return;
    const s = detail.summary;
    const pulseValue = s.avg_pulse == null ? '–' : `${s.avg_pulse} / ${whole(s.max_pulse)}`;
    const item = (label, value, unit) => el('div', { class: 'tile sum-item' },
      el('span', { class: 'lbl' }, label), el('span', { class: 'mid' }, value, unit ? el('small', {}, ` ${unit}`) : ''));
    const energyItem = (kj) => {
      const e = energy(kj);
      return el('div', { class: 'tile sum-item sum-energy' },
        el('span', { class: 'lbl' }, 'Arbeit · Verbrauch'),
        el('span', { class: 'mid' }, el('span', { class: 'e-line' }, `${e.kj} kJ`), el('span', { class: 'e-line' }, `${e.kcal} kcal`)),
        el('span', { class: 'muted energy-note' }, cockpitNote(kj)));
    };
    message.replaceChildren(
      el('h1', {}, 'Fahrt beendet'),
      el('p', { class: 'muted' }, s.program_name || 'Freie Fahrt'),
      el('div', { class: 'summary-grid' },
        item('Dauer', duration(s.ride_secs)),
        item('Ø / max Watt', `${s.avg_watts} / ${s.max_watts}`, 'W'),
        item('Ø / max Puls', pulseValue, s.avg_pulse == null ? '' : '/min'),
        item('Ø / max U/min', rpmPair(s), s.avg_rpm == null ? '' : 'U/min'),
        item('Strecke', km(s.distance_m), 'km'),
        item('Hm bergauf', whole(s.elevation_gain_m)),
        energyItem(s.energy_kj)),
      summaryActions(id));
  }

  // Shortly after the ride a strap measurement is still stored with it.
  const summaryRecoveryBtn = el('button', { type: 'button', class: 'btn', hidden: true, onclick: () => {
    if (!live.send({ type: 'recovery_start' })) showToast('Keine Verbindung – Befehl nicht gesendet');
  } }, 'Erholung messen');

  function summaryActions(id) {
    summaryRecoveryBtn.hidden = !(view && view.recovery_available);
    return el('div', { class: 'actions' },
      summaryRecoveryBtn,
      el('a', { href: `#/history/${id}`, class: 'btn' }, 'Zum Verlauf'),
      el('a', { href: '#/riders', class: 'btn primary' }, 'Neue Fahrt'));
  }

  function totalSecs() {
    const s = ride.session;
    if (ride.segments && s.program_secs) return Math.max(s.program_secs, s.ride_secs);
    return Math.max(600, s.ride_secs * 1.2);
  }

  function renderChart() {
    if (!ride) return;
    const total = totalSecs();
    const max = scaleMax(ride.segments, points);
    programArea.setAttribute('d', ride.segments ? programPath(ride.segments, total, CHART_W, CHART_H, max) : '');
    const trace = (key) => tracePath(downsample(points, TRACE_POINTS, key), key, total, CHART_W, CHART_H, max);
    targetLine.setAttribute('points', trace('target'));
    wattsLine.setAttribute('points', trace('watts'));
    pulseLine.setAttribute('points', trace('pulse'));
    const cadMax = cadenceMax(points);
    rpmLine.setAttribute('points', tracePath(downsample(points, TRACE_POINTS, 'rpm'), 'rpm', total, CHART_W, CHART_H, cadMax));
    cadence.update(cadMax);
    const x = String((ride.session.ride_secs / total) * CHART_W);
    nowLine.setAttribute('x1', x);
    nowLine.setAttribute('x2', x);
    future.setAttribute('x', x);
    future.setAttribute('width', ride.segments ? String(Math.max(0, CHART_W - Number(x))) : '0');
    grid.update(max);
    marks.update(recoveries, total);
    strip.update({
      preview: ride.elevation_preview, points: elevationPoints(points), total,
      now: ride.session.ride_secs, height: ride.elevation_m, gain: ride.elevation_gain_m,
    });
    const scrubOn = scrubbing();
    chartBody.classList.toggle('scrub-on', scrubOn);
    if (!scrubOn) scrub.hide();
  }

  function renderLive() {
    const s = ride.session;
    const last = s.last || (view && view.last);
    const program = Boolean(ride.segments);

    chartTitle.textContent = program ? `Ganzes Programm${ride.program_name ? ` „${ride.program_name}“` : ''}` : 'Freie Fahrt';
    chartTime.textContent = program && s.program_secs ? `${mmss(s.ride_secs)} / ${mmss(s.program_secs)}` : mmss(s.ride_secs);

    rpm.textContent = last ? String(last.rpm) : '–';
    // The server's choice: the strap's pulse, else the cockpit's (never its recovery drop).
    pulse.textContent = pulseText(view && view.pulse);
    pulseSource.textContent = pulseSourceText(view);
    speedKmh.data = last ? kmh(last.speed_x10) : '–';
    speedKm.textContent = km(s.distance_m);

    const e = energy(s.energy_kj);
    energyKj.textContent = `${e.kj} kJ`;
    energyKcalNum.data = String(e.kcal);
    energyCockpit.textContent = cockpitText(s.energy_kj);

    ringWatts.textContent = last ? String(last.actual_watts) : '–';
    ringTarget.textContent = `Ziel ${s.target_watts} W`;
    ringTarget.classList.toggle('overridden', s.overridden);
    let progress;
    if (program && s.segment_label != null) {
      ringSegment.textContent = `${s.segment_label} · ${mmss(s.segment_remaining_secs || 0)}`;
      progress = s.segment_progress || 0;
    } else if (program) {
      ringSegment.textContent = 'Programm zu Ende';
      progress = 1;
    } else {
      ringSegment.textContent = `Fahrzeit ${mmss(s.ride_secs)}`;
      progress = (s.ride_secs % 60) / 60;
    }
    ringFg.setAttribute('stroke-dasharray', `${(Math.min(1, Math.max(0, progress)) * RING_C).toFixed(1)} ${RING_C.toFixed(1)}`);

    const phase = s.phase;
    plus.disabled = phase !== 'Active';
    minus.disabled = phase !== 'Active';
    pauseBtn.textContent = phase === 'Paused' ? 'Weiter' : 'Pause';
    pauseBtn.classList.toggle('resume', phase === 'Paused');
    pauseBtn.disabled = !['Active', 'AutoPaused', 'Paused'].includes(phase);
    stopBtn.disabled = phase === 'Finished';
    recoveryBtn.hidden = !(view && view.recovery_available);

    hintServer.hidden = live.isOnline();
    hintDevice.hidden = !live.isOnline() || view.connected !== false;
    hintStorage.hidden = !view || !view.storage_error;
    const big = Boolean(view && view.chart_view === 'big');
    const phaseHint = s.held ? 'Erholungsmessung läuft – die Fahrt ist angehalten' : {
      Waiting: 'Bitte lostreten',
      AutoPaused: 'Pause – weitertreten zum Fortsetzen',
      Paused: pausedHint(big, Boolean(view && view.keypad)),
      Finished: 'Programm beendet – speichere …',
    }[phase];
    hintPhase.textContent = phaseHint || '';
    hintPhase.hidden = !phaseHint;
    hintStop.hidden = !(view && view.stop_hold);
    hintStraps.hidden = !(view && view.strap_note === 'ambiguous');
    liveScreen.classList.toggle('paused', phase === 'Paused' || phase === 'AutoPaused');
    keypadMark.hidden = !(view && view.keypad);
    soundBtn.hidden = !program;

    liveScreen.classList.toggle('big-view', big);
    const hintsHome = big ? bandWrap : rideMain;
    if (hints.parentNode !== hintsHome) hintsHome.append(hints);
    if (big) renderBand();

    renderChart();
  }

  function renderBand() {
    const b = bandValues(view);
    bandWatts.textContent = b.watts;
    bandTarget.textContent = b.target;
    bandTargetLine.classList.toggle('overridden', b.overridden);
    bandLabel.textContent = b.label;
    bandRemaining.textContent = b.remaining;
    // "1:02:05" needs a smaller size than "12:34" to fit the tile.
    bandRemaining.classList.toggle('long', b.remaining.length > 5);
    bandPulse.textContent = b.pulse;
    bandRpm.textContent = b.rpm;
  }

  // ---- Live view handling ----------------------------------------------
  function onView(v) {
    view = v;
    summaryRecoveryBtn.hidden = !v.recovery_available;
    // A finished recovery measurement is stored with the ride: reload to draw its line.
    recoveryStates = recoveryFinished(recoveryStates, v);
    const recoveryDone = recoveryStates.done;
    if (v.ride) {
      if (recoveryDone && v.ride.ride_id === rideId) setTimeout(() => { if (!disposed) loadTrace(); }, 1000);
      if (v.ride.ride_id !== rideId) {
        rideId = v.ride.ride_id;
        points = [];
        recoveries = [];
        pending = [];
        cueState = null;
        starting = null;
        ride = v.ride;
        mode = 'live';
        show(liveScreen);
        loadTrace();
      } else if (mode !== 'live') {
        mode = 'live';
        show(liveScreen);
      }
      ride = v.ride;
      addLivePoint();
      playCues();
      renderLive();
    } else if (rideId != null) {
      const ended = rideId;
      rideId = null;
      ride = null;
      points = [];
      dialog.hidden = true;
      showSummary(ended);
    } else if (mode === 'idle' && !starting) {
      showIdle();
    }
  }

  function onConnection() {
    if (mode === 'live' && ride) renderLive();
    else if (mode === 'idle' && !starting) showIdle();
  }

  const offs = [
    live.onLive(onView),
    live.onConnection(onConnection),
    // After a reconnect the server may have recorded samples we missed.
    live.onOpen(() => { if (rideId != null) loadTrace(); }),
    sound.onUnlockChange(renderSound),
  ];
  renderSound();

  if (starting) showStarting();
  const initial = live.current();
  if (initial) {
    onView(initial);
  } else if (!starting) {
    message.replaceChildren(el('h1', {}, 'Verbinde …'));
    show(message);
  }

  // Keep the iPad awake during training.
  let wakeLock = null;
  async function requestWakeLock() {
    if (disposed || wakeLock || !('wakeLock' in navigator) || document.visibilityState !== 'visible') return;
    try {
      const lock = await navigator.wakeLock.request('screen');
      if (disposed) { lock.release().catch(() => {}); return; }
      wakeLock = lock;
      lock.addEventListener('release', () => { if (wakeLock === lock) wakeLock = null; });
    } catch {
      // Not allowed or unsupported: nothing to do.
    }
  }
  const onVisibility = () => { if (document.visibilityState === 'visible') requestWakeLock(); };
  document.addEventListener('visibilitychange', onVisibility);
  requestWakeLock();

  return () => {
    disposed = true;
    for (const off of offs) off();
    clearTimeout(startTimer);
    clearTimeout(toastTimer);
    document.removeEventListener('visibilitychange', onVisibility);
    if (wakeLock) wakeLock.release().catch(() => {});
    wakeLock = null;
  };
}

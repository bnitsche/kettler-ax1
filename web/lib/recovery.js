// Text for the recovery (heart rate recovery) measurement of the cockpit or, with a chest strap, the server.

import { mmss, decimal1 } from './format.js';

function pulse(value) {
  return value ? String(value) : '–';
}

/** "F5,7" for a grade × 10. */
export function gradeText(gradeX10) {
  return `F${decimal1(gradeX10 / 10)}`;
}

/** "Puls 106 → 100 (−6)": P1, P2 = P1 − difference, difference. */
export function pulsesText(pulseStart, difference) {
  return `Puls ${pulseStart} → ${Math.max(0, pulseStart - difference)} (−${difference})`;
}

/** A stored recovery (`RideDetail.recoveries`): "Puls 106 → 100 (−6) · Fitnessnote F5,7 · Gurt". */
export function storedRecoveryText(r) {
  const source = r.source === 'strap' ? 'Gurt' : 'Ohr';
  return `${pulsesText(r.pulse_start, r.difference)} · Fitnessnote ${gradeText(r.grade_x10)} · ${source}`;
}

/** The stored recovery closest to `t` within `radius` seconds, or null. */
export function recoveryNear(recoveries, t, radius) {
  let best = null;
  for (const r of recoveries || []) {
    const d = Math.abs(r.ride_secs - t);
    if (d <= radius && (!best || d < Math.abs(best.ride_secs - t))) best = r;
  }
  return best;
}

/** Banner text for `LiveView.recovery`; '' when no measurement runs. */
export function recoveryText(rec) {
  if (!rec) return '';
  if (rec.state === 'waiting') return 'Erholungsmessung – warte auf Puls';
  if (rec.state === 'measuring') return `Erholungsmessung · noch ${mmss(rec.remaining_secs)} · Puls ${pulse(rec.pulse)}`;
  if (rec.state === 'failed') return 'Erholungsmessung fehlgeschlagen – kein Puls vom Gurt';
  if (rec.state === 'result') {
    const pulses = rec.pulse_start ? pulsesText(rec.pulse_start, rec.difference) : `Pulsdifferenz ${rec.difference}`;
    const grade = rec.grade_x10 != null ? ` · Fitnessnote ${gradeText(rec.grade_x10)}` : '';
    return `Erholung: ${pulses}${grade}`;
  }
  return '';
}

/** The banner: the strap measurement if there is one (with the cockpit's grade next to it), else the cockpit's. */
export function bannerText(view) {
  if (!view) return '';
  const strap = view.strap_recovery;
  if (!strap) return recoveryText(view.recovery);
  const cockpit = view.recovery;
  const extra = cockpit && cockpit.state === 'result' && cockpit.grade_x10 != null ? ` · Ohr: ${gradeText(cockpit.grade_x10)}` : '';
  return recoveryText(strap) + extra;
}

/**
 * Whether a recovery measurement, the cockpit's or the strap's, has just given a result.
 * `prev` is the previous call's return value (or null); pass the returned value on.
 */
export function recoveryFinished(prev, view) {
  const state = (rec) => (rec ? rec.state : null);
  const cockpit = state(view.recovery);
  const strap = state(view.strap_recovery);
  const fresh = (now, before) => now === 'result' && before !== 'result';
  const done = Boolean(prev) && (fresh(cockpit, prev.cockpit) || fresh(strap, prev.strap));
  return { cockpit, strap, done };
}

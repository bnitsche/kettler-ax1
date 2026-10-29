// Number and time formatting for the German UI.

const pad = (n) => String(n).padStart(2, '0');

/** Seconds as "mm:ss"; minutes are not wrapped into hours (3725 → "62:05"). */
export function mmss(secs) {
  const s = Math.max(0, Math.floor(secs || 0));
  return `${pad(Math.floor(s / 60))}:${pad(s % 60)}`;
}

/** One decimal with a decimal comma. */
export function decimal1(value) {
  return (Math.round((value || 0) * 10) / 10).toFixed(1).replace('.', ',');
}

/** Device speed in 0.1 km/h units (234 → "23,4"). */
export function kmh(speedX10) {
  return decimal1((speedX10 || 0) / 10);
}

/** Meters as kilometres (7300 → "7,3"). */
export function km(meters) {
  return decimal1((meters || 0) / 1000);
}

/** Whole number, or "–" for null/undefined. */
export function whole(value) {
  return value == null ? '–' : String(Math.round(value));
}

/** A cadence value ("70"), or "–" when there is none. */
export function rpmValue(rpm) {
  return whole(rpm);
}

/** Ride summary cadence as "Ø / max" ("78 / 92"), or "–" when the ride has no pedalling. */
export function rpmPair(summary) {
  return summary.avg_rpm == null ? '–' : `${whole(summary.avg_rpm)} / ${whole(summary.max_rpm)}`;
}

/** Unix seconds as local German date and time ("25.10.2026, 14:05"). */
export function dateTime(unixSecs) {
  const d = new Date(unixSecs * 1000);
  return `${pad(d.getDate())}.${pad(d.getMonth() + 1)}.${d.getFullYear()}, ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** Seconds as "m:ss", or "h:mm:ss" from one hour on. */
export function duration(secs) {
  const s = Math.max(0, Math.floor(secs || 0));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h > 0) return `${h}:${pad(m)}:${pad(s % 60)}`;
  return `${m}:${pad(s % 60)}`;
}

// Cycling rule of thumb: about 24 % efficiency, so 1 kJ of work is about 1 kcal of body energy.
const KCAL_PER_KJ_WORK = 1;
// The AX1 cockpit shows body energy in kJ. The probe measured +71 device units for 18 kJ of work
// (3 min at 100 W), about 25 % efficiency, so the cockpit shows roughly 4 times the work.
const COCKPIT_FACTOR = 4;

/** Mechanical work in kJ (server energy_kj) as whole kJ, kcal and the cockpit-style kJ. */
export function energy(kj) {
  const work = kj || 0;
  return {
    kj: Math.round(work),
    kcal: Math.round(work * KCAL_PER_KJ_WORK),
    cockpitKj: Math.round(work * COCKPIT_FACTOR),
  };
}

/** "198 kJ · 198 kcal". */
export function energyLine(kj) {
  const e = energy(kj);
  return `${e.kj} kJ · ${e.kcal} kcal`;
}

/** "(Cockpit ≈ 792 kJ)", the note after the work and kcal values. */
export function cockpitNote(kj) {
  return `(${cockpitText(kj)})`;
}

/** "Cockpit ≈ 792 kJ". */
export function cockpitText(kj) {
  return `Cockpit ≈ ${energy(kj).cockpitKj} kJ`;
}

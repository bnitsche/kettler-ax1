// Pure helper for the start dialog: short text lines describing a program's blocks.

import { duration, energyLine } from './format.js';
import { gainText } from './elevation.js';

function describeStep(s) {
  const watts = s.start_watts === s.end_watts ? `${s.start_watts} W` : `${s.start_watts}→${s.end_watts} W`;
  return `${s.name} ${duration(s.duration_secs)} · ${watts}`;
}

// "↗", "↘" or "↗↘" from the step levels; empty when flat or irregular.
function direction(steps) {
  const lv = steps.map((s) => (s.start_watts + s.end_watts) / 2);
  const peak = lv.indexOf(Math.max(...lv));
  const last = lv.length - 1;
  if (lv[0] === lv[last] && peak === 0) return '';
  if (peak === 0) return lv[last] < lv[0] ? '↘' : '';
  if (peak === last) return '↗';
  return lv[0] < lv[peak] && lv[last] < lv[peak] ? '↗↘' : '';
}

// A block with many steps (staircase, ramp test) is summarised instead of listed.
function describeMany(steps) {
  const secs = steps.reduce((sum, s) => sum + s.duration_secs, 0);
  const watts = steps.flatMap((s) => [s.start_watts, s.end_watts]);
  const lo = Math.min(...watts);
  const hi = Math.max(...watts);
  const hint = direction(steps);
  return `${steps.length} Abschnitte ${duration(secs)} · ${lo === hi ? lo : `${lo}–${hi}`} W${hint ? ` ${hint}` : ''}`;
}

/** One line per block: "Aufwärmen 5:00 · 60→100 W", "4 × (Intervall … / Erholung …)". */
export function describeBlocks(program) {
  const lines = [];
  for (const block of (program && program.blocks) || []) {
    const raw = block.steps || [];
    if (raw.length === 0) continue;
    const text = raw.length > 4 ? describeMany(raw) : raw.map(describeStep).join(' / ');
    lines.push((block.repeat || 1) > 1 ? `${block.repeat} × (${text})` : text);
  }
  return lines;
}

/** "Ø 125 W · max 200 W · ≈ 15 kJ · 15 kcal" for the program ridden on target; "" without segments. */
export function statsLine(segments) {
  let joules = 0;
  let secs = 0;
  let max = 0;
  for (const s of segments || []) {
    joules += s.duration_secs * (s.start_watts + s.end_watts) / 2;
    secs += s.duration_secs;
    max = Math.max(max, s.start_watts, s.end_watts);
  }
  if (secs === 0) return '';
  return `Ø ${Math.round(joules / secs)} W · max ${max} W · ≈ ${energyLine(joules / 1000)}`;
}

const pad = (n) => String(n).padStart(2, '0');

/**
 * The rider's finished rides of this program (matched by name, `rides` newest first):
 * "3× gefahren · zuletzt am 24.10.2026: Ø 118 W, Ø Puls 132, 82 Hm" or "Noch nie gefahren".
 */
export function historyText(rides, programName) {
  const done = (rides || []).filter((r) => r.finished && r.program_name === programName);
  if (done.length === 0) return 'Noch nie gefahren';
  const last = done[0];
  const d = new Date(last.started_at * 1000);
  const values = [`Ø ${last.avg_watts} W`];
  if (last.avg_pulse != null) values.push(`Ø Puls ${last.avg_pulse}`);
  values.push(gainText(last.elevation_gain_m));
  return `${done.length}× gefahren · zuletzt am ${pad(d.getDate())}.${pad(d.getMonth() + 1)}.${d.getFullYear()}: ${values.join(', ')}`;
}

// Pure helpers for the program editor: duration input, local expansion, summary.

/**
 * Duration input as seconds: "2:00" → 120, "90" → 90, "10 min" → 600, "45 s" → 45.
 * Anything else returns null.
 */
export function parseDuration(text) {
  const t = String(text ?? '').trim().toLowerCase();
  let m = t.match(/^(\d+):(\d{1,2})$/);
  if (m) return Number(m[2]) < 60 ? Number(m[1]) * 60 + Number(m[2]) : null;
  m = t.match(/^(\d+)\s*(min|m)$/);
  if (m) return Number(m[1]) * 60;
  m = t.match(/^(\d+)\s*(s|sek)?$/);
  if (m) return Number(m[1]);
  return null;
}

/** Seconds as "m:ss" (120 → "2:00"); minutes are not wrapped into hours. */
export function formatDuration(secs) {
  const s = Math.max(0, Math.floor(secs || 0));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

/** Same rules as `Program::expand` on the server, for the preview. */
export function expand(program) {
  const out = [];
  let t = 0;
  for (const block of program.blocks || []) {
    const repeat = block.repeat || 1;
    for (let i = 1; i <= repeat; i++) {
      for (const step of block.steps || []) {
        out.push({
          name: step.name,
          label: repeat > 1 ? `${step.name} ${i}/${repeat}` : step.name,
          start_secs: t,
          duration_secs: step.duration_secs,
          start_watts: step.start_watts,
          end_watts: step.end_watts,
        });
        t += step.duration_secs;
      }
    }
  }
  return out;
}

/** Total duration and number of flat segments, for display only. */
export function summary(program) {
  let totalSecs = 0;
  let segmentCount = 0;
  for (const block of program.blocks || []) {
    const repeat = block.repeat || 1;
    for (const step of block.steps || []) {
      totalSecs += repeat * step.duration_secs;
      segmentCount += repeat;
    }
  }
  return { totalSecs, segmentCount };
}

export const MAX_REPEAT = 50;
export const MAX_STEP_SECS = 7200;
export const MAX_TOTAL_SECS = 21600;
export const MAX_NAME = 60;

const wholeNumber = (text) => (/^\d+$/.test(String(text).trim()) ? Number(text) : null);

/** Name for a copy, "<Name> (Kopie)", shortened so it stays within the name limit. */
export function copyName(name) {
  const suffix = ' (Kopie)';
  return name.slice(0, MAX_NAME - suffix.length) + suffix;
}

/**
 * Editor draft (text fields: repeat, step.dur/from/to) → {program} or {error}
 * with a German message. Same limits as the server.
 */
export function toProgram(draft, limits) {
  if (!draft.name.trim()) return { error: 'Programm braucht einen Namen' };
  if (draft.blocks.length === 0) return { error: 'Programm braucht mindestens einen Block' };
  let total = 0;
  const blocks = [];
  for (const [bi, b] of draft.blocks.entries()) {
    const n = bi + 1;
    const repeat = wholeNumber(b.repeat);
    if (repeat == null || repeat < 1 || repeat > MAX_REPEAT) {
      return { error: `Block ${n}: Wiederholungen müssen zwischen 1 und ${MAX_REPEAT} liegen` };
    }
    if (b.steps.length === 0) return { error: `Block ${n} braucht mindestens einen Abschnitt` };
    const steps = [];
    let blockSecs = 0;
    for (const s of b.steps) {
      const name = s.name.trim();
      if (!name) return { error: `Block ${n}: Abschnitt braucht einen Namen` };
      const secs = parseDuration(s.dur);
      if (secs == null || secs < 1 || secs > MAX_STEP_SECS) {
        return { error: `Block ${n}, Abschnitt „${name}“: Dauer mindestens 0:01, höchstens ${formatDuration(MAX_STEP_SECS)} (${MAX_STEP_SECS / 3600} Stunden), z. B. 2:00, 90 oder 10 min` };
      }
      const from = wholeNumber(s.from);
      const to = wholeNumber(s.to);
      if (from == null || to == null || from < limits.min || from > limits.max || to < limits.min || to > limits.max) {
        return { error: `Block ${n}, Abschnitt „${name}“: Watt müssen zwischen ${limits.min} und ${limits.max} liegen` };
      }
      blockSecs += secs;
      steps.push({ name, duration_secs: secs, start_watts: from, end_watts: to });
    }
    total += blockSecs * repeat;
    blocks.push({ repeat, steps });
  }
  if (total > MAX_TOTAL_SECS) return { error: `Programm ist zu lang (höchstens ${MAX_TOTAL_SECS / 3600} Stunden)` };
  return { program: { id: draft.id ?? null, name: draft.name.trim(), blocks } };
}

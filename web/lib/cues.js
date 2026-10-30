// Announcements and beeps before a program's power changes, and the cue mode remembered in localStorage.

const STORAGE_KEY = 'kettler.cues';
// The announcement comes at 10 s; a later first view (skipped second, resume) still announces down to 8 s.
const SPEAK_FROM = 10;
const SPEAK_UNTIL = 8;
const BEEPS = [3, 2, 1];

/** Button order: announcement and beeps, beeps only, silent. */
export const CUE_MODES = ['all', 'beeps', 'off'];

/** "Noch 10 Sekunden, dann Erholung, 100 Watt"; `next` null: the program ends. Ramps name their start watts. */
export function cueText(secs, next) {
  const then = next ? `${next.name}, ${next.start_watts} Watt` : 'geschafft';
  return `Noch ${secs} Sekunden, dann ${then}`;
}

/**
 * Cues due for a live session view: `{state, cues}` with cues `{type: 'speak', text}` or `{type: 'beep', last}`.
 * `state` (null at first) remembers what was already cued in the current segment, so repeated views, a pause
 * or a reconnect never cue twice. Only active program rides cue: every segment change is announced, beeps
 * only come when the power changes (or the program ends).
 */
export function planCues(state, segments, session) {
  const index = session.segment_index;
  const remaining = session.segment_remaining_secs;
  if (!segments || index == null || remaining == null || !segments[index]) return { state, cues: [] };
  const done = state && state.index === index ? state.done : [];
  if (session.phase !== 'Active') return { state: { index, done }, cues: [] };

  const next = segments[index + 1] || null;
  const cues = [];
  const marks = [];
  if (remaining <= SPEAK_FROM && remaining >= SPEAK_UNTIL && !done.includes('speak')) {
    cues.push({ type: 'speak', text: cueText(remaining, next) });
    marks.push('speak');
  }
  // Beeps only where the power changes: there the rider has something to do.
  const change = !next || next.start_watts !== segments[index].end_watts;
  if (change && BEEPS.includes(remaining) && !done.includes(remaining)) {
    cues.push({ type: 'beep', last: remaining === 1 });
    marks.push(remaining);
  }
  // Once the beeps started the announcement would come too late.
  if (remaining < SPEAK_UNTIL && !done.includes('speak')) marks.push('speak');
  return { state: { index, done: [...done, ...marks] }, cues };
}

export function nextCueMode(mode) {
  return CUE_MODES[(CUE_MODES.indexOf(mode) + 1) % CUE_MODES.length];
}

/** Unknown or missing values fall back to "all". */
export function parseCueMode(text) {
  return CUE_MODES.includes(text) ? text : 'all';
}

const defaultStorage = () => {
  try {
    return localStorage;
  } catch {
    return undefined;
  }
};

export function loadCueMode(storage = defaultStorage()) {
  try {
    return parseCueMode(storage.getItem(STORAGE_KEY));
  } catch {
    return 'all';
  }
}

export function saveCueMode(mode, storage = defaultStorage()) {
  try {
    storage.setItem(STORAGE_KEY, mode);
  } catch {
    // Private mode or storage disabled: the choice is only kept for this page.
  }
}

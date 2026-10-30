import test from 'node:test';
import assert from 'node:assert/strict';
import { planCues, cueText, nextCueMode, parseCueMode, loadCueMode, saveCueMode, CUE_MODES } from './cues.js';

const seg = (name, start_watts, end_watts = start_watts, duration_secs = 60) => ({ name, label: name, duration_secs, start_watts, end_watts });
const segments = [seg('Aufwärmen', 60, 100), seg('Intervall', 200), seg('Erholung', 100), seg('Intervall', 100)];

const session = (segment_index, segment_remaining_secs, phase = 'Active') => ({ phase, segment_index, segment_remaining_secs });

/** Runs the views through `planCues` and collects the cues of every step. */
function run(views, segs = segments) {
  let state = null;
  return views.map((s) => {
    const r = planCues(state, segs, s);
    state = r.state;
    return r.cues;
  });
}

test('cueText names step and start watts, or the end', () => {
  assert.equal(cueText(10, seg('Erholung', 100)), 'Noch 10 Sekunden, dann Erholung, 100 Watt');
  assert.equal(cueText(9, seg('Rampe', 120, 200)), 'Noch 9 Sekunden, dann Rampe, 120 Watt');
  assert.equal(cueText(10, null), 'Noch 10 Sekunden, dann geschafft');
});

test('announces at 10 s and beeps at 3, 2, 1 before a power change', () => {
  const cues = run([12, 11, 10, 9, 4, 3, 2, 1].map((r) => session(0, r)));
  assert.deepEqual(cues, [
    [], [], [{ type: 'speak', text: 'Noch 10 Sekunden, dann Intervall, 200 Watt' }], [], [],
    [{ type: 'beep', last: false }], [{ type: 'beep', last: false }], [{ type: 'beep', last: true }],
  ]);
});

test('a skipped 10 still announces once with the real remaining time', () => {
  const cues = run([11, 9, 8, 7].map((r) => session(0, r)));
  assert.deepEqual(cues, [[], [{ type: 'speak', text: 'Noch 9 Sekunden, dann Intervall, 200 Watt' }], [], []]);
});

test('repeated views of the same second cue only once', () => {
  const cues = run([10, 10, 3, 3].map((r) => session(0, r)));
  assert.equal(cues.flat().length, 2);
});

test('only the announcement, no beeps, when the power stays the same', () => {
  // Erholung 100 W -> Intervall 100 W.
  assert.deepEqual(run([10, 3, 2, 1].map((r) => session(2, r))), [
    [{ type: 'speak', text: 'Noch 10 Sekunden, dann Intervall, 100 Watt' }], [], [], [],
  ]);
});

test('the last segment announces the end', () => {
  const cues = run([10, 1].map((r) => session(3, r)));
  assert.deepEqual(cues, [[{ type: 'speak', text: 'Noch 10 Sekunden, dann geschafft' }], [{ type: 'beep', last: true }]]);
});

test('no cues outside an active program', () => {
  assert.deepEqual(run([session(0, 10, 'Paused'), session(0, 3, 'AutoPaused'), session(0, 2, 'Waiting')]).flat(), []);
  assert.deepEqual(run([session(null, null)]).flat(), []);
  assert.deepEqual(run([session(0, 10)], null).flat(), []);
});

test('a pause does not repeat cues; resuming late only beeps', () => {
  const cues = run([session(0, 10), session(0, 10, 'Paused'), session(0, 10), session(0, 3)]);
  assert.deepEqual(cues.map((c) => c.map((x) => x.type)), [['speak'], [], [], ['beep']]);
  const late = run([session(0, 20), session(0, 20, 'Paused'), session(0, 6), session(0, 3)]);
  assert.deepEqual(late.map((c) => c.map((x) => x.type)), [[], [], [], ['beep']]);
});

test('a new segment starts fresh', () => {
  const cues = run([session(0, 10), session(1, 10)]);
  assert.deepEqual(cues.map((c) => c.map((x) => x.text)), [['Noch 10 Sekunden, dann Intervall, 200 Watt'], ['Noch 10 Sekunden, dann Erholung, 100 Watt']]);
});

test('short segments get no announcement, only beeps', () => {
  const short = [seg('Sprint', 400, 400, 5), seg('Erholung', 100)];
  const cues = run([5, 4, 3].map((r) => session(0, r)), short);
  assert.deepEqual(cues.map((c) => c.map((x) => x.type)), [[], [], ['beep']]);
});

test('the mode cycles all -> beeps -> off -> all', () => {
  assert.deepEqual(CUE_MODES, ['all', 'beeps', 'off']);
  assert.equal(nextCueMode('all'), 'beeps');
  assert.equal(nextCueMode('beeps'), 'off');
  assert.equal(nextCueMode('off'), 'all');
});

test('the stored mode falls back to all', () => {
  assert.equal(parseCueMode('beeps'), 'beeps');
  assert.equal(parseCueMode(null), 'all');
  assert.equal(parseCueMode('loud'), 'all');
});

test('the mode is remembered in the storage', () => {
  const data = new Map();
  const storage = { getItem: (k) => (data.has(k) ? data.get(k) : null), setItem: (k, v) => data.set(k, v) };
  assert.equal(loadCueMode(storage), 'all');
  saveCueMode('off', storage);
  assert.equal(loadCueMode(storage), 'off');
  assert.equal(loadCueMode({ getItem: () => { throw new Error('blocked'); } }), 'all');
  saveCueMode('off', { setItem: () => { throw new Error('blocked'); } });
});

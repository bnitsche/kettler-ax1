import { test } from 'node:test';
import assert from 'node:assert/strict';
import { describeBlocks, statsLine, historyText } from './start-preview.js';

const step = (name, secs, a, b = a) => ({ name, duration_secs: secs, start_watts: a, end_watts: b });

test('describeBlocks: ramp, flat and repeated blocks', () => {
  const program = {
    blocks: [
      { repeat: 1, steps: [step('Aufwärmen', 300, 60, 100)] },
      { repeat: 4, steps: [step('Intervall', 240, 170), step('Erholung', 180, 80)] },
      { repeat: 1, steps: [step('A', 60, 90), step('B', 3725, 100, 80)] },
    ],
  };
  assert.deepEqual(describeBlocks(program), [
    'Aufwärmen 5:00 · 60→100 W',
    '4 × (Intervall 4:00 · 170 W / Erholung 3:00 · 80 W)',
    'A 1:00 · 90 W / B 1:02:05 · 100→80 W',
  ]);
});

test('describeBlocks: empty or missing blocks', () => {
  assert.deepEqual(describeBlocks({}), []);
  assert.deepEqual(describeBlocks({ blocks: [{ repeat: 2, steps: [] }] }), []);
});

test('describeBlocks: long blocks are summarised', () => {
  const steps = [100, 120, 140, 160, 180].map((w) => step(`Stufe ${w} W`, 120, w));
  assert.deepEqual(describeBlocks({ blocks: [{ repeat: 1, steps }] }), ['5 Abschnitte 10:00 · 100–180 W ↗']);
  assert.deepEqual(describeBlocks({ blocks: [{ repeat: 2, steps }] }), ['2 × (5 Abschnitte 10:00 · 100–180 W ↗)']);
  const four = steps.slice(0, 4);
  assert.match(describeBlocks({ blocks: [{ repeat: 1, steps: four }] })[0], /^Stufe 100 W 2:00 · 100 W \//);
});

test('describeBlocks: pyramid shows rise and fall', () => {
  const steps = [110, 130, 150, 170, 150, 130, 110].map((w) => step(`S${w}`, 120, w));
  assert.deepEqual(describeBlocks({ blocks: [{ repeat: 1, steps }] }), ['7 Abschnitte 14:00 · 110–170 W ↗↘']);
});

const seg = (start, secs, a, b = a) => ({ start_secs: start, duration_secs: secs, start_watts: a, end_watts: b });

test('statsLine: average and highest target, work on target', () => {
  // 60 s at 100 W and a 60 s ramp 100→200 W: 6 + 9 = 15 kJ over 120 s.
  assert.equal(statsLine([seg(0, 60, 100), seg(60, 60, 100, 200)]), 'Ø 125 W · max 200 W · ≈ 15 kJ · 15 kcal');
  assert.equal(statsLine([]), '');
});

const ride = (startedAt, extra = {}) => ({
  program_name: 'Pyramide', finished: true, started_at: startedAt,
  avg_watts: 118, avg_pulse: 132, elevation_gain_m: 82.4, ...extra,
});

test('historyText: count and the newest finished ride of this program', () => {
  const oct24 = new Date(2026, 9, 24, 18, 0).getTime() / 1000;
  const rides = [
    ride(oct24 + 86_400, { finished: false }),
    ride(oct24),
    ride(oct24 - 86_400, { avg_watts: 100 }),
    ride(oct24 - 2 * 86_400, { program_name: 'Andere' }),
    ride(oct24 - 3 * 86_400, { avg_pulse: null }),
  ];
  assert.equal(historyText(rides, 'Pyramide'), '3× gefahren · zuletzt am 24.10.2026: Ø 118 W, Ø Puls 132, 82 Hm');
  assert.equal(historyText([ride(oct24, { avg_pulse: null })], 'Pyramide'), '1× gefahren · zuletzt am 24.10.2026: Ø 118 W, 82 Hm');
  assert.equal(historyText(rides, 'Neu'), 'Noch nie gefahren');
  assert.equal(historyText([], 'Pyramide'), 'Noch nie gefahren');
});

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseDuration, summary, expand, formatDuration, toProgram, copyName } from './program-edit.js';

test('parseDuration', () => {
  assert.equal(parseDuration('2:00'), 120);
  assert.equal(parseDuration('90'), 90);
  assert.equal(parseDuration('10 min'), 600);
  assert.equal(parseDuration('abc'), null);
});

test('parseDuration edge cases', () => {
  assert.equal(parseDuration(' 1:30 '), 90);
  assert.equal(parseDuration('10min'), 600);
  assert.equal(parseDuration('45 s'), 45);
  assert.equal(parseDuration(''), null);
  assert.equal(parseDuration('1:75'), null);
  assert.equal(parseDuration('-5'), null);
  assert.equal(parseDuration('1:2:3'), null);
});

test('formatDuration', () => {
  assert.equal(formatDuration(120), '2:00');
  assert.equal(formatDuration(65), '1:05');
  assert.equal(formatDuration(3725), '62:05');
});

const example = { blocks: [
  { repeat: 1, steps: [{ duration_secs: 600 }] },
  { repeat: 5, steps: [{ duration_secs: 120 }, { duration_secs: 120 }] },
  { repeat: 1, steps: [{ duration_secs: 300 }] }] };

test('summary counts repeats', () => {
  assert.deepEqual(summary(example), { totalSecs: 2100, segmentCount: 12 });
});

test('expand labels, times and watts', () => {
  const segs = expand({ blocks: [
    { repeat: 1, steps: [{ name: 'Aufwärmen', duration_secs: 600, start_watts: 60, end_watts: 100 }] },
    { repeat: 2, steps: [
      { name: 'Hart', duration_secs: 120, start_watts: 150, end_watts: 150 },
      { name: 'Locker', duration_secs: 120, start_watts: 80, end_watts: 80 }] }] });
  assert.deepEqual(segs.map((s) => s.label), ['Aufwärmen', 'Hart 1/2', 'Locker 1/2', 'Hart 2/2', 'Locker 2/2']);
  assert.deepEqual(segs.map((s) => s.start_secs), [0, 600, 720, 840, 960]);
  assert.equal(segs[0].end_watts, 100);
  assert.equal(segs[1].name, 'Hart');
  assert.equal(segs.length, summary({ blocks: [
    { repeat: 1, steps: [{ duration_secs: 1 }] }, { repeat: 2, steps: [{ duration_secs: 1 }, { duration_secs: 1 }] }] }).segmentCount);
});

const limits = { min: 25, max: 400 };
const step = (o = {}) => ({ name: 'Hart', dur: '2:00', from: '150', to: '150', ...o });
const draft = (o = {}) => ({ id: null, name: 'Test', blocks: [{ repeat: '1', steps: [step()] }], ...o });
const err = (d) => toProgram(d, limits).error;

test('toProgram converts a valid draft', () => {
  const r = toProgram(draft({ id: 3, name: ' Test ', blocks: [{ repeat: '5', steps: [step({ name: ' Hart ', dur: '10 min', from: '60', to: '100' })] }] }), limits);
  assert.deepEqual(r.program, { id: 3, name: 'Test', blocks: [{ repeat: 5, steps: [
    { name: 'Hart', duration_secs: 600, start_watts: 60, end_watts: 100 }] }] });
});

test('toProgram name and structure', () => {
  assert.match(err(draft({ name: '  ' })), /Namen/);
  assert.match(err(draft({ blocks: [] })), /mindestens einen Block/);
  assert.match(err(draft({ blocks: [{ repeat: '1', steps: [] }] })), /Block 1 braucht mindestens einen Abschnitt/);
  assert.match(err(draft({ blocks: [{ repeat: '1', steps: [step({ name: '' })] }] })), /Abschnitt braucht einen Namen/);
});

test('toProgram repeat boundaries', () => {
  const rep = (r) => err(draft({ blocks: [{ repeat: r, steps: [step()] }] }));
  assert.match(rep('0'), /Wiederholungen/);
  assert.match(rep('51'), /Wiederholungen/);
  assert.match(rep('x'), /Wiederholungen/);
  assert.equal(rep('1'), undefined);
  assert.equal(rep('50'), undefined);
});

test('toProgram duration boundaries', () => {
  const dur = (d) => err(draft({ blocks: [{ repeat: '1', steps: [step({ dur: d })] }] }));
  assert.match(dur('7201'), /120:00 \(2 Stunden\)/);
  assert.match(dur('0'), /Dauer/);
  assert.match(dur('abc'), /Dauer/);
  assert.equal(dur('7200'), undefined);
  assert.equal(dur('1'), undefined);
});

test('toProgram watt boundaries', () => {
  const w = (from, to = from) => err(draft({ blocks: [{ repeat: '1', steps: [step({ from, to })] }] }));
  assert.match(w('24'), /zwischen 25 und 400/);
  assert.match(w('401'), /zwischen 25 und 400/);
  assert.match(w('100', '401'), /zwischen 25 und 400/);
  assert.match(w(''), /zwischen 25 und 400/);
  assert.equal(w('25'), undefined);
  assert.equal(w('400'), undefined);
});

test('toProgram total limit of 6 h', () => {
  const total = (repeat, dur) => err(draft({ blocks: [{ repeat, steps: [step({ dur })] }] }));
  assert.match(total('50', '7200'), /zu lang/);
  assert.equal(total('3', '7200'), undefined);
  assert.match(total('4', '5401'), /zu lang/);
  assert.equal(total('4', '5400'), undefined);
});

test('copyName stays within 60 characters', () => {
  assert.equal(copyName('Test'), 'Test (Kopie)');
  const long = copyName('x'.repeat(60));
  assert.equal(long.length, 60);
  assert.ok(long.endsWith(' (Kopie)'));
});

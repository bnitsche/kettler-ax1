import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseLines, loadLines, saveLines, DEFAULT_LINES } from './lines.js';

const memory = (initial) => {
  const data = new Map(initial ? [['kettler.chart_lines', initial]] : []);
  return { getItem: (k) => (data.has(k) ? data.get(k) : null), setItem: (k, v) => data.set(k, v), data };
};

test('defaults show all lines', () => {
  assert.deepEqual(DEFAULT_LINES, { rpm: true, pulse: true, elevation: true });
  assert.deepEqual(loadLines(memory()), { rpm: true, pulse: true, elevation: true });
});

test('a choice saved before the elevation strip existed shows the strip', () => {
  assert.deepEqual(loadLines(memory('{"rpm":false,"pulse":true}')), { rpm: false, pulse: true, elevation: true });
});

test('parseLines takes booleans and falls back for anything else', () => {
  assert.deepEqual(parseLines('{"rpm":false,"pulse":true,"elevation":false}'), { rpm: false, pulse: true, elevation: false });
  assert.deepEqual(parseLines('{"rpm":"no","elevation":1}'), { rpm: true, pulse: true, elevation: true });
  assert.deepEqual(parseLines('not json'), { rpm: true, pulse: true, elevation: true });
  assert.deepEqual(parseLines('null'), { rpm: true, pulse: true, elevation: true });
  assert.deepEqual(parseLines(null), { rpm: true, pulse: true, elevation: true });
});

test('saved choice is loaded again', () => {
  const store = memory();
  saveLines({ rpm: false, pulse: true, elevation: false }, store);
  assert.deepEqual(loadLines(store), { rpm: false, pulse: true, elevation: false });
  saveLines({ rpm: true, pulse: false, elevation: true }, store);
  assert.deepEqual(loadLines(store), { rpm: true, pulse: false, elevation: true });
});

test('a throwing storage never breaks loading or saving', () => {
  const broken = { getItem() { throw new Error('denied'); }, setItem() { throw new Error('denied'); } };
  assert.deepEqual(loadLines(broken), { rpm: true, pulse: true, elevation: true });
  assert.doesNotThrow(() => saveLines({ rpm: false, pulse: false, elevation: false }, broken));
  assert.deepEqual(loadLines(undefined), { rpm: true, pulse: true, elevation: true });
});

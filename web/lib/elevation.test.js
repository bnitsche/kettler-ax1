import { test } from 'node:test';
import assert from 'node:assert/strict';
import { elevationRange, elevationSteps, elevationPath, elevationArea, heightText, gainText } from './elevation.js';

test('an empty range starts at 0 and spans 10 m plus the margin', () => {
  assert.deepEqual(elevationRange([], []), { min: 0, max: 10.5 });
  assert.deepEqual(elevationRange(null, undefined), { min: 0, max: 10.5 });
});

test('the range covers the preview and the ridden heights', () => {
  const r = elevationRange([[0, 0], [10, 60], [20, 121]], [{ t: 0, h: 0 }, { t: 1, h: 50 }]);
  assert.equal(r.min, 0);
  assert.ok(r.max >= 121, `${r.max}`);
  assert.ok(r.max <= 121 * 1.1, `${r.max}`);
  const ridden = elevationRange([[0, 0], [10, 20]], [{ t: 0, h: 0 }, { t: 5, h: 80 }]);
  assert.ok(ridden.max >= 80, `${ridden.max}`);
});

test('below the start the minimum goes negative, still with at least 10 m span', () => {
  const r = elevationRange([], [{ t: 0, h: 0 }, { t: 5, h: -3 }]);
  assert.ok(r.min <= -3, `${r.min}`);
  assert.ok(r.max - r.min >= 10, `${r.max - r.min}`);
});

test('missing heights are ignored', () => {
  assert.deepEqual(elevationRange([], [{ t: 0, h: null }, { t: 1 }]), { min: 0, max: 10.5 });
});

test('heights and gains are rounded whole numbers', () => {
  assert.equal(heightText(61.6), '62 m');
  assert.equal(heightText(-0.4), '0 m');
  assert.equal(heightText(null), '–');
  assert.equal(gainText(120.4), '120 Hm');
  assert.equal(gainText(121), '121 Hm');
  assert.equal(gainText(undefined), '–');
});

test('axis steps are one or two round values inside the range', () => {
  assert.deepEqual(elevationSteps({ min: 0, max: 10.5 }), [10]);
  assert.deepEqual(elevationSteps({ min: 0, max: 127 }), [100]);
  assert.deepEqual(elevationSteps({ min: -0.3, max: 127 }), [0, 100]);
  assert.deepEqual(elevationSteps({ min: -6, max: 12 }), [0, 10]);
});

test('elevationPath scales {t, h} into the strip and skips missing heights', () => {
  const range = { min: 0, max: 100 };
  assert.equal(elevationPath([{ t: 0, h: 0 }, { t: 50, h: null }, { t: 100, h: 50 }], 100, 1000, 60, range), '0,60 1000,30');
  assert.equal(elevationPath([], 100, 1000, 60, range), '');
});

test('elevationArea closes the path down to the bottom', () => {
  const range = { min: 0, max: 100 };
  assert.equal(elevationArea([{ t: 0, h: 0 }, { t: 100, h: 50 }], 100, 1000, 60, range), 'M0,60 L0,60 1000,30 1000,60 Z');
  assert.equal(elevationArea([], 100, 1000, 60, range), '');
});

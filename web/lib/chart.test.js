import { test } from 'node:test';
import assert from 'node:assert/strict';
import { programPath, tracePath, scaleMax, appendPoint, mergeTrace, compareScale, downsample, gridSteps, nearestPoint, scrubTime, tooltipLeft, isHorizontalDrag, medianSpacing, cadenceMax, cadenceSteps, isTap } from './chart.js';

test('scaleMax rounds up', () => {
  assert.equal(scaleMax([{ start_watts: 150, end_watts: 150 }], []), 200);
  assert.equal(scaleMax([{ start_watts: 230, end_watts: 230 }], [{ watts: 260 }]), 300);
});

test('scaleMax without program uses points and pulse', () => {
  assert.equal(scaleMax(null, [{ watts: 120, pulse: 251, target: 100 }]), 300);
});

test('programPath starts and ends on baseline', () => {
  const p = programPath([{ start_secs: 0, duration_secs: 100, start_watts: 100, end_watts: 100 }], 100, 300, 80, 200);
  assert.match(p, /^M0,80 /);
  assert.match(p, /300,80 Z$/);
  assert.match(p, /L0,40 300,40 /);
});

test('programPath draws ramps as slopes', () => {
  const p = programPath([{ start_secs: 0, duration_secs: 50, start_watts: 0, end_watts: 200 }], 100, 100, 80, 200);
  assert.equal(p, 'M0,80 L0,80 50,0 50,80 Z');
});

test('tracePath maps points', () => {
  const pts = [{ t: 0, watts: 100 }, { t: 50, watts: 200 }];
  assert.equal(tracePath(pts, 'watts', 100, 300, 80, 200), '0,40 150,0');
});

test('tracePath skips missing and zero pulse values', () => {
  const pts = [{ t: 0, pulse: 0 }, { t: 50, pulse: 100 }];
  assert.equal(tracePath(pts, 'pulse', 100, 300, 80, 200), '150,40');
});

test('appendPoint appends, drops duplicates and detects gaps', () => {
  const pts = [{ t: 1 }];
  assert.equal(appendPoint(pts, { t: 2 }), 'added');
  assert.equal(appendPoint(pts, { t: 2 }), 'duplicate');
  assert.equal(appendPoint(pts, { t: 1 }), 'duplicate');
  assert.equal(appendPoint(pts, { t: 4 }), 'added');
  assert.equal(appendPoint(pts, { t: 7 }), 'gap');
  assert.deepEqual(pts.map((p) => p.t), [1, 2, 4]);
});

test('appendPoint on an empty trace with a late start is a gap', () => {
  assert.equal(appendPoint([], { t: 1 }), 'added');
  assert.equal(appendPoint([], { t: 10 }), 'gap');
});

test('mergeTrace keeps a live point the history is missing', () => {
  const history = [{ t: 1, watts: 100 }, { t: 2, watts: 100 }];
  const merged = mergeTrace(history, [{ t: 6, watts: 120 }]);
  assert.deepEqual(merged.map((p) => p.t), [1, 2, 6]);
  // The next live point continues without another gap.
  assert.equal(appendPoint(merged, { t: 7, watts: 120 }), 'added');
});

test('mergeTrace with empty history accepts a late first point', () => {
  const merged = mergeTrace([], [{ t: 10, watts: 80 }]);
  assert.deepEqual(merged, [{ t: 10, watts: 80 }]);
  assert.equal(appendPoint(merged, { t: 11 }), 'added');
});

test('mergeTrace drops duplicates, history wins, result sorted', () => {
  const history = [{ t: 3, watts: 1 }, { t: 4, watts: 1 }];
  const pending = [{ t: 5, watts: 2 }, { t: 4, watts: 2 }, { t: 5, watts: 2 }];
  const merged = mergeTrace(history, pending);
  assert.deepEqual(merged, [{ t: 3, watts: 1 }, { t: 4, watts: 1 }, { t: 5, watts: 2 }]);
});

test('compareScale uses the longer ride and the common watt maximum', () => {

  const a = [{ t: 0, watts: 100 }, { t: 60, watts: 180 }];
  const b = [{ t: 0, watts: 90 }, { t: 90, watts: 320 }];
  assert.deepEqual(compareScale(a, b), { total: 90, max: 350 });
  assert.deepEqual(compareScale([], []), { total: 60, max: 200 });
});

test('downsample keeps small series and the peaks of large ones', () => {
  const small = [{ t: 0, watts: 1 }, { t: 1, watts: 2 }];
  assert.equal(downsample(small, 10, 'watts'), small);
  const big = Array.from({ length: 10000 }, (_, i) => ({ t: i, watts: i === 4321 ? 999 : (i === 777 ? -5 : 100) }));
  const out = downsample(big, 200, 'watts');
  assert.ok(out.length <= 202 && out.length >= 100);
  assert.ok(out.some((p) => p.watts === 999));
  assert.ok(out.some((p) => p.watts === -5));
  assert.equal(out[0].t, 0);
  assert.equal(out[out.length - 1].t, 9999);
  for (let i = 1; i < out.length; i++) assert.ok(out[i].t > out[i - 1].t);
});

test('downsample skips points without the key', () => {
  const pts = Array.from({ length: 50 }, (_, i) => ({ t: i, watts: i, pulse: i % 2 ? null : 80 }));
  const out = downsample(pts, 10, 'pulse');
  assert.ok(out.every((p) => p.pulse != null));
});

test('gridSteps uses 50 W up to a max of 300 and 100 W above', () => {
  assert.deepEqual(gridSteps(200), [50, 100, 150, 200]);
  assert.deepEqual(gridSteps(300), [50, 100, 150, 200, 250, 300]);
  assert.deepEqual(gridSteps(350), [100, 200, 300]);
  assert.deepEqual(gridSteps(400), [100, 200, 300, 400]);
  assert.deepEqual(gridSteps(30), []);
  assert.deepEqual(gridSteps(0), []);
});

test('nearestPoint finds the closest point by time', () => {
  const pts = [{ t: 0 }, { t: 10 }, { t: 20 }, { t: 40 }];
  assert.equal(nearestPoint(pts, -5).t, 0);
  assert.equal(nearestPoint(pts, 4).t, 0);
  assert.equal(nearestPoint(pts, 6).t, 10);
  assert.equal(nearestPoint(pts, 21).t, 20);
  assert.equal(nearestPoint(pts, 31).t, 40);
  assert.equal(nearestPoint(pts, 999).t, 40);
  assert.equal(nearestPoint([{ t: 7 }], 100).t, 7);
  assert.equal(nearestPoint([], 5), null);
  assert.equal(nearestPoint(null, 5), null);
});

test('nearestPoint prefers the earlier point on a tie', () => {
  assert.equal(nearestPoint([{ t: 0 }, { t: 10 }], 5).t, 0);
});

test('scrubTime maps x to a time and clamps to the chart', () => {
  assert.equal(scrubTime(50, 100, 600), 300);
  assert.equal(scrubTime(-10, 100, 600), 0);
  assert.equal(scrubTime(500, 100, 600), 600);
  assert.equal(scrubTime(10, 0, 600), 0);
});

test('tooltipLeft puts the box right of the cursor and flips near the right edge', () => {
  assert.equal(tooltipLeft(100, 500, 120), 112);
  assert.equal(tooltipLeft(450, 500, 120), 318);
});

test('tooltipLeft stays inside the chart', () => {
  assert.equal(tooltipLeft(5, 100, 120), 0);
  assert.equal(tooltipLeft(60, 130, 120), 0);
});

test('isHorizontalDrag needs a mostly horizontal move of more than 6 px', () => {
  assert.equal(isHorizontalDrag(10, 2), true);
  assert.equal(isHorizontalDrag(-10, 2), true);
  assert.equal(isHorizontalDrag(5, 0), false);
  assert.equal(isHorizontalDrag(10, 12), false);
  assert.equal(isHorizontalDrag(8, 8), false);
});

test('medianSpacing ignores outlier gaps', () => {
  assert.equal(medianSpacing([{ t: 0 }, { t: 1 }, { t: 2 }, { t: 50 }]), 1);
  assert.equal(medianSpacing([{ t: 0 }, { t: 2 }, { t: 6 }]), 3);
  assert.equal(medianSpacing([{ t: 0 }]), 0);
  assert.equal(medianSpacing(null), 0);
});

test('cadenceMax is 120 at least and rounds the highest rpm up to 10', () => {
  assert.equal(cadenceMax([]), 120);
  assert.equal(cadenceMax(null), 120);
  assert.equal(cadenceMax([{ rpm: 70 }, { rpm: 120 }]), 120);
  assert.equal(cadenceMax([{ rpm: 121 }]), 130);
  assert.equal(cadenceMax([{ rpm: 140 }, { watts: 5 }, { rpm: null }]), 140);
});

test('cadenceSteps gives 2 to 3 axis labels', () => {
  assert.deepEqual(cadenceSteps(120), [60, 90, 120]);
  assert.deepEqual(cadenceSteps(130), [50, 100]);
  assert.deepEqual(cadenceSteps(150), [50, 100, 150]);
  for (let m = 120; m <= 250; m += 10) {
    const steps = cadenceSteps(m);
    assert.ok(steps.length >= 2 && steps.length <= 3, `${m}: ${steps}`);
    assert.ok(steps.every((v) => v <= m));
  }
});

test('tracePath draws zero rpm (not pedalling) but skips missing values', () => {
  const pts = [{ t: 0, rpm: 0 }, { t: 50, rpm: 60 }, { t: 100 }];
  assert.equal(tracePath(pts, 'rpm', 100, 300, 80, 120), '0,80 150,40');
});

test('downsample works on rpm and keeps its peak', () => {
  const big = Array.from({ length: 5000 }, (_, i) => ({ t: i, rpm: i === 2500 ? 130 : 70 }));
  const out = downsample(big, 200, 'rpm');
  assert.ok(out.some((p) => p.rpm === 130));
});

test('isTap: a short press without moving is a tap', () => {
  assert.equal(isTap(3, 4, 200), true);
  assert.equal(isTap(11, 0, 100), false);
  assert.equal(isTap(0, 0, 600), false);
});

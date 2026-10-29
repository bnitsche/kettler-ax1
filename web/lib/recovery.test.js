import { test } from 'node:test';
import assert from 'node:assert/strict';
import { recoveryText, gradeText, storedRecoveryText, recoveryNear, bannerText, recoveryFinished } from './recovery.js';

test('recoveryText is empty without a measurement', () => {
  assert.equal(recoveryText(null), '');
  assert.equal(recoveryText(undefined), '');
});

test('recoveryText shows waiting, countdown and result', () => {
  assert.equal(recoveryText({ state: 'waiting' }), 'Erholungsmessung – warte auf Puls');
  assert.equal(
    recoveryText({ state: 'measuring', remaining_secs: 42, pulse_start: 106, pulse: 103 }),
    'Erholungsmessung · noch 00:42 · Puls 103',
  );
  assert.equal(
    recoveryText({ state: 'measuring', remaining_secs: 59, pulse_start: null, pulse: null }),
    'Erholungsmessung · noch 00:59 · Puls –',
  );
  assert.equal(
    recoveryText({ state: 'result', pulse_start: 106, difference: 6, grade_x10: 57 }),
    'Erholung: Puls 106 → 100 (−6) · Fitnessnote F5,7',
  );
  assert.equal(
    recoveryText({ state: 'result', pulse_start: 150, difference: 30, grade_x10: 20 }),
    'Erholung: Puls 150 → 120 (−30) · Fitnessnote F2,0',
  );
  assert.equal(
    recoveryText({ state: 'result', pulse_start: null, difference: 6, grade_x10: null }),
    'Erholung: Pulsdifferenz 6',
  );
});

test('stored recoveries: text and the one near a time', () => {
  const a = { ride_secs: 100, pulse_start: 106, difference: 6, grade_x10: 57 };
  const b = { ride_secs: 130, pulse_start: 150, difference: 30, grade_x10: 20 };
  assert.equal(gradeText(57), 'F5,7');
  assert.equal(storedRecoveryText(b), 'Puls 150 → 120 (−30) · Fitnessnote F2,0 · Ohr');
  assert.equal(storedRecoveryText({ ...b, source: 'strap' }), 'Puls 150 → 120 (−30) · Fitnessnote F2,0 · Gurt');
  assert.equal(recoveryNear([a, b], 104, 5), a);
  assert.equal(recoveryNear([a, b], 126, 5), b);
  assert.equal(recoveryNear([a, b], 115, 5), null);
  assert.equal(recoveryNear(undefined, 100, 5), null);
});

test('recoveryText for a failed strap measurement', () => {
  assert.equal(recoveryText({ state: 'failed' }), 'Erholungsmessung fehlgeschlagen – kein Puls vom Gurt');
});

test('bannerText prefers the strap measurement and adds the cockpit result', () => {
  const measuring = { state: 'measuring', remaining_secs: 42, pulse_start: 120, pulse: 110 };
  const cockpitResult = { state: 'result', pulse_start: 106, difference: 6, grade_x10: 57 };
  assert.equal(bannerText({ strap_recovery: measuring, recovery: { state: 'waiting' } }), 'Erholungsmessung · noch 00:42 · Puls 110');
  const strapResult = { state: 'result', pulse_start: 120, difference: 20, grade_x10: 22 };
  assert.equal(
    bannerText({ strap_recovery: strapResult, recovery: cockpitResult }),
    'Erholung: Puls 120 → 100 (−20) · Fitnessnote F2,2 · Ohr: F5,7',
  );
  assert.equal(bannerText({ strap_recovery: null, recovery: cockpitResult }), 'Erholung: Puls 106 → 100 (−6) · Fitnessnote F5,7');
  assert.equal(bannerText({ strap_recovery: null, recovery: null }), '');
  assert.equal(bannerText(null), '');
});

test('recoveryFinished reports a new result from the cockpit or the strap once', () => {
  const measuring = { state: 'measuring', remaining_secs: 10, pulse_start: 120, pulse: 110 };
  const result = { state: 'result', pulse_start: 120, difference: 20, grade_x10: 26 };
  let prev = recoveryFinished(null, {});
  assert.equal(prev.done, false);
  prev = recoveryFinished(prev, { strap_recovery: measuring });
  assert.equal(prev.done, false);
  prev = recoveryFinished(prev, { strap_recovery: result });
  assert.equal(prev.done, true, 'strap result');
  prev = recoveryFinished(prev, { strap_recovery: result });
  assert.equal(prev.done, false, 'only once');
  prev = recoveryFinished(prev, { strap_recovery: result, recovery: result });
  assert.equal(prev.done, true, 'cockpit result');
  prev = recoveryFinished(prev, { strap_recovery: { state: 'failed' } });
  assert.equal(prev.done, false);
});

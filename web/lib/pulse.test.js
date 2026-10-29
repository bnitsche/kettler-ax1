import { test } from 'node:test';
import assert from 'node:assert/strict';
import { samplePulse, pulseSourceText, pulseBySource, strapStateText } from './pulse.js';

const sample = (cockpit, strap) => ({ status: { pulse: cockpit }, strap_pulse: strap });

test('samplePulse prefers the strap and skips zero', () => {
  assert.equal(samplePulse(sample(70, 95)), 95);
  assert.equal(samplePulse(sample(70, null)), 70);
  assert.equal(samplePulse(sample(70, 0)), 70);
  assert.equal(samplePulse(sample(0, null)), null);
  assert.equal(samplePulse(sample(0, undefined)), null);
});

test('pulseSourceText names the source and the other value', () => {
  const last = (pulse, extra = {}) => ({ pulse, recovery: false, elapsed_secs: 10, ...extra });
  assert.equal(pulseSourceText({ pulse: 95, strap: { pulse: 95 }, last: last(70) }), 'Gurt · Ohr 70');
  assert.equal(pulseSourceText({ pulse: 95, strap: { pulse: 95 }, last: last(0) }), 'Gurt');
  assert.equal(pulseSourceText({ pulse: 70, strap: { pulse: null }, last: last(70) }), 'Ohr');
  assert.equal(pulseSourceText({ pulse: 70, strap: null, last: last(70) }), 'Ohr');
  assert.equal(pulseSourceText({ pulse: null, strap: null, last: last(0) }), '');
  // The cockpit's recovery result is a pulse drop, not a pulse.
  const drop = last(6, { recovery: true, elapsed_secs: 0 });
  assert.equal(pulseSourceText({ pulse: 95, strap: { pulse: 95 }, last: drop }), 'Gurt');
  // The ride's session has the newest status.
  assert.equal(pulseSourceText({ pulse: 95, strap: { pulse: 95 }, last: null, ride: { session: { last: last(71) } } }), 'Gurt · Ohr 71');
  assert.equal(pulseSourceText(null), '');
});

test('pulseBySource rounds like the server and skips missing values', () => {
  const samples = [sample(80, 100), sample(81, 101), sample(0, null), sample(0, 90)];
  assert.deepEqual(pulseBySource(samples), { strap: { avg: 97, max: 101 }, cockpit: { avg: 81, max: 81 } });
  assert.deepEqual(pulseBySource([sample(70, null)]), { strap: null, cockpit: { avg: 70, max: 70 } });
  assert.deepEqual(pulseBySource([]), { strap: null, cockpit: null });
});

test('strapStateText', () => {
  assert.equal(strapStateText({ link: 'connected', pulse: 72, battery: 80 }), 'verbunden · Puls 72 · Akku 80 %');
  assert.equal(strapStateText({ link: 'connected', pulse: null, battery: null }), 'verbunden · kein Puls');
  assert.equal(strapStateText({ link: 'searching' }), 'sucht …');
  assert.equal(strapStateText({ link: 'out_of_range' }), 'nicht in Reichweite');
});

test('strapStateText shows why connecting failed', () => {
  assert.equal(strapStateText({ link: 'searching', problem: 'failed: Verbinden' }), 'sucht … · zuletzt: failed: Verbinden');
  assert.equal(strapStateText({ link: 'out_of_range', problem: 'failed: Verbinden' }), 'nicht in Reichweite · zuletzt: failed: Verbinden');
  assert.equal(strapStateText({ link: 'connected', pulse: 72, battery: null, problem: null }), 'verbunden · Puls 72');
});

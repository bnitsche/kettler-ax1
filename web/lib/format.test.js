import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mmss, kmh, km, dateTime, duration, energy, energyLine, cockpitText, cockpitNote, rpmPair, rpmValue } from './format.js';

test('rpmPair shows average and maximum, or a dash without pedalling', () => {
  assert.equal(rpmPair({ avg_rpm: 78, max_rpm: 92 }), '78 / 92');
  assert.equal(rpmPair({ avg_rpm: 78.4, max_rpm: 92 }), '78 / 92');
  assert.equal(rpmPair({ avg_rpm: null, max_rpm: null }), '–');
  assert.equal(rpmPair({}), '–');
});

test('rpmValue is the whole number or a dash', () => {
  assert.equal(rpmValue(70), '70');
  assert.equal(rpmValue(0), '0');
  assert.equal(rpmValue(null), '–');
  assert.equal(rpmValue(undefined), '–');
});

test('mmss', () => {
  assert.equal(mmss(725), '12:05');
  assert.equal(mmss(5), '00:05');
  assert.equal(mmss(3725), '62:05');
});

test('kmh/km', () => {
  assert.equal(kmh(234), '23,4');
  assert.equal(km(7300), '7,3');
});

test('km rounds to one decimal', () => {
  assert.equal(km(0), '0,0');
  assert.equal(km(12345), '12,3');
});

test('dateTime formats German local date and time', () => {
  const unix = new Date(2026, 9, 25, 14, 5).getTime() / 1000;
  assert.equal(dateTime(unix), '25.10.2026, 14:05');
  assert.equal(dateTime(new Date(2026, 0, 3, 7, 9).getTime() / 1000), '03.01.2026, 07:09');
});

test('duration is m:ss, or h:mm:ss from one hour', () => {
  assert.equal(duration(65), '1:05');
  assert.equal(duration(0), '0:00');
  assert.equal(duration(725), '12:05');
  assert.equal(duration(3599), '59:59');
  assert.equal(duration(3600), '1:00:00');
  assert.equal(duration(3725), '1:02:05');
});

test('energy: work, kcal and cockpit kJ', () => {
  assert.deepEqual(energy(0), { kj: 0, kcal: 0, cockpitKj: 0 });
  assert.deepEqual(energy(null), { kj: 0, kcal: 0, cockpitKj: 0 });
  assert.deepEqual(energy(18.0), { kj: 18, kcal: 18, cockpitKj: 72 });
  assert.deepEqual(energy(198.4), { kj: 198, kcal: 198, cockpitKj: 794 });
  assert.deepEqual(energy(0.6), { kj: 1, kcal: 1, cockpitKj: 2 });
});

test('energy texts', () => {
  assert.equal(energyLine(198), '198 kJ · 198 kcal');
  assert.equal(cockpitText(198), 'Cockpit ≈ 792 kJ');
  assert.equal(energyLine(0), '0 kJ · 0 kcal');
  assert.equal(cockpitNote(198), '(Cockpit ≈ 792 kJ)');
});

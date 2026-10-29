import { test } from 'node:test';
import assert from 'node:assert/strict';
import { durationText, bytesText, traceLine, keysText, countText, bluetoothText } from './service-format.js';

test('durationText rounds to minutes, hours and days', () => {
  assert.equal(durationText(30_000), 'unter 1 min');
  assert.equal(durationText(14 * 60_000), '14 min');
  assert.equal(durationText(125 * 60_000), '2 h 5 min');
  assert.equal(durationText(3 * 86_400_000), '3 Tage');
  assert.equal(durationText(86_400_000), '1 Tag');
});

test('bytesText uses German decimals', () => {
  assert.equal(bytesText(0), '0 B');
  assert.equal(bytesText(812_000), '812 kB');
  assert.equal(bytesText(2_400_000), '2,4 MB');
});

test('traceLine formats time, class and prefix', () => {
  const at = new Date(2026, 9, 27, 18, 22, 1, 120).getTime();
  assert.deepEqual(traceLine({ at_ms: at, kind: 'sent', text: 'ST' }), { time: '18:22:01.120', cls: 'sent', text: '→ ST' });
  assert.deepEqual(traceLine({ at_ms: at, kind: 'discarded', text: 'xyz' }), { time: '18:22:01.120', cls: 'discarded', text: '· xyz' });
  assert.equal(traceLine({ at_ms: at, kind: 'received', text: 'ACK' }).text, '← ACK');
  assert.equal(traceLine({ at_ms: at, kind: 'key', text: 'RECHTS gedrückt' }).text, '⌨ RECHTS gedrückt');
  assert.equal(traceLine({ at_ms: at, kind: 'link', text: 'verbunden: AX1S' }).text, 'verbunden: AX1S');
});

test('keysText names source and state', () => {
  assert.equal(keysText('device', true), 'aktiv (eigene Schnittstelle)');
  assert.equal(keysText('tcp', true), 'aktiv (Bridge)');
  assert.equal(keysText('off', false), 'aus');
  assert.equal(keysText('tcp', false), 'nicht verbunden (Bridge)');
});

test('countText picks singular or plural', () => {
  assert.equal(countText(1, 'Fahrt', 'Fahrten'), '1 Fahrt');
  assert.equal(countText(0, 'Fahrt', 'Fahrten'), '0 Fahrten');
  assert.equal(countText(4, 'Programm', 'Programme'), '4 Programme');
});

test('bluetoothText', () => {
  assert.equal(bluetoothText({ state: 'ready' }), 'bereit');
  assert.equal(bluetoothText({ state: 'sim' }), 'Simulation');
  assert.equal(bluetoothText({ state: 'off' }), 'aus');
  assert.equal(bluetoothText({ state: 'unavailable', reason: 'kein Adapter' }), 'nicht verfügbar: kein Adapter');
  assert.equal(bluetoothText(null), 'unbekannt');
});

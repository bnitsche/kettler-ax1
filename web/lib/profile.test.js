import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseProfile } from './profile.js';

const bob = { id: 1, name: 'Bob', weight_kg: 75, flat_watts: 70 };
const same = { name: 'Bob', weight: '75', flat: '70' };

test('parseProfile accepts a decimal comma', () => {
  const r = parseProfile({ ...same, weight: '82,5' }, bob);
  assert.deepEqual(r, { ok: true, body: { weight_kg: 82.5 } });
});

test('parseProfile accepts a decimal point', () => {
  assert.equal(parseProfile({ ...same, weight: '82.5' }, bob).body.weight_kg, 82.5);
});

test('parseProfile reports an unchanged form', () => {
  assert.deepEqual(parseProfile(same, bob), { ok: false, error: 'Keine Änderung' });
});

test('parseProfile rejects a weight out of range', () => {
  assert.deepEqual(parseProfile({ ...same, weight: '20' }, bob), { ok: false, error: 'Gewicht muss zwischen 30 und 250 kg liegen' });
  assert.equal(parseProfile({ ...same, weight: 'x' }, bob).ok, false);
});

test('parseProfile rejects a non-integer or out-of-range flat power', () => {
  const err = { ok: false, error: 'Ebene muss zwischen 0 und 300 W liegen' };
  assert.deepEqual(parseProfile({ ...same, flat: 'abc' }, bob), err);
  assert.deepEqual(parseProfile({ ...same, flat: '70.5' }, bob), err);
  assert.deepEqual(parseProfile({ ...same, flat: '301' }, bob), err);
});

test('parseProfile trims the name and rejects an empty one', () => {
  assert.equal(parseProfile({ ...same, name: '  Bobby ' }, bob).body.name, 'Bobby');
  assert.deepEqual(parseProfile({ ...same, name: '  ' }, bob), { ok: false, error: 'Name darf nicht leer sein' });
});

test('parseProfile sends only changed fields', () => {
  const r = parseProfile({ name: 'Bob', weight: '85', flat: '80' }, bob);
  assert.deepEqual(r, { ok: true, body: { weight_kg: 85, flat_watts: 80 } });
});

test('parseProfile sends a changed strap and accepts it as the only change', () => {
  const withStrap = { ...bob, strap_id: 3 };
  assert.deepEqual(parseProfile({ ...same, strap: '5' }, withStrap), { ok: true, body: { strap_id: 5 } });
  assert.deepEqual(parseProfile({ ...same, strap: '' }, withStrap), { ok: true, body: { strap_id: null } });
  assert.deepEqual(parseProfile({ ...same, strap: '3' }, withStrap), { ok: false, error: 'Keine Änderung' });
  // Without a strap field (no straps registered) nothing is sent for it.
  assert.deepEqual(parseProfile({ ...same, weight: '80' }, withStrap), { ok: true, body: { weight_kg: 80 } });
});

import test from 'node:test';
import assert from 'node:assert/strict';
import { rideDeleteText, personDeleteText } from './delete-text.js';
import { loadSelected, saveSelected, clearSelected } from './selection.js';

const at = (y, mo, d, h, mi) => new Date(y, mo - 1, d, h, mi).getTime() / 1000;

test('ride text names date, program and duration', () => {
  const s = { started_at: at(2026, 10, 25, 14, 5), program_name: 'Grundlage 30', ride_secs: 1800 };
  assert.equal(rideDeleteText(s),
    'Fahrt vom 25.10.2026, 14:05 (Grundlage 30, 30:00) wirklich löschen? Das lässt sich nicht rückgängig machen.');
});

test('free rides are called Frei', () => {
  const s = { started_at: at(2026, 1, 2, 3, 4), program_name: null, ride_secs: 65 };
  assert.match(rideDeleteText(s), /\(Frei, 1:05\)/);
});

test('person text counts the rides', () => {
  const tail = ' wirklich löschen? Das lässt sich nicht rückgängig machen.';
  assert.equal(personDeleteText('Alice', 12), `Alice und alle 12 Fahrten${tail}`);
  assert.equal(personDeleteText('Bob', 2), `Bob und beide Fahrten${tail}`);
  assert.equal(personDeleteText('Bob', 1), `Bob und die Fahrt${tail}`);
  assert.equal(personDeleteText('Cy', 0), `Cy${tail}`);
});

test('clearSelected forgets the stored person', () => {
  const store = new Map();
  globalThis.localStorage = {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, v),
    removeItem: (k) => store.delete(k),
  };
  saveSelected(7);
  assert.equal(loadSelected(), 7);
  clearSelected();
  assert.equal(loadSelected(), null);
  delete globalThis.localStorage;
  assert.doesNotThrow(() => clearSelected());
});

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { backupName, restoreText } from './backup-name.js';

test('backupName uses the local date and time', () => {
  assert.equal(backupName(new Date(2026, 9, 27, 9, 5)), 'kettler-backup-2026-10-27-0905.db');
  assert.equal(backupName(new Date(2026, 0, 3, 23, 59)), 'kettler-backup-2026-01-03-2359.db');
});

test('restoreText counts with singular and plural', () => {
  const text = restoreText({ persons: 2, programs: 7, rides: 1 });
  assert.match(text, /2 Fahrer/);
  assert.match(text, /7 Programme/);
  assert.match(text, /1 Fahrt(?!en)/);
  assert.match(restoreText({ persons: 1, programs: 1, rides: 2 }), /1 Fahrer, 1 Programm und 2 Fahrten/);
});

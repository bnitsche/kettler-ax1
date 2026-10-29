import { test } from 'node:test';
import assert from 'node:assert/strict';
import { versionText, isNewer } from './version.js';

const a = { version: '1.0.0', commit: 'abc1234' };

test('versionText shows version and commit', () => {
  assert.equal(versionText(a), 'Version 1.0.0 (abc1234)');
  assert.equal(versionText(null), '');
});

test('isNewer detects a different version or commit only', () => {
  assert.equal(isNewer(a, { ...a }), false);
  assert.equal(isNewer(a, { ...a, commit: 'def5678' }), true);
  assert.equal(isNewer(a, { ...a, version: '1.1.0' }), true);
  assert.equal(isNewer(null, a), false);
  assert.equal(isNewer(a, null), false);
});

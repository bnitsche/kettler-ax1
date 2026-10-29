import { test } from 'node:test';
import assert from 'node:assert/strict';
import { rideState, loadFinished } from './history.js';

test('rideState', () => {
  assert.equal(rideState({ id: 3, finished: true }, null, false), 'finished');
  assert.equal(rideState({ id: 3, finished: true }, 3, true), 'finished');
  assert.equal(rideState({ id: 3, finished: false }, null, false), 'unknown');
  assert.equal(rideState({ id: 3, finished: false }, 3, true), 'running');
  assert.equal(rideState({ id: 3, finished: false }, null, true), 'aborted');
  assert.equal(rideState({ id: 3, finished: false }, 4, true), 'aborted');
});

test('loadFinished retries until the summary is finished', async () => {
  const states = [false, false, true, true];
  let calls = 0;
  const sleeps = [];
  const load = async () => ({ summary: { finished: states[calls++] } });
  const detail = await loadFinished(load, { sleep: async (ms) => { sleeps.push(ms); } });
  assert.equal(detail.summary.finished, true);
  assert.equal(calls, 3);
  assert.deepEqual(sleeps, [300, 300]);
});

test('loadFinished gives up after five retries and returns the last detail', async () => {
  let calls = 0;
  const load = async () => ({ n: ++calls, summary: { finished: false } });
  const detail = await loadFinished(load, { sleep: async () => {} });
  assert.equal(calls, 6);
  assert.equal(detail.n, 6);
});

test('loadFinished passes errors on', async () => {
  const load = async () => { throw new Error('weg'); };
  await assert.rejects(loadFinished(load, { sleep: async () => {} }), /weg/);
});

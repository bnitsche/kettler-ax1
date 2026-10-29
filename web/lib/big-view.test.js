import { test } from 'node:test';
import assert from 'node:assert/strict';
import { bandValues, pausedHint } from './big-view.js';

const status = { actual_watts: 178, pulse: 152, rpm: 88 };
const segments = [{ start_secs: 0, duration_secs: 600, start_watts: 100, end_watts: 200 }];

function programView(extra = {}) {
  return {
    last: status,
    pulse: 152,
    ride: {
      segments,
      session: { phase: 'Active', ride_secs: 400, target_watts: 180, overridden: false, segment_label: 'Intervall 3', segment_remaining_secs: 72, last: status, ...extra },
    },
  };
}

test('bandValues shows an active program ride', () => {
  assert.deepEqual(bandValues(programView()), {
    watts: '178', target: '180', overridden: false, label: 'Intervall 3', remaining: '1:12', pulse: '152', rpm: '88',
  });
});

test('bandValues passes the overridden target on', () => {
  assert.equal(bandValues(programView({ overridden: true })).overridden, true);
});

test('bandValues without a status gives dashes everywhere', () => {
  assert.deepEqual(bandValues({ last: null, pulse: null, ride: null }), {
    watts: '–', target: '–', overridden: false, label: '–', remaining: '–', pulse: '–', rpm: '–',
  });
  const v = bandValues({ last: null, pulse: null, ride: { segments, session: { ...programView().ride.session, last: null } } });
  assert.equal(v.watts, '–');
  assert.equal(v.pulse, '–');
  assert.equal(v.rpm, '–');
});

test('bandValues of a free ride shows the ride time', () => {
  const v = bandValues({ last: status, ride: { segments: null, session: { phase: 'Active', ride_secs: 3725, target_watts: 120, overridden: false, segment_label: null, segment_remaining_secs: null, last: status } } });
  assert.equal(v.label, 'Fahrzeit');
  assert.equal(v.remaining, '1:02:05');
  assert.equal(v.target, '120');
});

test('bandValues after the program end', () => {
  const v = bandValues(programView({ segment_label: null, segment_remaining_secs: null }));
  assert.equal(v.label, 'Programm zu Ende');
  assert.equal(v.remaining, '–');
});

test('bandValues shows the pulse the server chose (strap or cockpit)', () => {
  assert.equal(bandValues({ last: status, pulse: 95, ride: null }).pulse, '95');
  assert.equal(bandValues({ last: status, pulse: null, ride: null }).pulse, '–');
});

test('pausedHint names the way to resume that the view offers', () => {
  assert.equal(pausedHint(false, false), 'Pausiert – „Weiter“ zum Fortsetzen');
  assert.equal(pausedHint(false, true), 'Pausiert – „Weiter“ zum Fortsetzen');
  assert.equal(pausedHint(true, true), 'Pausiert – Taste RECHTS zum Fortsetzen');
  assert.equal(pausedHint(true, false), 'Pausiert – auf den Verlauf tippen, dann „Weiter“');
});

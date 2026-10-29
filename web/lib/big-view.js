// Values of the band under the large chart view (pure, no DOM).

import { duration, whole } from './format.js';

/**
 * Display strings for the band of the large view from a LiveView: watts, target, overridden (bool), the
 * segment label with its remaining time (free ride: "Fahrzeit" and the ride time), pulse (`view.pulse`) and rpm.
 * Unknown values are "–".
 */
export function bandValues(view) {
  const ride = (view && view.ride) || null;
  const s = ride ? ride.session : null;
  const last = (s && s.last) || (view && view.last) || null;
  let label = '–';
  let remaining = '–';
  if (s && !ride.segments) {
    label = 'Fahrzeit';
    remaining = duration(s.ride_secs);
  } else if (s && s.segment_label != null) {
    label = s.segment_label;
    remaining = duration(s.segment_remaining_secs || 0);
  } else if (s) {
    label = 'Programm zu Ende';
  }
  return {
    watts: last ? whole(last.actual_watts) : '–',
    target: s ? whole(s.target_watts) : '–',
    overridden: Boolean(s && s.overridden),
    label,
    remaining,
    // The server's choice: the strap's pulse, else the cockpit's (never its recovery drop).
    pulse: view && view.pulse ? whole(view.pulse) : '–',
    rpm: last ? whole(last.rpm) : '–',
  };
}

/**
 * The hint while the rider paused. The large view has no "Weiter" button: it names the RIGHT key when a keypad
 * is connected, otherwise the tap back to the normal view.
 */
export function pausedHint(big, keypad) {
  if (!big) return 'Pausiert – „Weiter“ zum Fortsetzen';
  return keypad ? 'Pausiert – Taste RECHTS zum Fortsetzen' : 'Pausiert – auf den Verlauf tippen, dann „Weiter“';
}

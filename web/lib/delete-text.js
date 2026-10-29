// Confirmation texts for deleting a ride or a person.

import { dateTime, duration } from './format.js';

const TAIL = ' wirklich löschen? Das lässt sich nicht rückgängig machen.';

/** `summary` is a ride summary from the API. */
export function rideDeleteText(summary) {
  const program = summary.program_name || 'Frei';
  return `Fahrt vom ${dateTime(summary.started_at)} (${program}, ${duration(summary.ride_secs)})${TAIL}`;
}

export function personDeleteText(name, rides) {
  let what = name;
  if (rides === 1) what += ' und die Fahrt';
  else if (rides === 2) what += ' und beide Fahrten';
  else if (rides > 2) what += ` und alle ${rides} Fahrten`;
  return what + TAIL;
}

// Pure helpers for the ride history screens.

/**
 * State of a ride for the badge. `liveRideId` is the id of the ride the live view reports (or null),
 * `liveKnown` tells whether any live frame was received yet.
 * 'aborted' only when a live frame was seen and does not show this ride.
 */
export function rideState(summary, liveRideId, liveKnown) {
  if (summary.finished) return 'finished';
  if (!liveKnown) return 'unknown';
  return liveRideId === summary.id ? 'running' : 'aborted';
}

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Loads a ride detail with `load()` and, while its summary is not finished yet (the server
 * writes it right after the ride ends), retries up to `retries` times every `delayMs`.
 * Returns the last detail; errors are passed on.
 */
export async function loadFinished(load, { retries = 5, delayMs = 300, sleep = wait } = {}) {
  let detail = await load();
  for (let i = 0; i < retries && !detail.summary.finished; i++) {
    await sleep(delayMs);
    detail = await load();
  }
  return detail;
}

// Pulse from the cockpit (ear clip) and the chest strap.

const valid = (p) => (p > 0 ? p : null);

/** The cockpit's pulse of a status; at the end of its recovery measurement that field holds the drop. */
function cockpitPulse(last) {
  if (!last || (last.recovery && last.elapsed_secs === 0)) return null;
  return valid(last.pulse);
}

/** Effective pulse of a stored sample: strap if > 0, else cockpit if > 0, else null. */
export function samplePulse(s) {
  return valid(s.strap_pulse) ?? valid(s.status.pulse);
}

/** Small text under the live pulse: "Gurt · Ohr 70", "Gurt", "Ohr" or "" without a pulse. */
export function pulseSourceText(view) {
  if (!view) return '';
  const last = (view.ride && view.ride.session && view.ride.session.last) || view.last;
  const cockpit = cockpitPulse(last);
  if (view.strap && valid(view.strap.pulse)) return cockpit ? `Gurt · Ohr ${cockpit}` : 'Gurt';
  return view.pulse ? 'Ohr' : '';
}

function avgMax(values) {
  if (!values.length) return null;
  const n = values.length;
  const sum = values.reduce((a, b) => a + b, 0);
  return { avg: Math.floor((sum + Math.floor(n / 2)) / n), max: Math.max(...values) };
}

/** Ø/max per source, rounded like the server: { strap: {avg, max} | null, cockpit: {avg, max} | null }. */
export function pulseBySource(samples) {
  const list = samples || [];
  return {
    strap: avgMax(list.map((s) => valid(s.strap_pulse)).filter((p) => p != null)),
    cockpit: avgMax(list.map((s) => valid(s.status.pulse)).filter((p) => p != null)),
  };
}

/** "verbunden · Puls 72 · Akku 80 %", "sucht …" or "nicht in Reichweite". */
export function strapStateText(state) {
  if (state.link === 'connected') {
    const parts = ['verbunden', state.pulse ? `Puls ${state.pulse}` : 'kein Puls'];
    if (state.battery != null) parts.push(`Akku ${state.battery} %`);
    return parts.join(' · ');
  }
  const text = state.link === 'searching' ? 'sucht …' : 'nicht in Reichweite';
  return state.problem ? `${text} · zuletzt: ${state.problem}` : text;
}

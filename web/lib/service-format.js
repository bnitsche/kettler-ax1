// Texts for the Zustand and Live tabs of the service page.

const pad = (n, width = 2) => String(n).padStart(width, '0');

/** Rough duration: "unter 1 min", "14 min", "2 h 5 min", "3 Tage". */
export function durationText(ms) {
  const minutes = Math.floor(ms / 60_000);
  if (minutes < 1) return 'unter 1 min';
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} h ${minutes % 60} min`;
  const days = Math.floor(hours / 24);
  return days === 1 ? '1 Tag' : `${days} Tage`;
}

/** File size with German decimals: "0 B", "812 kB", "2,4 MB". */
export function bytesText(n) {
  if (n < 1000) return `${n} B`;
  if (n < 1_000_000) return `${Math.round(n / 1000)} kB`;
  return `${(n / 1_000_000).toFixed(1).replace('.', ',')} MB`;
}

const PREFIX = { sent: '→ ', received: '← ', discarded: '· ', key: '⌨ ' };

/** One raw traffic entry as shown: local time with milliseconds, CSS class and text. */
export function traceLine(entry) {
  const d = new Date(entry.at_ms);
  const time = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
  return { time, cls: entry.kind, text: `${PREFIX[entry.kind] || ''}${entry.text}` };
}

const SOURCE = { device: 'eigene Schnittstelle', tcp: 'Bridge' };

/** Key channel state, e.g. "aktiv (Bridge)". */
export function keysText(keys, connected) {
  if (keys === 'off' || !SOURCE[keys]) return 'aus';
  return `${connected ? 'aktiv' : 'nicht verbunden'} (${SOURCE[keys]})`;
}

/** "1 Fahrt", "4 Fahrten". */
export function countText(n, one, many) {
  return `${n} ${n === 1 ? one : many}`;
}

/** The strap backend's availability (`GET /api/straps`, `/api/service/status`). */
export function bluetoothText(a) {
  if (!a) return 'unbekannt';
  if (a.state === 'ready') return 'bereit';
  if (a.state === 'sim') return 'Simulation';
  if (a.state === 'off') return 'aus';
  return `nicht verfügbar: ${a.reason}`;
}

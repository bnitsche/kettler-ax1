// Pure helpers for the elevation strip: axis range and texts. The heights themselves come from the server.

const MIN_SPAN_M = 10;
const MARGIN = 0.05;

/**
 * Axis range of the strip for the program preview (`[secs, height]` pairs) and the ridden points (`{t, h}`).
 * `min` is 0 or the lowest height below it; the span is at least 10 m, plus a 5 % margin at the top (and at the
 * bottom when the ride went below its start).
 */
export function elevationRange(previewPoints, points) {
  let lo = 0;
  let hi = 0;
  const take = (h) => {
    if (typeof h !== 'number' || !Number.isFinite(h)) return;
    lo = Math.min(lo, h);
    hi = Math.max(hi, h);
  };
  for (const p of previewPoints || []) take(p[1]);
  for (const p of points || []) take(p.h);
  const span = Math.max(MIN_SPAN_M, hi - lo);
  const margin = span * MARGIN;
  return { min: lo < 0 ? lo - margin : 0, max: lo + span + margin };
}

const rounded = (m) => String(Math.round(m) || 0);

/** A height in whole metres ("62 m"), or "–" when there is none. */
export function heightText(m) {
  return m == null ? '–' : `${rounded(m)} m`;
}

/** Metres climbed ("121 Hm"), or "–" when there is none. */
export function gainText(m) {
  return m == null ? '–' : `${rounded(m)} Hm`;
}

/** Round heights for the strip's axis lines: multiples of a 1-2-5 step of about half the span, above `min` (one or two). */
export function elevationSteps({ min, max }) {
  const rough = (max - min) / 2;
  const power = 10 ** Math.floor(Math.log10(rough));
  const step = [1, 2, 5, 10].map((f) => f * power).find((s) => s >= rough);
  const out = [];
  for (let v = Math.floor(min / step + 1) * step; v <= max; v += step) out.push(v);
  return out;
}

const num = (n) => String(Math.round(n * 10) / 10);

function scaled(points, totalSecs, width, height, { min, max }) {
  const out = [];
  for (const p of points || []) {
    if (typeof p.h !== 'number') continue;
    const x = totalSecs > 0 ? (Math.min(p.t, totalSecs) / totalSecs) * width : 0;
    const y = height - ((p.h - min) / (max - min)) * height;
    out.push(`${num(x)},${num(Math.min(height, Math.max(0, y)))}`);
  }
  return out;
}

/** Polyline points of `{t, h}` in a `width` x `height` strip spanning `totalSecs` and the height `range`. */
export function elevationPath(points, totalSecs, width, height, range) {
  return scaled(points, totalSecs, width, height, range).join(' ');
}

/** Closed SVG path under the same line, down to the bottom of the strip (the light fill). */
export function elevationArea(points, totalSecs, width, height, range) {
  const pts = scaled(points, totalSecs, width, height, range);
  if (pts.length === 0) return '';
  const x = (p) => p.split(',')[0];
  return `M${x(pts[0])},${num(height)} L${pts.join(' ')} ${x(pts[pts.length - 1])},${num(height)} Z`;
}

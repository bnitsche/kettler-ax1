// Pure helpers for the training chart (SVG path data).

const num = (n) => String(Math.round(n * 10) / 10);

function point(t, value, totalSecs, width, height, maxValue) {
  const x = totalSecs > 0 ? (t / totalSecs) * width : 0;
  const y = height - (Math.min(value, maxValue) / maxValue) * height;
  return `${num(x)},${num(Math.max(0, y))}`;
}

/** Closed SVG path of the program's target area, from baseline to baseline. */
export function programPath(segments, totalSecs, width, height, maxWatts) {
  if (!segments || segments.length === 0) return '';
  const pts = [];
  for (const s of segments) {
    pts.push(point(s.start_secs, s.start_watts, totalSecs, width, height, maxWatts));
    pts.push(point(s.start_secs + s.duration_secs, s.end_watts, totalSecs, width, height, maxWatts));
  }
  const last = segments[segments.length - 1];
  const endX = num(((last.start_secs + last.duration_secs) / totalSecs) * width);
  const startX = num((segments[0].start_secs / totalSecs) * width);
  return `M${startX},${num(height)} L${pts.join(' ')} ${endX},${num(height)} Z`;
}

/**
 * Polyline points for `key` of each point ({t, watts, pulse, target}).
 * Missing values are skipped, as are zero pulses (no heart rate signal).
 */
export function tracePath(points, key, totalSecs, width, height, maxValue) {
  const out = [];
  for (const p of points) {
    const v = p[key];
    if (v == null || (key === 'pulse' && v === 0)) continue;
    out.push(point(p.t, v, totalSecs, width, height, maxValue));
  }
  return out.join(' ');
}

/** Y-axis maximum: largest program, watt, target or pulse value, rounded up to 50, at least 200. */
export function scaleMax(segments, points) {
  let max = 0;
  for (const s of segments || []) max = Math.max(max, s.start_watts, s.end_watts);
  for (const p of points || []) max = Math.max(max, p.watts || 0, p.target || 0, p.pulse || 0);
  return Math.max(200, Math.ceil(max / 50) * 50);
}

/** Right-hand cadence axis maximum: 120 U/min, or the highest rpm rounded up to 10 when above. */
export function cadenceMax(points) {
  let max = 0;
  for (const p of points || []) max = Math.max(max, p.rpm || 0);
  return Math.max(120, Math.ceil(max / 10) * 10);
}

/** Two or three labelled values of the cadence axis: 60, 90, 120 up to 120, then thirds of the maximum. */
export function cadenceSteps(max) {
  if (max <= 120) return [60, 90, 120];
  const step = Math.ceil(max / 30) * 10;
  const out = [];
  for (let v = step; v <= max && out.length < 3; v += step) out.push(v);
  return out;
}

/**
 * Appends `p` to `points` if its `t` is new. Returns "added", "duplicate"
 * (t not after the last point) or "gap" (more than 2 s missing; not appended,
 * the caller should reload the trace).
 */
export function appendPoint(points, p) {
  const last = points[points.length - 1];
  if (last && p.t <= last.t) return 'duplicate';
  if (p.t - (last ? last.t : 0) > 2) return 'gap';
  points.push(p);
  return 'added';
}

/**
 * Combines the stored history with live points that arrived around a reload
 * (including the point whose gap triggered it). Sorted by t, one point per t,
 * history wins on equal t. Gaps are accepted, so a point missing from storage
 * cannot cause a reload loop.
 */
export function mergeTrace(history, pending) {
  const byT = new Map();
  for (const p of pending || []) byT.set(p.t, p);
  for (const p of history || []) byT.set(p.t, p);
  return [...byT.values()].sort((a, b) => a.t - b.t);
}

/** Shared x and y scale for two traces: the longer ride's length (at least 60 s) and the common maximum. */
export function compareScale(pointsA, pointsB) {
  let total = 60;
  for (const p of [...(pointsA || []), ...(pointsB || [])]) total = Math.max(total, p.t);
  return { total, max: scaleMax(null, [...(pointsA || []), ...(pointsB || [])]) };
}

/**
 * Reduces `points` to about `n` points for drawing `key`: the first and last point plus the minimum and
 * maximum of each bucket, so peaks survive. Points without a value for `key` are dropped.
 */
export function downsample(points, n, key) {
  if (points.length <= n) return points;
  const valid = points.filter((p) => p[key] != null);
  if (valid.length <= n) return valid;
  const buckets = Math.max(1, Math.floor((n - 2) / 2));
  const size = valid.length / buckets;
  const keep = new Set([valid[0], valid[valid.length - 1]]);
  for (let b = 0; b < buckets; b++) {
    const from = Math.floor(b * size);
    const to = Math.min(valid.length, Math.floor((b + 1) * size));
    let lo = valid[from];
    let hi = valid[from];
    for (let i = from; i < to; i++) {
      if (valid[i][key] < lo[key]) lo = valid[i];
      if (valid[i][key] > hi[key]) hi = valid[i];
    }
    keep.add(lo);
    keep.add(hi);
  }
  return valid.filter((p) => keep.has(p));
}

/** Horizontal grid lines: every 50 W up to a max of 300, every 100 W above, from the step up to `maxWatts`. */
export function gridSteps(maxWatts) {
  const step = maxWatts <= 300 ? 50 : 100;
  const out = [];
  for (let v = step; v <= maxWatts; v += step) out.push(v);
  return out;
}

/** The point of `points` (sorted by t) closest to `t`; the earlier one on a tie. Null when empty. */
export function nearestPoint(points, t) {
  if (!points || points.length === 0) return null;
  let lo = 0;
  let hi = points.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (points[mid].t < t) lo = mid + 1;
    else hi = mid;
  }
  // `lo` is the first point at or after t; its predecessor may be closer.
  if (lo > 0 && t - points[lo - 1].t <= points[lo].t - t) return points[lo - 1];
  return points[lo];
}

/** Time at pixel `x` of a chart `width` pixels wide that spans `totalSecs`, clamped to the chart. */
export function scrubTime(x, width, totalSecs) {
  if (!(width > 0)) return 0;
  return Math.min(1, Math.max(0, x / width)) * totalSecs;
}

/** Left edge of a tooltip `tipWidth` wide next to the cursor at `x`: right of it, left of it near the edge, never outside. */
export function tooltipLeft(x, width, tipWidth, gap = 12) {
  let left = x + gap;
  if (left + tipWidth > width) left = x - gap - tipWidth;
  return Math.max(0, Math.min(left, width - tipWidth));
}

/** A touch movement counts as a scrub (not a page scroll) when it is mostly horizontal and over `min` pixels. */
export function isHorizontalDrag(dx, dy, min = 6) {
  return Math.abs(dx) > Math.abs(dy) && Math.abs(dx) > min;
}

/** Median time between neighbouring points (0 for fewer than two points). */
export function medianSpacing(points) {
  if (!points || points.length < 2) return 0;
  const gaps = [];
  for (let i = 1; i < points.length; i++) gaps.push(points[i].t - points[i - 1].t);
  gaps.sort((a, b) => a - b);
  const mid = gaps.length >> 1;
  return gaps.length % 2 ? gaps[mid] : (gaps[mid - 1] + gaps[mid]) / 2;
}

/** A pointer press counts as a tap when it moved at most 10 pixels and lasted at most 500 ms. */
export function isTap(dx, dy, ms) {
  return Math.hypot(dx, dy) <= 10 && ms <= 500;
}

// Which optional chart lines (cadence, pulse) and the elevation strip are shown, remembered in localStorage.

const STORAGE_KEY = 'kettler.chart_lines';

export const DEFAULT_LINES = { rpm: true, pulse: true, elevation: true };

/** Parses the stored JSON; every missing or malformed value falls back to "shown". */
export function parseLines(text) {
  let data = null;
  try {
    data = JSON.parse(text);
  } catch {
    // Not JSON: use the defaults.
  }
  const pick = (key) => (data && typeof data[key] === 'boolean' ? data[key] : DEFAULT_LINES[key]);
  return { rpm: pick('rpm'), pulse: pick('pulse'), elevation: pick('elevation') };
}

const defaultStorage = () => {
  try {
    return localStorage;
  } catch {
    return undefined;
  }
};

export function loadLines(storage = defaultStorage()) {
  try {
    return parseLines(storage.getItem(STORAGE_KEY));
  } catch {
    return { ...DEFAULT_LINES };
  }
}

export function saveLines(lines, storage = defaultStorage()) {
  try {
    storage.setItem(STORAGE_KEY, JSON.stringify({ rpm: lines.rpm, pulse: lines.pulse, elevation: lines.elevation }));
  } catch {
    // Private mode or storage disabled: the choice is only kept for this page.
  }
}

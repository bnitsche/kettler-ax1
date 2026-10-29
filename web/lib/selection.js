// The selected person, remembered in localStorage.

const STORAGE_KEY = 'kettler.person_id';

export function loadSelected() {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v == null) return null;
    const n = Number(v);
    return Number.isInteger(n) ? n : null;
  } catch {
    return null;
  }
}

export function saveSelected(id) {
  try {
    localStorage.setItem(STORAGE_KEY, String(id));
  } catch {
    // Private mode or storage disabled: the choice is only kept for this page.
  }
}

export function clearSelected() {
  try {
    localStorage.removeItem(STORAGE_KEY);
  } catch {
    // Storage unavailable: nothing was remembered.
  }
}

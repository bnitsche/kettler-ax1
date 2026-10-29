// Navigation guard: a screen may register a function that returns false
// to keep the user on the page (e.g. unsaved changes).

let guard = null;

export function setBeforeLeave(fn) {
  guard = fn;
}

export function canLeave() {
  return !guard || guard() !== false;
}

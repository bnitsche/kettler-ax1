// Beeps and speech (speechSynthesis) for the ride cues. iOS only plays sound after a tap:
// `unlock()` must run inside a click handler (start button, speaker button).
//
// Beeps play from <audio> elements: on iOS, Web Audio goes silent after the second announcement while
// still reporting "running". The Web Audio context remains for the lock state of the speaker button and
// as the beep source until the elements were unlocked by a tap.

const RATE = 22050;
const TONES = { normal: { freq: 880, secs: 0.18 }, last: { freq: 1320, secs: 0.45 } };

let ctx = null;
let elements = null;
let elementsUnlocked = false;
const listeners = new Set();

function notify() {
  for (const cb of listeners) cb();
}

function context() {
  if (ctx) return ctx;
  const Ctx = globalThis.AudioContext || globalThis.webkitAudioContext;
  if (!Ctx) return null;
  try {
    ctx = new Ctx();
  } catch {
    return null;
  }
  ctx.addEventListener('statechange', notify);
  return ctx;
}

/** A sine tone with short fades as a 16-bit mono WAV blob URL. */
function wavUrl({ freq, secs }) {
  const n = Math.round(RATE * secs);
  const view = new DataView(new ArrayBuffer(44 + n * 2));
  const text = (at, s) => { for (let i = 0; i < s.length; i++) view.setUint8(at + i, s.charCodeAt(i)); };
  text(0, 'RIFF');
  view.setUint32(4, 36 + n * 2, true);
  text(8, 'WAVEfmt ');
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, RATE, true);
  view.setUint32(28, RATE * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  text(36, 'data');
  view.setUint32(40, n * 2, true);
  const fade = Math.round(RATE * 0.01);
  for (let i = 0; i < n; i++) {
    const env = Math.min(1, i / fade, (n - i) / fade);
    view.setInt16(44 + i * 2, Math.round(Math.sin((2 * Math.PI * freq * i) / RATE) * env * 0.6 * 32767), true);
  }
  return URL.createObjectURL(new Blob([view], { type: 'audio/wav' }));
}

function audioElements() {
  if (!elements && typeof Audio !== 'undefined') {
    elements = { normal: new Audio(wavUrl(TONES.normal)), last: new Audio(wavUrl(TONES.last)) };
  }
  return elements;
}

/**
 * Whether beeps can play: after a tap unlocked the elements, or while Web Audio runs without one
 * (desktop browsers). iOS suspends the unused context after an element played, so its state alone
 * does not tell.
 */
export function isUnlocked() {
  if (elementsUnlocked) return true;
  const c = context();
  return Boolean(c) && c.state === 'running';
}

/** Called on every change of `isUnlocked()`; returns the unsubscribe function. */
export function onUnlockChange(cb) {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

/** Unlocks beeps and speech; call it from a tap. */
export function unlock() {
  const c = context();
  if (c) {
    c.resume().then(notify, () => {});
    // A silent buffer counts as the sound "started by the user".
    const source = c.createBufferSource();
    source.buffer = c.createBuffer(1, 1, RATE);
    source.connect(c.destination);
    source.start(0);
  }
  // Safari lifts the restriction on an element with play() inside the tap, even when the silent speech
  // below aborts that play; the muted tone otherwise plays to its end.
  const els = audioElements();
  if (els) {
    for (const a of Object.values(els)) {
      a.muted = true;
      a.play().catch(() => {});
    }
    elementsUnlocked = true;
  }
  if ('speechSynthesis' in globalThis) {
    const silent = new SpeechSynthesisUtterance(' ');
    silent.volume = 0;
    speechSynthesis.speak(silent);
  }
  notify();
}

/** A short beep; the `last` one before the change is higher and longer. */
export function beep(last = false) {
  const tone = last ? TONES.last : TONES.normal;
  if (elementsUnlocked) {
    const a = elements[last ? 'last' : 'normal'];
    a.muted = false;
    a.currentTime = 0;
    a.play().catch(() => {});
    return;
  }
  const c = context();
  if (c && c.state === 'running') webAudioTone(c, tone);
}

function webAudioTone(c, { freq, secs }) {
  const t = c.currentTime;
  const osc = c.createOscillator();
  const gain = c.createGain();
  osc.type = 'sine';
  osc.frequency.value = freq;
  gain.gain.setValueAtTime(0.0001, t);
  gain.gain.exponentialRampToValueAtTime(0.6, t + 0.01);
  gain.gain.exponentialRampToValueAtTime(0.0001, t + secs);
  osc.connect(gain).connect(c.destination);
  osc.start(t);
  osc.stop(t + secs + 0.02);
}

/** Speaks German text, replacing whatever is still being said. */
export function speak(text) {
  if (!('speechSynthesis' in globalThis)) return;
  speechSynthesis.cancel();
  const u = new SpeechSynthesisUtterance(text);
  u.lang = 'de-DE';
  const voice = speechSynthesis.getVoices().find((v) => v.lang && v.lang.toLowerCase().startsWith('de'));
  if (voice) u.voice = voice;
  speechSynthesis.speak(u);
}

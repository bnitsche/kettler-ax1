// App shell: hash router, menu and screen lifecycle.

import * as live from './lib/live.js';
import * as riders from './screens/riders.js';
import * as ride from './screens/ride.js';
import * as programs from './screens/programs.js';
import * as history from './screens/history.js';
import * as service from './screens/service.js';
import { canLeave, setBeforeLeave } from './lib/nav.js';
import { api } from './lib/api.js';
import { el } from './lib/dom.js';
import { versionText, isNewer } from './lib/version.js';
import { bannerText } from './lib/recovery.js';

const root = document.getElementById('app');
const menu = document.getElementById('menu');
let cleanup = null;
let firstView = true;
let shownHash = null;
let restoring = false;

const routes = [
  [/^#\/riders$/, () => riders],
  [/^#\/ride$/, () => ride],
  [/^#\/programs$/, () => programs.list],
  [/^#\/programs\/new$/, () => programs.edit],
  [/^#\/programs\/(\d+)$/, () => programs.edit],
  [/^#\/history$/, () => history.list],
  [/^#\/history\/compare\/(\d+)\/(\d+)$/, () => history.compare],
  [/^#\/history\/(\d+)$/, () => history.detail],
  [/^#\/service$/, () => service],
];

// ---- Version: fetched once; compared again after every websocket (re)connect -------------
const menuVersion = document.getElementById('menu-version');
let loadedVersion = null;
let newVersion = false;
const footVersion = el('p', { class: 'version-foot' });
// Phones: the hint sits above the tab bar on every page (the menu's version text is hidden there).
const banner = el('div', { class: 'version-banner', role: 'status', hidden: true });
document.body.append(banner);

function renderVersion() {
  const text = versionText(loadedVersion);
  const hint = () => el('button', { type: 'button', class: 'version-hint', onclick: () => location.reload() },
    'Neue Version verfügbar – Seite neu laden');
  menuVersion.replaceChildren(text, ...(newVersion ? [' ', hint()] : []));
  footVersion.replaceChildren(text);
  banner.replaceChildren(...(newVersion ? [hint()] : []));
  banner.hidden = !newVersion;
  menuVersion.hidden = !loadedVersion;
}

async function fetchVersion() {
  try {
    return await api('/api/version');
  } catch {
    return null;
  }
}

async function initVersion() {
  loadedVersion = await fetchVersion();
  if (!loadedVersion) return;
  console.info(versionText(loadedVersion));
  renderVersion();
  live.onOpen(async () => {
    if (newVersion) return;
    newVersion = isNewer(loadedVersion, await fetchVersion());
    if (newVersion) renderVersion();
  });
}

function highlightMenu(hash) {
  for (const a of menu.querySelectorAll('a[data-section]')) {
    const section = a.dataset.section;
    const active = section === 'riders' ? hash === '#/riders' : hash.startsWith(`#/${section}`);
    a.classList.toggle('active', active);
    if (active) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }
}

function render() {
  const hash = location.hash;
  if (restoring) { restoring = false; return; }
  // The automatic jump to a running ride never asks; a draft stays in its sessionStorage backup.
  const toRunningRide = hash === '#/ride' && Boolean(live.current()?.ride);
  if (shownHash != null && hash !== shownHash && !toRunningRide && !canLeave()) {
    // Keep the screen and put the address back (Back button, edge swipe, links).
    restoring = true;
    location.replace(shownHash);
    return;
  }
  setBeforeLeave(null);
  shownHash = hash;
  const match = routes.find(([re]) => re.test(hash));
  if (!match) {
    const view = live.current();
    location.replace(view && view.ride ? '#/ride' : '#/riders');
    return;
  }
  if (cleanup) cleanup();
  cleanup = null;
  const params = hash.match(match[0]).slice(1);
  const onRide = hash === '#/ride';
  menu.hidden = onRide;
  document.body.classList.toggle('riding', onRide);
  highlightMenu(hash);
  root.replaceChildren();
  cleanup = match[1]().mount(root, params) || null;
  // On phones the tab bar is at the bottom, so the version sits at the end of the training selection.
  if (hash === '#/riders') root.append(footVersion);
}

// The recovery measurement (cockpit or chest strap) is shown on every screen, also after a ride.
const recoveryText = el('span');
const recoveryCancel = el('button', { type: 'button', class: 'btn small', hidden: true, onclick: () => live.send({ type: 'recovery_cancel' }) }, 'Abbrechen');
const recoveryBanner = el('div', { class: 'recovery-banner', role: 'status', hidden: true }, recoveryText, recoveryCancel);
document.body.append(recoveryBanner);
live.onLive((view) => {
  const text = bannerText(view);
  recoveryText.textContent = text;
  recoveryBanner.hidden = !text;
  // Only the server's own measurement with the strap can be cancelled.
  recoveryCancel.hidden = !(view.strap_recovery && view.strap_recovery.state === 'measuring');
});
live.onConnection((online) => { if (!online) recoveryBanner.hidden = true; });

// A ride already running when the app opens is shown directly.
live.onLive((view) => {
  if (firstView) {
    firstView = false;
    if (view.ride && location.hash !== '#/ride') location.hash = '#/ride';
  }
});

// After a restore all ids belong to other data: every open client reloads.
let dataEpoch = null;
live.onLive((view) => {
  if (dataEpoch == null) dataEpoch = view.data_epoch;
  else if (view.data_epoch !== dataEpoch) location.reload();
});

window.addEventListener('hashchange', render);
live.start();
render();
initVersion();

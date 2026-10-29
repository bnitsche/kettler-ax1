// Websocket to the server with automatic reconnect every 2 s.

const RETRY_MS = 2000;
// While the ergometer is connected the server sends a view about every second.
const STALE_MS = 5000;
const liveListeners = new Set();
const openListeners = new Set();
const stateListeners = new Set();
let socket = null;
let latest = null;
let online = false;
let lastMessageAt = 0;
let retryTimer = null;
let started = false;

function url() {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  return `${scheme}://${location.host}/ws`;
}

function setOnline(value) {
  if (online === value) return;
  online = value;
  for (const cb of stateListeners) cb(online);
}

function scheduleConnect(delay) {
  if (retryTimer != null) return;
  retryTimer = setTimeout(() => {
    retryTimer = null;
    connect();
  }, delay);
}

function connect() {
  if (socket) return;
  let ws;
  try {
    ws = new WebSocket(url());
  } catch {
    scheduleConnect(RETRY_MS);
    return;
  }
  socket = ws;
  lastMessageAt = Date.now();
  ws.addEventListener('open', () => {
    if (socket !== ws) return;
    lastMessageAt = Date.now();
    setOnline(true);
    for (const cb of openListeners) cb();
  });
  ws.addEventListener('message', (ev) => {
    if (socket !== ws) return;
    lastMessageAt = Date.now();
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch {
      return;
    }
    if (msg.type !== 'live') return;
    latest = msg;
    for (const cb of liveListeners) cb(msg);
  });
  ws.addEventListener('close', () => {
    if (socket !== ws) return;
    socket = null;
    setOnline(false);
    scheduleConnect(RETRY_MS);
  });
  ws.addEventListener('error', () => ws.close());
}

/** Drops the current socket (it may be half dead) and connects again right away. */
function reconnectNow() {
  const old = socket;
  socket = null;
  if (old) {
    try {
      old.close();
    } catch {
      // already closed
    }
  }
  setOnline(false);
  if (retryTimer != null) {
    clearTimeout(retryTimer);
    retryTimer = null;
  }
  connect();
}

function watchdog() {
  // Only while the ergometer is connected: then silence means the socket is stale.
  // Without an ergometer the server may legitimately stay quiet.
  const deviceUp = latest && latest.connected;
  if (socket && socket.readyState === WebSocket.OPEN && deviceUp && Date.now() - lastMessageAt > STALE_MS) {
    reconnectNow();
  }
}

/** Calls `cb(view)` for every live view. Returns an unsubscribe function. */
export function onLive(cb) {
  liveListeners.add(cb);
  return () => liveListeners.delete(cb);
}

/** Calls `cb()` after every (re)connect. Returns an unsubscribe function. */
export function onOpen(cb) {
  openListeners.add(cb);
  return () => openListeners.delete(cb);
}

/** Calls `cb(online)` when the websocket connection to the server opens or closes. */
export function onConnection(cb) {
  stateListeners.add(cb);
  return () => stateListeners.delete(cb);
}

/** Last live view received, or null. */
export function current() {
  return latest;
}

export function isOnline() {
  return online;
}

/** Sends a command object; returns false when not connected. */
export function send(cmd) {
  if (!socket || socket.readyState !== WebSocket.OPEN) return false;
  try {
    socket.send(JSON.stringify(cmd));
  } catch {
    return false;
  }
  return true;
}

export function start() {
  if (started) return;
  started = true;
  connect();
  setInterval(watchdog, 1000);
  // After the iPad wakes up the old socket is often dead without a close event.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') reconnectNow();
  });
}

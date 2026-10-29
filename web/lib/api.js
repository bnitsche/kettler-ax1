// JSON requests against the server API. Errors carry the server's German message.

export async function api(path, options = {}) {
  const init = { method: options.method || 'GET', headers: {} };
  if (options.body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(options.body);
  }
  let resp;
  try {
    resp = await fetch(path, init);
  } catch {
    throw new Error('Server nicht erreichbar');
  }
  if (resp.status === 204) return null;
  let data = null;
  try {
    data = await resp.json();
  } catch {
    data = null;
  }
  if (!resp.ok) {
    const err = new Error((data && data.error) || `Fehler ${resp.status}`);
    err.status = resp.status;
    throw err;
  }
  return data;
}

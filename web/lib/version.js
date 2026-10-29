// Version of the server this page was loaded from, and detection of a newer one after a redeploy.

/** "Version 1.0.0 (abc1234)". */
export function versionText(info) {
  return info ? `Version ${info.version} (${info.commit})` : '';
}

/** True when the server now reports a different build than the page was loaded with. */
export function isNewer(loaded, current) {
  return Boolean(loaded && current && (loaded.version !== current.version || loaded.commit !== current.commit));
}

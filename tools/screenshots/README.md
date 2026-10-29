# Screenshots

Takes the README screenshots in `docs/images/` from a demo database (Bob and Alice) and the simulated ergometer and chest strap.

Requirements: Rust (cargo) and Node 24.

```sh
cd tools/screenshots
npm install
npx playwright install webkit
npm run shots
```

The script builds the server (`cargo build --release`), writes a demo database to `demo/` (`kettler demo-data`), starts the server with `KETTLER_DEVICE=sim://` and `KETTLER_BLUETOOTH=sim` on a free local port, and drives it with WebKit at iPad (1180 × 820) and iPhone (390 × 844) size. The title image waits until the program „Pyramide“ is halfway through, so a run takes about 15 minutes. Afterwards the server is stopped and `demo/` is removed, also when the run fails or is interrupted.

The device picture in the README, `docs/images/system.svg`, is drawn by hand; edit it directly.

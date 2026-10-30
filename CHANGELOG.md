# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.5.10] - 2026-10-30

### Added
- Training screen: cues before segment changes in program rides (#9). At 10 s a German announcement via `speechSynthesis` („Noch 10 Sekunden, dann Erholung, 100 Watt“, ramps with their start watts, at the end „… dann geschafft“); where the power changes (and at the end) also beeps at 3, 2 and 1 s, the last one higher. No cues while paused or in free rides.
- Speaker button in the chart head cycling announcement + beeps / beeps only / off, remembered per device in localStorage and coloured by mode (green: both, blue: beeps only, grey: off). It blinks red while iOS keeps sound locked (after a reload or a start at the cockpit); a tap unlocks it. The start button of the program dialog unlocks sound as well.
- Beeps play from `<audio>` elements with generated WAV tones; on iOS, Web Audio turns silent after the second announcement while still reporting `running`. Web Audio remains for the button's lock state and until the elements are unlocked.
- README: cues feature; iOS mutes `speechSynthesis` in silent mode while the beeps still play.

### Changed
- Training screen on tablets (wider than 800 px): ride time, speaker and wrench icon at double size.

## [1.5.9] - 2026-10-30

### Added
- `CHANGELOG.md`; git tags and GitHub releases for every version since 1.0.0.

## [1.5.8] - 2026-10-30

No changes to the application (ideas moved to the issue tracker).

## [1.5.7] - 2026-10-30

### Added
- Training screen: wrench icon in the chart head opens the service page while riding (e.g. to check the chest strap).
- Service page: "Zurück zur Fahrt" button while a ride is running.

## [1.5.6] - 2026-10-29

### Added
- Hidden `demo-data` command: writes a demo database with two riders and four weeks of rides.
- Screenshot script (`tools/screenshots`) driving the server with the simulated ergometer and strap.

### Changed
- Simulator: cadence and pulse follow the load, also for the simulated strap; the simulated strap never equals the ear pulse.

## [1.5.5] - 2026-10-29

No changes to the application (public mirror tooling).

## [1.5.4] - 2026-10-29

No changes to the application (public mirror tooling).

## [1.5.3] - 2026-10-29

### Changed
- Licensed under MIT.

## [1.5.2] - 2026-10-29

### Added
- Chest strap: the reason a connect failed is shown in the strap tab, the log and the Live trace.

## [1.5.1] - 2026-10-29

### Added
- Chest strap over Bluetooth (heart rate measurement characteristic) with a BlueZ backend and a simulated strap (`KETTLER_BLUETOOTH`).
- Strap per rider, pulse source shown on the training screen, strap pulse stored per sample.
- Recovery measurement with the strap on the server's own 60 s clock; the ride stays paused meanwhile.
- Service page tab "Brustgurt": scan, register and connect straps; Bluetooth state in the service status.
- `deploy/compose.bluetooth.yaml` and the AppArmor profile `kettler-bluez` (D-Bus access to BlueZ only).
- Service page in the menu (wrench on the right, tab on phones).

### Changed
- Database schema v6: straps, strap per person, strap pulse per sample, recovery source.

## [1.4.1] - 2026-10-27

### Added
- Start preview: program chart with axis labels, elevation, program stats and the rider's history with that program.

## [1.4.0] - 2026-10-27

### Added
- Service page tabs "Zustand" (device, firmware via `VE`, USB adapter, database, server) and "Live" (raw device traffic and modem lines over `/ws/service`).

## [1.3.0] - 2026-10-27

### Added
- Service page with database backup download and restore (SQLite backup API).
- Restore checks the schema and references and replaces data only; all open clients reload afterwards (`data_epoch`).

## [1.2.2] - 2026-10-26

### Added
- `deploy/compose.serial.yaml` for a USB serial adapter in the Docker host.

## [1.2.1] - 2026-10-26

### Changed
- CI: GitHub Actions on Node 24, pinned by commit SHA, updated by Dependabot.

## [1.2.0] - 2026-10-26

### Added
- Elevation profile modelled from power, with "Hm bergauf" and a preview from the program.
- Rider profile with weight and flat power.

### Changed
- Database schema v5: rider weight and flat power, elevation snapshot per ride.

## [1.1.2] - 2026-10-26

### Fixed
- Keypad: the DOWN key is powered via RTS; raw modem lines are logged.

## [1.1.1] - 2026-10-26

### Fixed
- Keypad: DTR is set after the async reopen on Windows, so the keypad gets power.

## [1.1.0] - 2026-10-26

### Added
- Handlebar keypad on the Y cable controls the ride (power, pause, stop, large view), read from the RS232 modem lines directly or through the bridge's second TCP port (`KETTLER_KEYS`).
- Large chart view, switched by a tap on the chart or by the keypad.
- Rename persons.

## [1.0.3] - 2026-10-25

### Fixed
- Phones: stat and summary values fit their tiles.

## [1.0.2] - 2026-10-25

### Added
- Recovery measurements are stored with the ride and marked in the charts; result shown as fitness grade F1–F6.

## [1.0.1] - 2026-10-25

### Added
- Cadence in history, summary, comparison and charts (database schema v3 with backfill).

### Fixed
- The cockpit's recovery measurement no longer drops the connection.

## [1.0.0] - 2026-10-25

First release.

### Added
- Training server for the Kettler AX1: serial or TCP bridge connection, training session state machine with auto-pause, power control.
- Programs with templates, editor and validation; free ride.
- Ride history with comparison; delete persons and rides.
- Web app for iPad and iPhone: rider selection, training screen, chart with grid and scrub crosshair, work in kJ and kcal.
- SQLite storage, HTTP API and websocket, host allowlist and origin check.
- Version with git commit in the UI and `GET /api/version`.
- Docker image on GHCR and compose file.

[Unreleased]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.10...HEAD
[1.5.10]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.9...v1.5.10
[1.5.9]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.8...v1.5.9
[1.5.8]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.7...v1.5.8
[1.5.7]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.6...v1.5.7
[1.5.6]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.5...v1.5.6
[1.5.5]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.4...v1.5.5
[1.5.4]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.3...v1.5.4
[1.5.3]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.2...v1.5.3
[1.5.2]: https://github.com/bnitsche/kettler-ax1/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/bnitsche/kettler-ax1/compare/v1.4.1...v1.5.1
[1.4.1]: https://github.com/bnitsche/kettler-ax1/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/bnitsche/kettler-ax1/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/bnitsche/kettler-ax1/compare/v1.2.2...v1.3.0
[1.2.2]: https://github.com/bnitsche/kettler-ax1/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/bnitsche/kettler-ax1/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/bnitsche/kettler-ax1/compare/v1.1.2...v1.2.0
[1.1.2]: https://github.com/bnitsche/kettler-ax1/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/bnitsche/kettler-ax1/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/bnitsche/kettler-ax1/compare/v1.0.3...v1.1.0
[1.0.3]: https://github.com/bnitsche/kettler-ax1/compare/v1.0.2...v1.0.3
[1.0.2]: https://github.com/bnitsche/kettler-ax1/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/bnitsche/kettler-ax1/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/bnitsche/kettler-ax1/releases/tag/v1.0.0

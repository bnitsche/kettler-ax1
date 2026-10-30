# Kettler AX1 – Training Server

A training server and web app for the Kettler AX1 ergometer (Siemens cockpit, RS232, built around 2002). The server talks to the ergometer over its serial port; the web app on iPad and iPhone shows live values, runs interval programs, controls the load and records every ride.

> **Mirror:** This repository is published from a private development repository, one commit per release. Issues are welcome; changes from pull requests are carried over there and come back with the next release.

> **Note:** The web app's user interface is German only.

![Training screen on the iPad (landscape) halfway through the program “Pyramide”: the whole program's chart at the top with the ridden part, pulse, cadence and the elevation strip, the current power in a ring in the middle, cadence, pulse, speed and energy on the left, buttons for power, pause and stop on the right](docs/images/training-ipad.png)

*Training screen halfway through the program „Pyramide“ (recorded with the simulated ergometer and demo data).*

## Features

- **Live training:** power, target, cadence, pulse, speed, distance and energy, with a chart of the whole program and the part already ridden.
- **Interval programs:** blocks of steps with fixed or ramped wattage; the server sets the load on the ergometer second by second. Four templates included, own programs in the editor.
- **Cues:** 10 s before each step the iPad announces it („Noch 10 Sekunden, dann Erholung, 100 Watt“); where the power changes it also beeps at 3, 2, 1. The speaker button in the chart head switches between announcement and beeps, beeps only and off. iOS mutes the announcement in silent mode; the beeps still play.
- **Elevation:** the power is turned into a climb (per rider weight), with a planned profile before the start and the ridden one during the ride.
- **Riders:** several people with their own profile, history and chest strap.
- **History and comparison:** every ride with its key figures, a detail view with all curves, and two rides side by side.
- **Recovery measurement:** recovery pulse and fitness grade after a ride, from the cockpit's button or with the chest strap.
- **Chest strap:** a Bluetooth heart rate strap (e.g. Garmin HRM-Dual) read by the server, optional.
- **Handlebar keypad:** the AX1's four direction keys work during a ride: UP/DOWN ±10 W, RIGHT pause, LEFT switches the chart view, LEFT held for 2 s stops the ride.
- **Service page:** connection status, live trace of the device traffic, chest straps, backup and restore in the browser.

## How it fits together

![Devices and connections: iPad and iPhone reach the kettler server on a mini PC over Wi-Fi (HTTP and WebSocket); the server talks to the Kettler AX1 through a USB serial adapter (RS232, 9600 baud) and to a chest strap over Bluetooth LE; alternatively a PC at the ergometer runs the kettler bridge and passes the serial line on over TCP](docs/images/system.svg)

One Rust program does everything. As `server` it runs the training logic, stores the rides in SQLite and serves the web app. The ergometer is plugged into the server's computer with a USB serial adapter, or into another PC that runs the same program as a `bridge` and passes the serial line on over the network. All devices that open the web app see the same ride.

## Screenshots

<table>
  <tr>
    <td align="center"><img src="docs/images/training-select.png" width="400" alt="Training selection with the riders Bob and Alice and the programs to choose from"><br><sub>Choosing rider and program</sub></td>
    <td align="center"><img src="docs/images/start-dialog.png" width="400" alt="Preview of the program Pyramide with its step profile, planned elevation and the rider's figures from last time"><br><sub>Program preview before the start</sub></td>
  </tr>
  <tr>
    <td align="center"><img src="docs/images/programs-editor.png" width="400" alt="Program editor with the program Pyramide open, showing blocks and sections"><br><sub>Program editor</sub></td>
    <td align="center"><img src="docs/images/history.png" width="400" alt="History of Bob with his rides and key figures per ride"><br><sub>A rider's history with key figures per ride</sub></td>
  </tr>
  <tr>
    <td align="center"><img src="docs/images/ride-detail.png" width="400" alt="Ride detail with power, pulse from strap and ear clip, cadence, elevation strip and a recovery measurement"><br><sub>Ride detail with elevation and recovery</sub></td>
    <td align="center"><img src="docs/images/compare.png" width="400" alt="Comparison of two rides of the same program with power curves and key figures side by side"><br><sub>Comparing two rides</sub></td>
  </tr>
  <tr>
    <td align="center"><img src="docs/images/service-status.png" width="400" alt="Service page, tab Zustand: device connected, running ride, server version and database"><br><sub>Service page: status</sub></td>
    <td align="center"><img src="docs/images/training-iphone.png" width="190" alt="Training screen on the iPhone in portrait with the power ring and buttons"><br><sub>Training on the iPhone</sub></td>
  </tr>
</table>

## Installation

Hardware, the ways to install it (Docker, systemd, Proxmox, bridge on Windows), configuration, backup and troubleshooting: see **[deploy/README.md](deploy/README.md)**.

## License

[MIT](LICENSE)

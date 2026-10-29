# Installation and operation

How to run the Kettler AX1 training server: hardware, the ways to install it, configuration, backup and troubleshooting. For what the app does, see the [README](../README.md).

## Contents

- [Hardware](#hardware)
- [Modes and device link](#modes-and-device-link)
- [Variants](#variants): [A1 mini PC with Linux](#variant-a1-mini-pc-just-for-the-kettler-plain-linux-without-proxmox), [A2 Proxmox LXC](#variant-a2-mini-pc-with-proxmox-server-in-an-lxc-container), [B bridge](#variant-b-server-elsewhere-bridge-at-the-ergometer)
- [Deployment with Komodo](#deployment-with-komodo-from-github)
- [Chest strap: host setup](#chest-strap-host-setup)
- [On the iPad](#on-the-ipad), [Host names](#host-names-kettler_allowed_hosts), [Configuration](#configuration)
- [Shutdown](#shutdown), [Backup](#backup), [Troubleshooting](#troubleshooting), [Building from source](#building-from-source)

## Hardware

- Kettler AX1 with Siemens cockpit, serial 9600 baud, 8N1, commands terminated with `\r\n`.
- USB-RS232 adapter: FTDI recommended (no special driver, also works on Linux). The old Prolific PL2303HXA only works with Prolific driver 3.3.2.102; driver 3.9.6.0 blocks the chip.
- Optional later: a Wi-Fi bridge directly at the ergometer (Elfin-EW10A or ESP32 with MAX3232). Not decided yet.

## Modes and device link

One Rust program (one binary, one Docker image) with three modes:

- `server`: Kettler protocol, training logic, recording, SQLite database, web UI and a websocket for live values.
- `bridge`: forwards a local serial port unchanged over TCP (port 4001) and reports the keypad buttons on a second port (4002). Runs on the computer the ergometer is plugged into.
- `simulate`: a simulated ergometer for development and testing without pedaling.

The connection to the device is chosen in the configuration, e.g. via `KETTLER_DEVICE`:

- `tcp://192.168.1.50:4001` (the usual case, to the bridge or later to a Wi-Fi module)
- `serial://COM3` (device on the same computer)
- `sim://` (simulation)

## Variants

There are two ways to run it. Either the USB adapter is plugged directly into the computer running the server (variants A1 and A2, no bridge needed), or the server runs elsewhere and a bridge at the ergometer provides the connection (variant B).

### Variant A1: mini PC just for the Kettler, plain Linux without Proxmox

The simplest setup if the computer next to the ergometer is there only for this. Debian or Linux Mint; the FTDI USB-RS232 adapter is plugged directly into the PC and shows up as `/dev/ttyUSB0`. To check whether the adapter was detected:

```
ls /dev/serial/by-id/
dmesg | tail
ls -l /dev/ttyUSB0
```

**Device path.** The stable path from `/dev/serial/by-id/` is recommended, e.g. `serial:///dev/serial/by-id/usb-FTDI_FT232R_USB_UART_A1B2C3-if00-port0`. It stays the same even if other USB serial devices are plugged in or the adapter moves to another socket. `/dev/ttyUSB0` is shorter but can become `ttyUSB1` after replugging or with a second adapter. The examples below use `/dev/ttyUSB0`; the by-id path works the same everywhere.

The `dialout` group is only needed if you start `kettler server` by hand as your own user:

```
sudo usermod -aG dialout $USER     # log in again afterwards
```

It is not needed with Docker Compose (the group is in the image) or with the systemd service (`SupplementaryGroups=dialout`).

**With Docker Compose.** Install Docker, enable the variant A lines in [`docker-compose.yml`](../docker-compose.yml) (repository root) (`KETTLER_DEVICE=serial:///dev/ttyUSB0` instead of the `tcp://` line, plus the commented-out `devices:` block) and start:

```yaml
    environment:
      - KETTLER_DEVICE=serial:///dev/ttyUSB0
    devices:
      - /dev/ttyUSB0:/dev/ttyUSB0
```

```
mkdir -p data && sudo chown 10001:10001 data
docker compose up -d --build
```

Caution: with a `devices:` entry the container won't start if the adapter is not plugged in at startup (Docker can't find the device). Without Docker, as a systemd service, the server keeps running and connects as soon as the adapter appears.

The container runs as user `kettler` (uid 10001) in the image's `dialout` group. If the device's group on the host does not match the image's `dialout` group, add `group_add` with the numeric group ID (`stat -c %g /dev/ttyUSB0`) to the compose file.

**Without Docker.** Copy the binary (`cargo build --release`) to `/usr/local/bin/kettler` and run it as a systemd service. Example `/etc/systemd/system/kettler.service`:

```ini
[Unit]
Description=Kettler AX1 training server
After=network.target

[Service]
User=kettler
SupplementaryGroups=dialout
Environment=KETTLER_DEVICE=serial:///dev/ttyUSB0
Environment=KETTLER_DB=/var/lib/kettler/kettler.db
ExecStart=/usr/local/bin/kettler server --listen 0.0.0.0:8080
Restart=on-failure
StateDirectory=kettler

[Install]
WantedBy=multi-user.target
```

Create the user first (`sudo useradd --system kettler`). Enable with `sudo systemctl enable --now kettler`.

### Variant A2: mini PC with Proxmox, server in an LXC container

A mini PC (e.g. Lenovo M910q with Proxmox) stands next to the ergometer with the FTDI adapter plugged into it. The server runs in an unprivileged Debian LXC into which the USB adapter is passed through. The `bridge` is not needed.

**1. Find the adapter (on the Proxmox host).** To check: `ls -l /dev/ttyUSB0` and `ls /dev/serial/by-id/`. The by-id path shows which adapter is behind `ttyUSB0`; `/dev/ttyUSB0` is what gets passed through here.

**2. Pass it through (Proxmox 8).** Add to `/etc/pve/lxc/<id>.conf` (or in the UI: Container → Resources → Add → Device Passthrough):

```
dev0: /dev/ttyUSB0
```

If the permissions inside the container are not enough (the device belongs to `nobody` there), add the options `gid=<GID>`, `uid=<UID>` or `mode=0666` to `dev0`, e.g. `dev0: /dev/ttyUSB0,gid=20` for the `dialout` group.

**3. Allow Docker in the LXC.** If Docker should run inside the container, it needs `nesting` and `keyctl` under Options → Features (in the config: `features: nesting=1,keyctl=1`). Then restart the container and check inside: `ls -l /dev/ttyUSB0`.

**4. Start the server.** Inside the LXC as in A1 with Docker Compose or a systemd service; as in A1 the `dialout` group is not needed there. If `ls -l /dev/ttyUSB0` inside the container shows a group or permissions the service doesn't have, adjust `gid=`/`mode=` on `dev0` (see above) or use `group_add` in the compose file.

### Variant B: server elsewhere, bridge at the ergometer

The server runs on a NAS or Docker host that is not next to the ergometer. At the ergometer, a PC runs the bridge, which forwards the serial port over TCP.

**Start the server with Docker.** Put the bridge PC's IP into `KETTLER_DEVICE=tcp://<IP>:4001` in [`docker-compose.yml`](../docker-compose.yml) (repository root), then:

```
mkdir -p data && sudo chown 10001:10001 data
docker compose up -d --build
```

The `chown` is needed because Docker would otherwise create `./data` as `root` and the server (user with uid 10001) could not open the database.

Without Compose: `docker build -t kettler .` and `mkdir -p data && sudo chown 10001:10001 data`, then `docker run -d -p 8080:8080 -v "$PWD/data:/data" -e KETTLER_DEVICE=tcp://192.168.1.50:4001 kettler`. To try it without a device: `-e KETTLER_DEVICE=sim://`.

**Start the bridge on Windows.** Copy `kettler.exe` (from `cargo build --release`, in `target/release/`) to the PC at the ergometer. The adapter's COM number is in Device Manager under “Ports (COM & LPT)”.

```
kettler.exe bridge --port COM3 [--listen 0.0.0.0:4001] [--keys-listen 0.0.0.0:4002]
```

By default the bridge listens on `0.0.0.0:4001` for the device data (change with `--listen`) and on `0.0.0.0:4002` for the keypad buttons (`--keys-listen`; the server connects to it and turns presses into actions). At startup it opens the port once as a probe and logs a clear error if that fails; it keeps running anyway, so the adapter can be plugged in later. If port 4002 is already taken, it logs that as an error and runs without a keypad channel. On first start the Windows firewall asks; allow access on private networks. Or create a rule as administrator:

```
netsh advfirewall firewall add rule name="Kettler Bridge" dir=in action=allow protocol=TCP localport=4001
netsh advfirewall firewall add rule name="Kettler Bridge Keys" dir=in action=allow protocol=TCP localport=4002
```

Optionally start the bridge automatically at logon: in Task Scheduler create a task with the trigger “At log on” and the action “Start a program” with `kettler.exe` and the arguments `bridge --port COM3`.

Later the PC can be replaced by a Wi-Fi module directly at the ergometer (Elfin-EW10A or ESP32). For the server nothing changes: same URL `tcp://<IP>:4001`.

## Deployment with Komodo (from GitHub)

On every push to `main`, GitHub Actions (`.github/workflows/docker.yml`) runs the Rust and Node tests and then builds the image `ghcr.io/bnitsche/kettler-ax1` with the tags `latest`, `main` and `sha-<short>`. Komodo pulls this image and starts it with [`compose.yaml`](compose.yaml). The workflow can also be started by hand (Actions → Docker → Run workflow, also for other branches). It additionally builds `kettler.exe` for Windows and attaches it as the artifact `kettler-windows` to the workflow run (available for 30 days). The actions used are pinned by commit SHA (the version is in a comment next to it); in the development repository Dependabot proposes updates once a week as a grouped pull request.

**One-time setup in Komodo:**

1. Credentials (only needed if the repository or the GHCR package is private). For ghcr.io you need a classic Personal Access Token (classic PAT) with the scope `read:packages`; fine-grained tokens don't work with GHCR. In Komodo under Settings → Providers or registry accounts, add an account for the domain `ghcr.io` with your GitHub user and the token. If Komodo should read the compose file from a private repository, it also needs a git provider account for `github.com` with read access to the repository (scope `repo`, or a fine-grained token with “Contents: read”; fine-grained only works for this git account).
2. Create a stack. Source “Git Repo”: `bnitsche/kettler-ax1`, branch `main`, run directory `deploy`, file `compose.yaml` (Komodo's default name, the “File Paths” field can stay empty). Alternatively paste the compose file directly in the UI (“UI Defined”). Select the ghcr.io registry account for pulling images, if you created one. Under Environment add:
   - Bridge on a Windows PC: `KETTLER_DEVICE=tcp://<PC-IP>:4001`
   - USB adapter plugged directly into the Docker host (variant A): under “File Paths” add `compose.yaml` and `compose.serial.yaml`, set `KETTLER_DEVICE=serial:///dev/ttyUSB0` and `KETTLER_SERIAL` to the host path, e.g. `/dev/serial/by-id/usb-FTDI_FT232R_USB_UART_…-if00-port0` (default `/dev/ttyUSB0`). Inside the container the device is always `/dev/ttyUSB0`. If the device's group differs from 20 (`dialout`), set `KETTLER_SERIAL_GID` (`stat -c %g /dev/ttyUSB0`). The server then reads the keys itself; leave `KETTLER_KEYS` empty.
   - Chest strap via Bluetooth: additionally add `compose.bluetooth.yaml` under “File Paths” (see [Chest strap: host setup](#chest-strap-host-setup)).
   - Optional: `KETTLER_PORT` (default 8080), `KETTLER_ALLOWED_HOSTS`, `KETTLER_TAG`, `KETTLER_KEYS` (empty = automatic, `off` = no keys), `KETTLER_BLUETOOTH`, `KETTLER_APPARMOR`
3. Press “Deploy”. After a new push press “Deploy” again, or set up Komodo's polling for new images and automatic stack updates, or a webhook (the exact names differ between Komodo versions).

For a fixed version set `KETTLER_TAG=sha-<short>`, e.g. `sha-1a2b3c4`. Without it, `latest` applies. The image is only available after the workflow has run once (on `main`; to test a branch, set `KETTLER_TAG` to the branch tag, e.g. `feat-deploy`). If `KETTLER_DEVICE` is missing, the stack fails on purpose. `pull_policy: always` makes sure “Deploy” really pulls a new `latest`.

**Data and backup.** [`compose.yaml`](compose.yaml) uses the named volume `kettler-data`, so no `chown` is needed. The easiest way to back up or move is the service page in the browser (see [Backup](#backup)). Backing up the whole volume by hand (the name is fixed to `kettler-data` in the compose file, though tools may prefix it with the stack name):

```
docker stop <container>
docker run --rm -v kettler-data:/data -v "$PWD:/backup" debian tar czf /backup/kettler-data.tgz -C /data .
docker start <container>
```

**Running with the bridge.** On the Windows PC at the ergometer:

```
kettler.exe bridge --port COM3 --listen 0.0.0.0:4001 --keys-listen 0.0.0.0:4002
```

Open ports 4001 (device) and 4002 (keys) in the Windows firewall (PowerShell as administrator):

```
New-NetFirewallRule -DisplayName "Kettler Bridge" -Direction Inbound -Protocol TCP -LocalPort 4001,4002 -Action Allow -Profile Private
```

Either build `kettler.exe` locally with `cargo build --release` or download it from GitHub under Actions → run → Artifacts (`kettler-windows`).

## On the iPad

Open `http://<server>:8080` in Safari, then Share → “Add to Home Screen”.

Important: keeping the display awake (Wake Lock) only works over https or `localhost`. Over plain `http` the screen does not stay on by itself. So on the iPad set Settings → Display & Brightness → Auto-Lock → “Never” while training.

## Host names (`KETTLER_ALLOWED_HOSTS`)

The server only accepts API requests (including reads) and the websocket for local host names. Without configuration, IP addresses, `localhost`, single names without a dot such as `nas`, and names ending in `.local`, `.lan`, `.home.arpa` and `.fritz.box` work. Any other names, e.g. behind a reverse proxy, go comma-separated and without port into `KETTLER_ALLOWED_HOSTS`, e.g. `KETTLER_ALLOWED_HOSTS=kettler.example.org,training.example.org`.

## Configuration

| Setting | Default | Meaning |
| --- | --- | --- |
| `KETTLER_DEVICE` | `sim://` (with a warning in the log) | connection to the device (`tcp://…`, `serial://…`, `sim://`) |
| `KETTLER_DB` | `./kettler.db` (in the Docker image `/data/kettler.db`) | path of the SQLite database |
| `KETTLER_ALLOWED_HOSTS` | empty | additional host names, comma-separated |
| `--listen` (server) | `0.0.0.0:8080` | address and port of the server |
| `KETTLER_KEYS` | empty | source of the keypad buttons: empty = automatic (`tcp://` device: same host, port 4002; `serial://`: the server reads the lines itself; `sim://`: none), `off` = no keys, `tcp://host:port` = this address. An invalid value prevents startup. |
| `KETTLER_BLUETOOTH` | empty | chest strap: empty or `auto` = try BlueZ over D-Bus (off on Windows), `off` = no Bluetooth, `sim` = simulated strap. An invalid value prevents startup. |
| `KETTLER_APPARMOR` | `docker-default` | only with `compose.bluetooth.yaml`: the container's AppArmor profile, `kettler-bluez` on Ubuntu and Linux Mint (see [Chest strap: host setup](#chest-strap-host-setup)) |
| `--listen` (bridge) | `0.0.0.0:4001` | address and port of the bridge for the device data |
| `--keys-listen` (bridge) | `0.0.0.0:4002` | address and port of the bridge for the keys |

## Chest strap: host setup

Since 1.5.0 the server reads a Bluetooth chest strap itself, for example the Garmin HRM-Dual (standard Heart Rate profile over Bluetooth LE). iPhone and iPad cannot do Bluetooth in the browser, so the server talks to the strap through BlueZ on the Docker host. Bluetooth is **optional**: without a dongle, without BlueZ, on Windows or without a registered strap, everything works as before with the pulse from the cockpit (ear clip or Kettler plug-in receiver).

**Setup on the Docker host** (tested on Debian 13 with a Realtek RTL8761BU dongle in a Proxmox VM):
1. Pass the USB dongle through to the VM: in Proxmox at the VM under Hardware → Add → USB Device, easiest by vendor and product ID. This works while the VM is running. Don't change settings of other USB devices (such as “USB3”) during a workout, or the ergometer may briefly lose its connection.
2. Install firmware and BlueZ. Realtek dongles need `firmware-realtek` from `non-free-firmware`; on Debian 13 the package source is in `/etc/apt/sources.list.d/debian.sources`:
   ```
   sudo sed -i '/^Components:/{/non-free-firmware/!s/$/ non-free-firmware/}' /etc/apt/sources.list.d/debian.sources
   sudo apt update && sudo apt install firmware-realtek bluez
   sudo modprobe -r btusb && sudo modprobe btusb
   ```
3. Check: `bluetoothctl show` lists a controller, and `bluetoothctl --timeout 20 scan le` finds the strap while it is worn (e.g. “HRM-Dual:…”). Pairing is not needed. If `sudo dmesg | grep -i bluetooth` shows a timeout (`tx timeout`), reloading `btusb` once more often helps.
4. Make the container's user (uid 10001) known on the host, otherwise D-Bus won't let it talk to BlueZ:
   ```
   sudo useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin kettler
   sudo usermod -aG bluetooth kettler
   ```
5. Only on Ubuntu, Linux Mint and other hosts whose kernel checks D-Bus via AppArmor (tested on Linux Mint): Docker's default profile blocks the way to BlueZ there, and the status shows “An AppArmor policy prevents this sender …”. The profile [`apparmor/kettler-bluez`](apparmor/kettler-bluez) is the default profile plus D-Bus, restricted to BlueZ. Load it on the host (survives a reboot):
   ```
   sudo cp kettler-bluez /etc/apparmor.d/kettler-bluez
   sudo apparmor_parser -r -W /etc/apparmor.d/kettler-bluez
   ```
   and set `KETTLER_APPARMOR=kettler-bluez` in the stack. Not needed on Debian.
6. In Komodo, add `compose.bluetooth.yaml` under “File Paths”. It only mounts the D-Bus socket `/run/dbus/system_bus_socket`; the container gets no extra privileges. Leave `KETTLER_BLUETOOTH` empty (automatic).
7. On the service page in the „Brustgurt“ tab, wear the strap, press „Gurt suchen“ (search), then „Anmelden“ (register). If there are several straps, pick your own under „Profil“ for each rider.

![The „Brustgurt“ (chest strap) tab on the service page](../docs/images/brustgurt.png)

The tab shows the status („Bluetooth: bereit“ = ready, or „nicht verfügbar“ = unavailable, with a reason), every strap with connection, pulse and battery, and lets you rename and remove them. If connecting fails, the last reason is shown next to it („zuletzt: …“); it is also written to the log and to the „Live“ tab whenever it changes. The server only connects to registered straps and retries every few seconds after a drop. The HRM-Dual only holds **one** Bluetooth connection: while the server is connected, a phone cannot get the strap. A Garmin watch can keep listening via ANT+. If you need the strap on your phone, remove it in the tab; the server then releases it.

If the Bluetooth dongle was unplugged and replugged while running, „Gurt suchen“ still finds the strap but it no longer connects (BlueZ reports it as connected, the server as unreachable). A reboot of the host helps, or as a last resort `bluetoothctl remove <address>` and `sudo systemctl restart bluetooth`.

If searching or connecting fails with a D-Bus permission error, BlueZ needs its own rule for the user `kettler`. Create a file `/etc/dbus-1/system.d/kettler-bluetooth.conf` that allows the user `kettler` `send_destination="org.bluez"`, and reload the service.

For development without hardware there is `KETTLER_BLUETOOTH=sim`: a simulated „Sim-Gurt“ that can be found and registered and sends a fluctuating pulse (also on Windows).

## Shutdown

On Ctrl+C or `SIGTERM` (e.g. `docker compose stop`, `systemctl stop`) the server ends a running ride like „Beenden“ (stop), saves it, sets the ergometer to 25 W and waits at most about 3 s for that. Then it stops. For `SIGTERM` to reach the server inside the container, the compose file has `init: true`.

When connecting to the ergometer, the server sets the load to 25 W if no ride is running. A ride ends by itself if nobody starts pedaling within 10 minutes after the start. If the device doesn't answer for more than 5 s during a ride, that counts like a pedaling pause: if it comes back, the ride continues, otherwise it ends after 10 minutes. Rides without a single sample are not stored.

## Backup

**In the browser (since 1.3.0):** The service page `http://<server>:8080/#/service` opens with the wrench on the right of the menu (on phones the „Service“ tab).
- „Backup herunterladen“ (download backup) saves the whole database as `kettler-backup-YYYY-MM-DD-HHMM.db` (a plain SQLite file). This also works during a ride.
- „Backup einspielen …“ (restore backup) checks the file, shows how many riders, programs and rides it contains, and after confirmation replaces **all** of the server's data. Backups from older versions are upgraded; backups from a newer version and files that aren't a Kettler database are rejected. Restoring is blocked during a ride. Afterwards all open devices reload the page.
- **Moving to a new computer:** download the backup on the old server, start the new server, restore the backup there, done. The volume does not need to be copied.

**By hand:** All data is in `data/kettler.db`. The database runs in WAL mode, so the files `kettler.db-wal` and `kettler.db-shm` may sit next to it. For a backup, either stop the container first (`docker compose stop`) and copy all `kettler.db*` files, or take a consistent copy while running:

```
sqlite3 data/kettler.db ".backup 'kettler-backup.db'"
```

## Troubleshooting

- **First stop: the service page** `http://<server>:8080/#/service` (wrench in the menu):
  - „Zustand“ (status) shows whether the device is connected and since when, the last disconnect with its reason, device ID and firmware (`VE`), the USB adapter with serial number, the keypad channel, the running ride, the server version and the database.
  - „Live“ shows all of the device's readings, the four modem lines of the handlebar keys as lamps (only with a local serial port) and the raw traffic with timestamps: sent commands, answers, discarded lines, missing answers, keys and connection changes. „Anhalten“ (hold) freezes the list for reading. The server keeps only the last 200 entries, in memory.
  - „Backup“: see [Backup](#backup).
- **Adapter not detected:** Windows: look in Device Manager under “Ports (COM & LPT)” and note the COM number. Linux: `ls /dev/serial/by-id/` and `dmesg | tail`. The FTDI adapter needs no special driver.
- **Old Prolific PL2303HXA:** only works with driver 3.3.2.102. Driver 3.9.6.0 blocks the chip (error 433 when opening). Better use an FTDI adapter.
- **No answer from the device:** The cable to the cockpit may need a null-modem adapter (transmit and receive lines swapped). Try with and without a null-modem adapter, e.g. with `python kettler_test.py COM3` (needs `pip install pyserial`; it sends `RS`, `ID`, `VE`, `CM` and `ST`, so pedal while it runs). Settings: 9600 baud, 8N1.
- **`CM` is not acknowledged:** The device only answers `CM` while someone is pedaling. Pedal steadily while testing.
- **“Access denied” when opening the port:** Another program holds the port, e.g. a second bridge, a test script or a terminal program. Close it and try again.
- **Server doesn't start, message “Error: opening /data/kettler.db” with “Caused by: unable to open database file”:** The data directory is not writable for the container user. Run `sudo chown 10001:10001 data`.
- **Server reports „Unbekannter Hostname“ (unknown host name):** Add the name to `KETTLER_ALLOWED_HOSTS` (see above).
- **Handlebar keys don't react:** Is a ride running (keys only act then)? With `tcp://`, open port 4002 on the bridge PC (firewall) and update the bridge together with the server; the chart header shows the badge „Tasten“ while the keypad channel is connected (that says nothing about whether a keypad is plugged in). `KETTLER_KEYS=off` disables the keys. With a new adapter, run `python tools/kettler_keypad_probe.py COM3` directly on the port (not through the bridge) and press each key: UP should switch CTS and RI, DOWN DSR, LEFT DCD, RIGHT CTS alone.
- **Server starts but shows no values:** Check with `KETTLER_DEVICE=sim://` whether the app itself works. Then check `KETTLER_DEVICE`, IP, port 4001 and the firewall on the bridge PC.

## Building from source

```
cargo test                                   # Rust tests
node --test web/lib/*.test.js                # web app tests (the directory form fails under Node 24)
KETTLER_DEVICE=sim:// cargo run -- server    # server with a simulated device, http://localhost:8080
cargo run -- simulate                        # simulated ergometer as a TCP server (127.0.0.1:4001)
cargo run -- demo-data --db demo.db          # new demo database with Bob and Alice and four weeks of rides
```

The screenshots in this README are made by a script, see [tools/screenshots/README.md](../tools/screenshots/README.md).

In PowerShell set the environment variable with `$env:KETTLER_DEVICE = "sim://"`. The web app lives in `web/` and is embedded into the binary at build time.

"""Messskript fuer Kettler AX1: vermisst Geraeteverhalten (PW-Raster, Tretpause, Reconnect, Einheiten).

Ein Mensch muss am Geraet sitzen und treten; das Skript gibt Anweisungen mit sichtbarem Countdown (kein Enter noetig).
Wichtig: CM/ST/PW antworten nur, solange getreten wird.
Aufruf: python tools/kettler_probe.py [PORT_ODER_URL] [--wait-scale FAKTOR]
  --wait-scale multipliziert alle Countdowns (0 = ohne Wartezeit, zum schnellen Testen).
  Standard: COM3. Auch URLs wie socket://127.0.0.1:4001 sind moeglich (z.B. ueber die Bridge).
Die Ausgabe wird zusaetzlich in kettler_probe-JJJJMMTT-HHMMSS.log geschrieben.
"""
import argparse
import sys
import time
from datetime import datetime

import serial

ST_FIELDS = ["Puls", "U/min", "km/h", "Distanz", "Soll-Watt", "Energie", "Zeit", "Ist-Watt"]
PW_VALUES = [20, 25, 27, 30, 33, 100, 250, 400, 450, 600]
ST_PER_PW = 3
PAUSE_SECONDS = 20
CONST_PW = 100
CONST_SECONDS = 180
CM_ATTEMPTS = 30

LOG = None
T0 = time.monotonic()
PORT = "COM3"
WAIT_SCALE = 1.0


def log(line=""):
    stamped = f"[{time.monotonic() - T0:7.1f}s] {line}" if line else ""
    print(stamped)
    if LOG:
        LOG.write(stamped + "\n")
        LOG.flush()


def countdown(instruction, seconds):
    """Anweisung deutlich anzeigen, dann sekundenweise auf einer Zeile herunterzaehlen."""
    total = int(round(seconds * WAIT_SCALE))
    log("")
    log("*" * 60)
    log(f"*** JETZT: {instruction} ***")
    log("*" * 60)
    if total <= 0:
        return
    sys.stdout.write(f"noch {total} s ... ")
    sys.stdout.flush()
    for remaining in range(total - 1, 0, -1):
        time.sleep(1.0)
        sys.stdout.write(f"{remaining} ... ")
        sys.stdout.flush()
    time.sleep(1.0)
    print("los!")
    if LOG:
        LOG.write(f"[{time.monotonic() - T0:7.1f}s] (Countdown {total} s abgelaufen)\n")
        LOG.flush()


def open_port(target):
    if "://" in target:
        return serial.serial_for_url(target, baudrate=9600, bytesize=8, parity="N", stopbits=1, timeout=1)
    return serial.Serial(target, 9600, bytesize=8, parity="N", stopbits=1, timeout=1)


def open_port_retry(target, seconds=10.0):
    deadline = time.monotonic() + seconds
    while True:
        try:
            return open_port(target)
        except (serial.SerialException, OSError) as exc:
            if time.monotonic() >= deadline:
                log(f"FEHLER: {target} nicht zu oeffnen: {exc}")
                sys.exit(1)
            time.sleep(0.5)


def send(ser, cmd, wait=0.5):
    ser.reset_input_buffer()
    ser.write((cmd + "\r\n").encode("ascii"))
    time.sleep(wait)
    raw = ser.read_until(b"\n")  # wartet bis Zeilenende oder Timeout (1 s)
    if ser.in_waiting:
        raw += ser.read(ser.in_waiting)
    text = raw.decode("ascii", errors="replace").strip()
    log(f"> {cmd:<8} < {text!r}")
    return text


def parse_st(status):
    parts = status.split("\t")
    if len(parts) != len(ST_FIELDS):
        return None
    return dict(zip(ST_FIELDS, parts))


def show_st(status):
    st = parse_st(status)
    if st:
        log("   " + ", ".join(f"{k}={v}" for k, v in st.items()))
    return st


def soll(st):
    return int(st["Soll-Watt"]) if st and st["Soll-Watt"].isdigit() else None


def step_cm(ser, first=True, wait=15):
    log("\n=== CM bis ACK ===")
    if first:
        send(ser, "RS", wait=2.0)
    countdown("Mit dem Treten anfangen (gleichmaessig weitertreten)", wait)
    for attempt in range(1, CM_ATTEMPTS + 1):
        reply = send(ser, "CM", wait=2.0)
        if "ACK" in reply:
            log(f"CM bestaetigt nach {attempt} Versuch(en)")
            return
        log("   noch kein ACK, bitte treten ...")
    log(f"FEHLER: kein ACK auf CM nach {CM_ATTEMPTS} Versuchen, Abbruch")
    sys.exit(1)


def step_pw_grid(ser):
    log("\n=== 2. PW-Raster (Soll-Watt laut Geraet) ===")
    results = []
    for value in PW_VALUES:
        send(ser, f"PW {value}")
        seen = []
        for _ in range(ST_PER_PW):
            st = st_checked(ser)
            seen.append(st["Soll-Watt"] if st else None)
        results.append((value, seen))
    log("\nZusammenfassung PW -> Soll-Watt:")
    for value, seen in results:
        log(f"  PW {value:<4} -> {seen}")
    send(ser, "PW 25")


LAST_ST = None


def st_checked(ser, wait=1.0):
    global LAST_ST
    LAST_ST = show_st(send(ser, "ST", wait=wait))
    return LAST_ST


def step_pause(ser):
    log("\n=== 3. Tretpause ===")
    send(ser, "PW 50")
    countdown("gleich aufhoeren zu treten", 5)
    log(f"Ab jetzt NICHT treten, {PAUSE_SECONDS} s Pause")
    for i in range(PAUSE_SECONDS):
        log(f"Pause t={i + 1:>2} s")
        st_checked(ser)
    countdown("weitertreten", 5)
    log("Baseline nach der Pause:")
    st_checked(ser)
    log("Pruefe: wirkt PW 80 ohne erneutes CM?")
    send(ser, "PW 80")
    for _ in range(3):
        st_checked(ser)
    log("-> Sind Zeit, Distanz und Energie zurueckgesetzt? Soll-Watt == 80?")


def step_reconnect(ser):
    log("\n=== 4. USB-Kabel ziehen ===")
    ser.close()
    countdown("USB-Kabel ziehen", 10)
    countdown("wieder einstecken und weitertreten", 15)
    ser = open_port_retry(PORT)
    log(f"{PORT} neu geoeffnet")
    log("Ohne RS direkt ST (bis gueltige Antwort, max. 10 s):")
    start = time.monotonic()
    st = None
    while time.monotonic() - start < 10.0:
        st = st_checked(ser, wait=0.5)
        if st:
            break
    if st:
        log(f"Gueltige ST-Antwort nach {time.monotonic() - start:.1f} s")
    else:
        log("WARNUNG: keine gueltige ST-Antwort innerhalb 10 s nach Reconnect")
    send(ser, "PW 90")
    for _ in range(3):
        st_checked(ser)
    log("-> PC-Modus noch aktiv (Soll-Watt == 90)? Zaehler erhalten?")
    return ser


def step_units(ser):
    log("\n=== 5. Einheiten ===")
    st = LAST_ST
    if soll(st) != 90:
        log("PC-Modus nach Reconnect nicht bestaetigt (Soll-Watt != 90), CM erneut")
        step_cm(ser, first=False, wait=5)
    countdown(f"{CONST_SECONDS // 60} Minuten bei konstant PW {CONST_PW} treten", 5)
    send(ser, f"PW {CONST_PW}")
    st = st_checked(ser)
    if soll(st) != CONST_PW:
        log("!!!!! WARNUNG: PW 100 wurde NICHT uebernommen (Soll-Watt != 100) !!!!!")
    first = st
    start = time.monotonic()
    watts, speeds, last = [], [], first
    while time.monotonic() - start < CONST_SECONDS:
        st = st_checked(ser)
        if st:
            last = st
            if st["Ist-Watt"].isdigit() and st["km/h"].isdigit():
                watts.append(int(st["Ist-Watt"]))
                speeds.append(int(st["km/h"]))
        time.sleep(4.0)
    elapsed = time.monotonic() - start
    log(f"\nErster ST:  {first}")
    log(f"Letzter ST: {last}")
    if watts:
        kj = sum(watts) / len(watts) * elapsed / 1000
        km = sum(s / 10 for s in speeds) / len(speeds) * elapsed / 3600
        log(f"Verstrichen: {elapsed:.0f} s, mittlere Ist-Watt {sum(watts) / len(watts):.1f}")
        log(f"Erwartet Energie: {kj:.1f} kJ  oder  {kj / 4.184:.1f} kcal (Differenz letzter-erster Energie vergleichen)")
        log(f"Erwartet Distanz: {km:.2f} km (Differenz letzter-erster Distanz vergleichen; 0,1 km => Wert ~{km * 10:.0f})")
    log("-> Zuordnung ablesen: Energie in kJ oder kcal, Distanz in 0,1 km oder km.")
    send(ser, "PW 25")


def main():
    global LOG, PORT, T0, WAIT_SCALE
    ap = argparse.ArgumentParser(description="Messskript fuer Kettler AX1 (Mensch muss treten).")
    ap.add_argument("port", nargs="?", default="COM3", help="COM-Port oder URL (z.B. socket://127.0.0.1:4001), Standard COM3")
    ap.add_argument("--wait-scale", type=float, default=1.0, help="Faktor fuer alle Countdowns (Standard 1.0, 0 = keine Wartezeit)")
    args = ap.parse_args()
    PORT = args.port
    WAIT_SCALE = max(0.0, args.wait_scale)
    name = datetime.now().strftime("kettler_probe-%Y%m%d-%H%M%S.log")
    LOG = open(name, "w", encoding="utf-8")
    T0 = time.monotonic()
    ser = open_port(PORT)
    try:
        log(f"{PORT} geoeffnet, Log: {name}")
        log("\n=== 0. Schnellcheck RS / ID ===")
        rs = send(ser, "RS", wait=1.0)
        dev_id = send(ser, "ID", wait=1.0)
        if not rs and not dev_id:
            log("Keine Antwort vom Ergometer. Ist das Cockpit an? Einmal kurz treten, damit es aufwacht.")
        log("\n=== 1. RS, dann CM bis ACK ===")
        step_cm(ser)
        step_pw_grid(ser)
        step_pause(ser)
        ser = step_reconnect(ser)
        step_units(ser)
    finally:
        ser.close()
        log(f"\nFertig, Log in {name}")
        LOG.close()


if __name__ == "__main__":
    main()

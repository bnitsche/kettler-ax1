"""Messskript fuer das Tastenfeld am Y-Kabel des Kettler AX1.

Annahme: Das Tastenfeld haengt am selben RS232-Stecker wie das Cockpit und meldet die vier Tasten
nicht als Daten, sondern ueber die Steuerleitungen CTS, DSR, RI und DCD (der alte Weg fuer einfache
Tasten am seriellen Port; DTR/RTS liefern dabei oft die Spannung).
Ablauf (kein Enter noetig, Treten NICHT noetig):
  Je Taste ein Zeitfenster; dabei die Taste mehrmals kurz druecken und einmal lang halten.
  Danach dieselbe Taste mit DTR aus bzw. RTS aus, um die Spannungsquelle zu finden.
  Die Steuerleitungen werden etwa 50x pro Sekunde gelesen, jede Aenderung erscheint sofort.
Aufruf: python tools/kettler_keypad_probe.py [PORT]   (Standard COM3, nur direkt, nicht ueber die Bridge)
Die Ausgabe wird zusaetzlich in kettler_keypad-JJJJMMTT-HHMMSS.log geschrieben.
"""
import argparse
import sys
import time
import traceback
from datetime import datetime

import serial

KEYS = ["HOCH", "RUNTER", "LINKS", "RECHTS"]
PHASES = [(f"Taste {key} mehrmals kurz druecken, dann einmal 2 s halten", 12, True, True) for key in KEYS] + [
    ("Taste HOCH mehrmals druecken (DTR aus, RTS an)", 8, False, True),
    ("Taste HOCH mehrmals druecken (DTR an, RTS aus)", 8, True, False),
    ("Taste HOCH mehrmals druecken (DTR aus, RTS aus)", 8, False, False),
]
SAMPLE_INTERVAL = 0.02

LOG = None
T0 = time.monotonic()


def log(line=""):
    stamped = f"[{time.monotonic() - T0:7.1f}s] {line}" if line else ""
    print(stamped)
    if LOG:
        LOG.write(stamped + "\n")
        LOG.flush()


def lines(ser):
    return {"CTS": ser.cts, "DSR": ser.dsr, "RI": ser.ri, "DCD": ser.cd}


def fmt(state):
    return "  ".join(f"{k}={'AN ' if v else 'aus'}" for k, v in state.items())


def countdown(instruction, seconds, step):
    log("")
    log("#" * 70)
    log(f"##  {step}")
    log(f"##  GLEICH:  {instruction}")
    log("#" * 70)
    sys.stdout.write(f"noch {seconds} s ... ")
    sys.stdout.flush()
    for remaining in range(seconds - 1, 0, -1):
        time.sleep(1.0)
        sys.stdout.write(f"{remaining} ... ")
        sys.stdout.flush()
    time.sleep(1.0)
    print()
    log(f">>>>>>>>>>  JETZT: {instruction}  <<<<<<<<<<")


def watch(ser, seconds):
    """Steuerleitungen und Daten mitschreiben; liefert (Zahl der Leitungswechsel je Leitung, empfangene Bytes)."""
    state = lines(ser)
    log(f"   Ausgangslage: {fmt(state)}")
    flips = {k: 0 for k in state}
    received = b""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        now = lines(ser)
        if now != state:
            changed = [k for k in now if now[k] != state[k]]
            for k in changed:
                flips[k] += 1
            log(f"   *** {', '.join(changed)} geaendert -> {fmt(now)}")
            state = now
        if ser.in_waiting:
            chunk = ser.read(ser.in_waiting)
            received += chunk
            log(f"   !!! Daten empfangen: {chunk!r}")
        time.sleep(SAMPLE_INTERVAL)
    return flips, received


def run(ser, results):
    for n, (instruction, seconds, dtr, rts) in enumerate(PHASES, 1):
        ser.dtr = dtr
        ser.rts = rts
        countdown(instruction, 5, f"Schritt {n} von {len(PHASES)}")
        flips, received = watch(ser, seconds)
        seen = ", ".join(f"{k} {c}x" for k, c in flips.items() if c) or "keine Leitung"
        outcome = f"{seen}; {len(received)} Datenbytes"
        log(f"--- Ergebnis: {outcome}")
        results.append((instruction, outcome))


def main():
    global LOG, T0
    ap = argparse.ArgumentParser(description="Tastenfeld am Y-Kabel des Kettler AX1 vermessen.")
    ap.add_argument("port", nargs="?", default="COM3", help="COM-Port, Standard COM3")
    args = ap.parse_args()
    name = datetime.now().strftime("kettler_keypad-%Y%m%d-%H%M%S.log")
    LOG = open(name, "w", encoding="utf-8")
    T0 = time.monotonic()
    if "://" in args.port:
        ser = serial.serial_for_url(args.port, baudrate=9600, timeout=0)
    else:
        ser = serial.Serial(args.port, 9600, bytesize=8, parity="N", stopbits=1, timeout=0)
    results = []
    try:
        log(f"{args.port} geoeffnet, Log: {name}")
        run(ser, results)
    except KeyboardInterrupt:
        log("ABGEBROCHEN mit Strg+C")
    except Exception:
        log("FEHLER:\n" + traceback.format_exc())
    finally:
        ser.dtr = True
        ser.rts = True
        log("")
        log("=== ZUSAMMENFASSUNG ===")
        for what, outcome in results:
            log(f"  {what:<58} {outcome}")
        ser.close()
        log(f"\nFertig, Log in {name}")
        LOG.close()


if __name__ == "__main__":
    main()

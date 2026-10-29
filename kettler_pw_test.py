"""PW-Test fuer Kettler AX1: prueft, mit welchem Befehl die Zielleistung uebernommen wird.

Waehrend des Tests (ca. 1 Minute) gleichmaessig treten.
Aufruf: python kettler_pw_test.py [PORT]   (Standard: COM3)
Die Ausgabe wird zusaetzlich in kettler_pw_test.log geschrieben.
"""
import sys
import time

import serial

PORT = sys.argv[1] if len(sys.argv) > 1 else "COM3"
LOG = open("kettler_pw_test.log", "w", encoding="utf-8")

# (Beschreibung, Befehle vor dem PW, PW-Befehl, erwartete Soll-Watt)
VARIANTS = [
    ("PW mit Leerzeichen", [], "PW 50", 50),
    ("PW dreistellig", [], "PW 060", 60),
    ("PW ohne Leerzeichen", [], "PW70", 70),
    ("CM erneut, dann PW", ["CM"], "PW 80", 80),
]
ST_PER_VARIANT = 8


def log(line):
    print(line)
    LOG.write(line + "\n")
    LOG.flush()


def send(ser, cmd, wait=0.5):
    ser.reset_input_buffer()
    ser.write((cmd + "\r\n").encode("ascii"))
    time.sleep(wait)
    raw = ser.read(ser.in_waiting or 1)
    text = raw.decode("ascii", errors="replace").strip()
    log(f"> {cmd:<8} < {text!r}")
    return text


def soll_watt(status):
    parts = status.split("\t")
    return int(parts[4]) if len(parts) == 8 and parts[4].isdigit() else None


def main():
    with serial.Serial(PORT, 9600, bytesize=8, parity="N", stopbits=1, timeout=1) as ser:
        log(f"{PORT} geoeffnet")
        send(ser, "RS", wait=2.0)
        send(ser, "CM", wait=2.0)  # laenger warten: kommt doch noch eine Bestaetigung?
        send(ser, "ID")

        input("Jetzt gleichmaessig treten und Enter druecken ... ")
        results = []
        for name, pre, pw, expected in VARIANTS:
            log(f"\n--- {name}: {pw} (erwartet {expected} W) ---")
            for cmd in pre:
                send(ser, cmd, wait=1.0)
            send(ser, pw)
            seen = []
            for _ in range(ST_PER_VARIANT):
                seen.append(soll_watt(send(ser, "ST", wait=1.0)))
            results.append((name, pw, expected, seen[-1]))

        send(ser, "PW 25")  # zurueck auf Grundlast
        log("\n=== Ergebnis ===")
        for name, pw, expected, got in results:
            mark = "OK" if got == expected else "nein"
            log(f"{mark:<5} {pw:<8} {name:<22} Soll-Watt danach: {got}")


if __name__ == "__main__":
    main()

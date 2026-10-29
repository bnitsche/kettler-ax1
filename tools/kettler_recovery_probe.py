"""Messskript fuer die Recovery-Taste (Erholungspuls) am Kettler AX1.

Frage: Antwortet das Cockpit waehrend der Erholungsmessung noch auf ST, und ist es danach noch im PC-Modus?
Ablauf (kein Enter noetig, Anweisungen mit Countdown):
  1. Treten, CM bis ACK, PW 100, 15 s normale Werte.
  2. Recovery-Taste druecken; 150 s lang jede Sekunde ST mit Rohantwort protokollieren.
  3. Weitertreten; pruefen, ob PW 80 ohne neues CM wirkt, sonst CM wiederholen.
Aufruf: python tools/kettler_recovery_probe.py [PORT_ODER_URL]   (Standard COM3)
Die Ausgabe wird zusaetzlich in kettler_recovery-JJJJMMTT-HHMMSS.log geschrieben.
"""
import argparse
import sys
import time
from datetime import datetime

import serial

ST_FIELDS = ["Puls", "U/min", "km/h", "Distanz", "Soll-Watt", "Energie", "Zeit", "Ist-Watt"]
CM_ATTEMPTS = 30
RECOVERY_SECONDS = 150

LOG = None
T0 = time.monotonic()


def log(line=""):
    stamped = f"[{time.monotonic() - T0:7.1f}s] {line}" if line else ""
    print(stamped)
    if LOG:
        LOG.write(stamped + "\n")
        LOG.flush()


def countdown(instruction, seconds):
    log("")
    log("*" * 60)
    log(f"*** JETZT: {instruction} ***")
    log("*" * 60)
    sys.stdout.write(f"noch {seconds} s ... ")
    sys.stdout.flush()
    for remaining in range(seconds - 1, 0, -1):
        time.sleep(1.0)
        sys.stdout.write(f"{remaining} ... ")
        sys.stdout.flush()
    time.sleep(1.0)
    print("los!")


def open_port(target):
    if "://" in target:
        return serial.serial_for_url(target, baudrate=9600, bytesize=8, parity="N", stopbits=1, timeout=1)
    return serial.Serial(target, 9600, bytesize=8, parity="N", stopbits=1, timeout=1)


def send(ser, cmd, wait=0.3):
    """Befehl senden, Rohantwort (auch leer oder unerwartet) protokollieren und als Text liefern."""
    pending = ser.read(ser.in_waiting) if ser.in_waiting else b""
    if pending:
        log(f"  (unaufgefordert empfangen: {pending!r})")
    ser.write((cmd + "\r\n").encode("ascii"))
    time.sleep(wait)
    raw = ser.read_until(b"\n")
    if ser.in_waiting:
        raw += ser.read(ser.in_waiting)
    text = raw.decode("ascii", errors="replace").strip()
    log(f"> {cmd:<6} < {raw!r}")
    return text


def parse_st(text):
    parts = text.split("\t")
    return dict(zip(ST_FIELDS, parts)) if len(parts) == len(ST_FIELDS) else None


def poll(ser, seconds, label):
    """Jede Sekunde ST senden; liefert (gueltig, ungueltig, letzte gueltige Werte)."""
    log(f"\n=== {label}: {seconds} s ST-Abfrage ===")
    ok = bad = 0
    last = None
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        started = time.monotonic()
        st = parse_st(send(ser, "ST"))
        if st:
            ok += 1
            last = st
            log("   " + ", ".join(f"{k}={v}" for k, v in st.items()))
        else:
            bad += 1
        time.sleep(max(0.0, 1.0 - (time.monotonic() - started)))
    log(f"--- {label}: {ok} gueltige, {bad} fehlende/ungueltige Antworten")
    return ok, bad, last


def cm_until_ack(ser):
    for attempt in range(1, CM_ATTEMPTS + 1):
        if "ACK" in send(ser, "CM", wait=2.0):
            log(f"CM bestaetigt nach {attempt} Versuch(en)")
            return True
        log("   noch kein ACK, bitte treten ...")
    log(f"FEHLER: kein ACK auf CM nach {CM_ATTEMPTS} Versuchen")
    return False


def soll(st):
    return int(st["Soll-Watt"]) if st and st["Soll-Watt"].isdigit() else None


def main():
    global LOG, T0
    ap = argparse.ArgumentParser(description="Recovery-Taste am Kettler AX1 vermessen.")
    ap.add_argument("port", nargs="?", default="COM3", help="COM-Port oder URL, Standard COM3")
    args = ap.parse_args()
    name = datetime.now().strftime("kettler_recovery-%Y%m%d-%H%M%S.log")
    LOG = open(name, "w", encoding="utf-8")
    T0 = time.monotonic()
    ser = open_port(args.port)
    try:
        log(f"{args.port} geoeffnet, Log: {name}")
        countdown("Treten anfangen und gleichmaessig weitertreten", 10)
        if not cm_until_ack(ser):
            sys.exit(1)
        send(ser, "PW 100")
        poll(ser, 15, "Vorher (normal treten, PW 100)")

        countdown("RECOVERY-TASTE DRUECKEN, dann aufhoeren zu treten und Messung abwarten", 5)
        poll(ser, RECOVERY_SECONDS, "Recovery-Messung")

        countdown("Wieder treten (Fitnessnote ggf. am Cockpit wegdruecken)", 10)
        _, _, last = poll(ser, 10, "Nachher")
        log(f"Soll-Watt nachher: {soll(last)} (vorher 100)")
        send(ser, "PW 80")
        _, _, last = poll(ser, 5, "Nachher PW 80 ohne CM")
        if soll(last) == 80:
            log("ERGEBNIS: PC-Modus nach Recovery noch aktiv, PW wirkt ohne CM")
        else:
            log("ERGEBNIS: PW 80 ohne CM wirkt NICHT, versuche CM erneut")
            if cm_until_ack(ser):
                send(ser, "PW 80")
                _, _, last = poll(ser, 5, "Nachher PW 80 nach CM")
                log(f"ERGEBNIS: nach erneutem CM Soll-Watt = {soll(last)}")
        send(ser, "PW 25")
    finally:
        ser.close()
        log(f"\nFertig, Log in {name}")
        LOG.close()


if __name__ == "__main__":
    main()

"""Messskript fuer die vier Richtungstasten am Kettler AX1.

Frage: Kommt ein Druck auf HOCH, RUNTER, LINKS oder RECHTS ueber die Schnittstelle an, als
geaendertes ST-Feld oder als zusaetzliche, unaufgeforderte Ausgabe?
Ablauf (kein Enter noetig, Anweisungen mit Countdown, die ganze Zeit gleichmaessig treten):
  1. CM bis ACK, PW 100, 10 s Grundwerte.
  2. 10 s nur zuhoeren (kein ST), dabei HOCH druecken: sendet das Cockpit von selbst etwas?
  3. Je Taste: einmal kurz, zweimal kurz, 3 s lang halten. Waehrenddessen ST etwa 3x pro Sekunde.
     Nach jeder Taste wird PW 100 wieder gesetzt.
Aufruf: python tools/kettler_keys_probe.py [PORT_ODER_URL]   (Standard COM3)
Auf dem Bildschirm stehen nur die Anweisungen und Auffaelligkeiten; alle Rohdaten stehen im Log
kettler_keys-JJJJMMTT-HHMMSS.log (im Ordner, aus dem das Skript gestartet wird).
"""
import argparse
import sys
import time
import traceback
from datetime import datetime

import serial

ST_FIELDS = ["Puls", "U/min", "km/h", "Distanz", "Soll-Watt", "Energie", "Zeit", "Ist-Watt"]
# Felder, die sich beim Treten ohnehin laufend aendern; Aenderungen der uebrigen werden hervorgehoben.
MOVING = {"Puls", "U/min", "km/h", "Distanz", "Energie", "Zeit", "Ist-Watt"}
KEYS = ["HOCH", "RUNTER", "LINKS", "RECHTS"]
PRESSES = [("einmal kurz druecken", 8), ("zweimal kurz druecken", 8), ("3 Sekunden lang gedrueckt halten", 10)]
STEPS = 1 + len(KEYS) * len(PRESSES)
CM_ATTEMPTS = 30
POLL_INTERVAL = 0.33

LOG = None
T0 = time.monotonic()


def log(line="", show=True):
    """Ins Log schreiben; mit `show` auch auf den Bildschirm."""
    stamped = f"[{time.monotonic() - T0:7.1f}s] {line}" if line else ""
    if show:
        print(stamped)
    if LOG:
        LOG.write(stamped + "\n")
        LOG.flush()


def countdown(instruction, seconds, step):
    log("")
    log("")
    log("#" * 70)
    log(f"##  {step}")
    log(f"##  GLEICH:  {instruction}")
    log("##  (dabei weiter gleichmaessig treten)")
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


def open_port(target):
    if "://" in target:
        return serial.serial_for_url(target, baudrate=9600, bytesize=8, parity="N", stopbits=1, timeout=0.3)
    return serial.Serial(target, 9600, bytesize=8, parity="N", stopbits=1, timeout=0.3)


def send(ser, cmd, wait=0.15):
    """Befehl senden, Rohantwort (auch leer oder unerwartet) ins Log schreiben und als Text liefern."""
    pending = ser.read(ser.in_waiting) if ser.in_waiting else b""
    if pending:
        log(f"  !!! unaufgefordert empfangen: {pending!r}")
    ser.write((cmd + "\r\n").encode("ascii"))
    time.sleep(wait)
    raw = ser.read_until(b"\n")
    if ser.in_waiting:
        raw += ser.read(ser.in_waiting)
    text = raw.decode("ascii", errors="replace").strip()
    log(f"> {cmd:<6} < {raw!r}", show=False)
    return text


def parse_st(text):
    parts = text.split("\t")
    return dict(zip(ST_FIELDS, parts)) if len(parts) == len(ST_FIELDS) else None


def poll(ser, seconds, label, before=None):
    """ST etwa 3x pro Sekunde; meldet Zeilen, die keine Statuszeile sind, und Aenderungen ruhiger Felder."""
    log(f"=== {label}: {seconds} s ST-Abfrage ===", show=False)
    ok = bad = 0
    last = before
    changes = []
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        started = time.monotonic()
        text = send(ser, "ST")
        st = parse_st(text)
        if st:
            ok += 1
            if last:
                for k in ST_FIELDS:
                    if st[k] != last[k] and k not in MOVING:
                        changes.append(f"{k}: {last[k]} -> {st[k]}")
                        log(f"  *** {k} geaendert: {last[k]} -> {st[k]}")
                if (":" in last["Zeit"]) != (":" in st["Zeit"]):
                    changes.append(f"Zeitformat: {last['Zeit']} -> {st['Zeit']}")
                    log(f"  *** Zeitformat geaendert: {last['Zeit']} -> {st['Zeit']}")
            last = st
            log("   " + ", ".join(f"{k}={v}" for k, v in st.items()), show=False)
        else:
            bad += 1
            log(f"  !!! keine gueltige Statuszeile: {text!r}")
        time.sleep(max(0.0, POLL_INTERVAL - (time.monotonic() - started)))
    rpm = last["U/min"] if last else "?"
    log(f"--- {label}: {ok} gueltige, {bad} ungueltige Antworten, U/min zuletzt {rpm}; "
        f"Aenderungen: {', '.join(changes) if changes else 'keine'}")
    return last, changes, bad


def listen(ser, seconds, label):
    """Nichts senden, nur mitschreiben, was das Cockpit von selbst schickt."""
    log(f"=== {label}: {seconds} s nur zuhoeren ===", show=False)
    received = b""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        chunk = ser.read(ser.in_waiting or 1)
        if chunk:
            received += chunk
            log(f"  !!! empfangen: {chunk!r}")
    log(f"--- {label}: {len(received)} Bytes unaufgefordert empfangen")
    return received


def cm_until_ack(ser):
    for attempt in range(1, CM_ATTEMPTS + 1):
        if "ACK" in send(ser, "CM", wait=2.0):
            log(f"CM bestaetigt nach {attempt} Versuch(en)")
            return True
        log("   noch kein ACK, bitte treten ...")
    log(f"FEHLER: kein ACK auf CM nach {CM_ATTEMPTS} Versuchen")
    return False


def run(ser, results):
    countdown("Treten anfangen und die ganze Zeit gleichmaessig weitertreten", 10, "Start")
    if not cm_until_ack(ser):
        sys.exit(1)
    send(ser, "PW 100")
    last, _, _ = poll(ser, 10, "Grundwerte (PW 100, keine Taste)")

    countdown("Taste HOCH einmal kurz druecken (es wird nur zugehoert)", 5, f"Schritt 1 von {STEPS}")
    unsolicited = listen(ser, 10, "HOCH ohne ST-Abfrage")
    results.append(("HOCH ohne Abfrage", f"{len(unsolicited)} Bytes unaufgefordert"))
    send(ser, "PW 100")
    last, _, _ = poll(ser, 4, "Zurueck auf PW 100")

    step = 1
    for key in KEYS:
        for how, seconds in PRESSES:
            step += 1
            countdown(f"Taste {key} {how}", 5, f"Schritt {step} von {STEPS}")
            last, changes, bad = poll(ser, seconds, f"{key}, {how}", last)
            rpm = last["U/min"] if last else "?"
            results.append((f"{key}, {how}", (", ".join(changes) or "keine Aenderung")
                            + f" (U/min {rpm})" + (f", {bad} ungueltige Antworten" if bad else "")))
        send(ser, "PW 100")
        last, _, _ = poll(ser, 4, f"Nach {key}: zurueck auf PW 100", last)
    send(ser, "PW 25")


def main():
    global LOG, T0
    ap = argparse.ArgumentParser(description="Richtungstasten am Kettler AX1 vermessen.")
    ap.add_argument("port", nargs="?", default="COM3", help="COM-Port oder URL, Standard COM3")
    args = ap.parse_args()
    name = datetime.now().strftime("kettler_keys-%Y%m%d-%H%M%S.log")
    LOG = open(name, "w", encoding="utf-8")
    T0 = time.monotonic()
    ser = open_port(args.port)
    results = []
    try:
        log(f"{args.port} geoeffnet, Log: {name}")
        run(ser, results)
    except KeyboardInterrupt:
        log("ABGEBROCHEN mit Strg+C")
    except Exception:
        log("FEHLER:\n" + traceback.format_exc())
    finally:
        log("")
        log("=== ZUSAMMENFASSUNG ===")
        for what, outcome in results:
            log(f"  {what:<45} {outcome}")
        ser.close()
        log(f"\nFertig, Log in {name}")
        LOG.close()


if __name__ == "__main__":
    main()

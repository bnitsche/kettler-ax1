"""Verbindungstest fuer Kettler-Ergometer (Seriellprotokoll, 9600 8N1, CRLF).

Aufruf: python kettler_test.py [PORT]   (Standard: COM3)
"""
import sys
import time

import serial

PORT = sys.argv[1] if len(sys.argv) > 1 else "COM3"
ST_FIELDS = ["Puls", "U/min", "km/h", "Distanz", "Soll-Watt", "Energie", "Zeit", "Ist-Watt"]


def send(ser, cmd, wait=0.5):
    ser.reset_input_buffer()
    ser.write((cmd + "\r\n").encode("ascii"))
    time.sleep(wait)
    raw = ser.read(ser.in_waiting or 1)
    text = raw.decode("ascii", errors="replace").strip()
    print(f"> {cmd:<8} < {text!r}  (roh: {raw.hex(' ') or '-'})")
    return text


def main():
    with serial.Serial(PORT, 9600, bytesize=8, parity="N", stopbits=1, timeout=1) as ser:
        print(f"{PORT} geoeffnet, DSR={ser.dsr} CTS={ser.cts} CD={ser.cd}")
        send(ser, "RS", wait=2.0)
        for cmd in ("CM", "ID", "VE", "PW 80"):
            send(ser, cmd)
        for _ in range(10):
            reply = send(ser, "ST", wait=1.0)
            parts = reply.split("\t")
            if len(parts) == len(ST_FIELDS):
                print("   " + ", ".join(f"{k}={v}" for k, v in zip(ST_FIELDS, parts)))


if __name__ == "__main__":
    main()

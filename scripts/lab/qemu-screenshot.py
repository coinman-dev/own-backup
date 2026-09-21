#!/usr/bin/env python3
"""Photograph a running QEMU guest through its monitor socket.

The laboratory VM runs headless, so when a phase does not finish this is the
only way to see what the guest is showing — a setup prompt, an error dialog, or
a desktop that is simply still working. Writes a PNG with no dependencies
beyond the standard library.

    qemu-screenshot.py <monitor.sock> <out.png>
"""

import os
import socket
import struct
import sys
import tempfile
import time
import zlib


def monitor_command(path: str, command: str, timeout: float = 20.0) -> str:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(timeout)
        sock.connect(path)
        time.sleep(0.3)
        try:
            sock.recv(65536)  # banner
        except socket.timeout:
            pass
        sock.sendall((command + "\n").encode())
        time.sleep(1.0)
        out = b""
        try:
            while True:
                chunk = sock.recv(65536)
                if not chunk:
                    break
                out += chunk
                if b"(qemu)" in out:
                    break
        except socket.timeout:
            pass
        return out.decode(errors="replace")


def read_ppm(path: str):
    with open(path, "rb") as handle:
        data = handle.read()
    if not data.startswith(b"P6"):
        raise SystemExit(f"{path} is not a binary PPM")
    fields, at = [], 2
    while len(fields) < 3:
        while at < len(data) and data[at : at + 1].isspace():
            at += 1
        if data[at : at + 1] == b"#":
            while data[at : at + 1] not in (b"\n", b""):
                at += 1
            continue
        start = at
        while at < len(data) and not data[at : at + 1].isspace():
            at += 1
        fields.append(int(data[start:at]))
    at += 1
    width, height, _maxval = fields
    return width, height, data[at : at + width * height * 3]


def write_png(path: str, width: int, height: int, rgb: bytes) -> None:
    raw = b"".join(
        b"\x00" + rgb[row * width * 3 : (row + 1) * width * 3] for row in range(height)
    )

    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 6))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as handle:
        handle.write(png)


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    monitor, out = sys.argv[1], sys.argv[2]
    ppm = os.path.join(tempfile.gettempdir(), "qemu-screen.ppm")
    if os.path.exists(ppm):
        os.unlink(ppm)
    reply = monitor_command(monitor, f"screendump {ppm}")
    for _ in range(40):
        if os.path.exists(ppm) and os.path.getsize(ppm) > 0:
            break
        time.sleep(0.25)
    else:
        raise SystemExit(f"the monitor produced no image: {reply.strip()}")
    width, height, rgb = read_ppm(ppm)
    write_png(out, width, height, rgb)
    os.unlink(ppm)
    print(f"{out} ({width}x{height})")


if __name__ == "__main__":
    main()

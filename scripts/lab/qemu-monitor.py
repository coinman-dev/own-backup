#!/usr/bin/env python3
"""Send one command to a running QEMU guest's monitor socket and print the reply.

Used by the laboratory driver for the two things the guest cannot do for itself:
pressing a key at "Press any key to boot from CD" and resetting a VM that is
stuck. Keeping it separate means the driver never needs an interactive console.

    qemu-monitor.py <monitor.sock> "sendkey ret"
"""

import socket
import sys
import time


def send(path: str, command: str, timeout: float = 10.0) -> str:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(timeout)
        sock.connect(path)
        time.sleep(0.2)
        try:
            sock.recv(65536)  # banner
        except socket.timeout:
            pass
        sock.sendall((command + "\n").encode())
        time.sleep(0.4)
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


def main() -> None:
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    print(send(sys.argv[1], " ".join(sys.argv[2:])).strip())


if __name__ == "__main__":
    main()

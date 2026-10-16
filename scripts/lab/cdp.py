#!/usr/bin/env python3
"""A minimal Chrome DevTools Protocol client on the standard library alone.

Enough to drive a WebView2 page: connect to a target's WebSocket, call a
method, evaluate JavaScript, wait until an expression turns true, take a
screenshot. Text frames only; the client masks what it sends, as RFC 6455
requires, and answers pings.
"""
import base64
import json
import os
import socket
import struct
import time
import urllib.request


def targets(host: str, port: int):
    with urllib.request.urlopen(f'http://{host}:{port}/json', timeout=5) as r:
        return json.loads(r.read().decode())


class Cdp:
    def __init__(self, ws_url: str, timeout: float = 30.0):
        # ws://host:port/devtools/page/<id>
        rest = ws_url.split('://', 1)[1]
        hostport, path = rest.split('/', 1)
        host, port = hostport.rsplit(':', 1)
        self.sock = socket.create_connection((host, int(port)), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((f'GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\n'
                           f'Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n'
                           'Sec-WebSocket-Version: 13\r\n\r\n').encode())
        head = b''
        while b'\r\n\r\n' not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError('DevTools closed during the handshake')
            head += chunk
        status = head.split(b'\r\n', 1)[0]
        if b' 101 ' not in status:
            raise ConnectionError(f'DevTools refused the WebSocket: {status!r}')
        self.buffer = head.split(b'\r\n\r\n', 1)[1]
        self.next_id = 0
        self.events = []

    def _read(self, n: int) -> bytes:
        while len(self.buffer) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError('DevTools connection closed')
            self.buffer += chunk
        data, self.buffer = self.buffer[:n], self.buffer[n:]
        return data

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        header = bytes([0x80 | opcode])
        n = len(payload)
        if n < 126:
            header += bytes([0x80 | n])
        elif n < 65536:
            header += bytes([0x80 | 126]) + struct.pack('>H', n)
        else:
            header += bytes([0x80 | 127]) + struct.pack('>Q', n)
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def _recv_message(self) -> str:
        parts = []
        while True:
            b0, b1 = self._read(2)
            opcode, fin = b0 & 0x0F, b0 & 0x80
            n = b1 & 0x7F
            if n == 126:
                n = struct.unpack('>H', self._read(2))[0]
            elif n == 127:
                n = struct.unpack('>Q', self._read(8))[0]
            if b1 & 0x80:
                mask = self._read(4)
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(self._read(n)))
            else:
                data = self._read(n)
            if opcode == 0x9:  # ping
                self._send_frame(0xA, data)
                continue
            if opcode == 0x8:
                raise ConnectionError('DevTools closed the connection')
            if opcode in (0x1, 0x0):
                parts.append(data)
                if fin:
                    return b''.join(parts).decode('utf-8', errors='replace')

    def call(self, method: str, params=None, timeout: float = 60.0):
        self.next_id += 1
        mine = self.next_id
        self._send_frame(0x1, json.dumps({'id': mine, 'method': method, 'params': params or {}}).encode())
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            msg = json.loads(self._recv_message())
            if msg.get('id') == mine:
                if 'error' in msg:
                    raise RuntimeError(f'{method}: {msg["error"]}')
                return msg.get('result', {})
            self.events.append(msg)
        raise TimeoutError(method)

    def eval(self, expression: str, timeout: float = 60.0):
        """Evaluate in the page; promises are awaited, the value comes back as JSON."""
        result = self.call('Runtime.evaluate', {'expression': expression, 'awaitPromise': True,
                                                'returnByValue': True}, timeout=timeout)
        if 'exceptionDetails' in result:
            raise RuntimeError(f'page threw: {result["exceptionDetails"].get("text")} '
                               f'{result["exceptionDetails"].get("exception", {}).get("description", "")}')
        return result.get('result', {}).get('value')

    def wait(self, expression: str, timeout: float = 120.0, every: float = 0.5):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            last = self.eval(expression)
            if last:
                return last
            time.sleep(every)
        raise TimeoutError(f'not true within {timeout} s: {expression[:120]} (last={last!r})')

    def screenshot(self, path) -> None:
        data = self.call('Page.captureScreenshot', {'format': 'png'})['data']
        with open(path, 'wb') as out:
            out.write(base64.b64decode(data))

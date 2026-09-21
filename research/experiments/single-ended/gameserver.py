#!/usr/bin/env python3
"""Ping-pong echo server for the game scenario: one fixed-size reply per request.

Models an interactive flow, where what matters is per-message round-trip latency
and its tail, not throughput. The reply is larger than the request (a server
pushes world state down to its clients), so the server -- the side being
accelerated -- is the one doing the sending.
"""
import socket
import struct
import sys
import threading

REPLY = 512


def handle(c):
    c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    try:
        while True:
            hdr = b""
            while len(hdr) < 8:
                b = c.recv(8 - len(hdr))
                if not b:
                    return
                hdr += b
            seq = struct.unpack("!Q", hdr)[0]
            c.sendall(struct.pack("!Q", seq) + b"\x00" * (REPLY - 8))
    except OSError:
        pass
    finally:
        c.close()


def main(port):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", port))
    s.listen(128)
    print(f"gameserver on {port}", flush=True)
    while True:
        c, _ = s.accept()
        threading.Thread(target=handle, args=(c,), daemon=True).start()


if __name__ == "__main__":
    main(int(sys.argv[1]) if len(sys.argv) > 1 else 9999)

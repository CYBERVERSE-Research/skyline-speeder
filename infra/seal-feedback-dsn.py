#!/usr/bin/env python3
"""Seal the Sentry DSN that error reports go to, and write it where they read it.

    python3 infra/seal-feedback-dsn.py            # asks for the DSN, input hidden
    ... | python3 infra/seal-feedback-dsn.py      # or reads it from stdin
    python3 infra/seal-feedback-dsn.py --check    # both copies agree and open
    python3 infra/seal-feedback-dsn.py --clear    # no DSN: nothing asks or sends
    python3 infra/seal-feedback-dsn.py --self-test

Two copies, because the two senders cannot share one: install.sh is often
piped in with no tree beside it, so it carries FEEDBACK_SEALED itself, and
ssctl compiles crates/ssctl/src/feedback-dsn.sealed in. This script writes
both, and `cargo test -p ssctl` fails when they disagree.

What sealing is and is not. The DSN is ChaCha20-encrypted (RFC 8439) under
FEEDBACK_SEAL_KEY and FEEDBACK_SEAL_NONCE, which install.sh holds in plain
sight: a program that sends reports has to be able to open it, so anyone who
reads the code can too. It keeps the DSN out of plain text in the repository and
in the binaries -- out of secret scanners, of a grep for the ingest host, of
`strings` -- and nothing more. That is enough for what a DSN is: a key that
can only submit events to one project, not read anything. Abuse is answered
on the Sentry side (rate limits, spike protection) and by rotating the key
and sealing the new one; an auth token never belongs anywhere in this tree.

Nothing of the DSN is printed, on success or on error.
"""

from __future__ import annotations

import argparse
import getpass
import re
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INSTALL_SH = ROOT / "install.sh"
RUST_COPY = ROOT / "crates" / "ssctl" / "src" / "feedback-dsn.sealed"

# The public key, an optional legacy secret, the host (and port), an optional
# path prefix (a self-hosted Sentry under a subpath) and the project id. Only
# https: install.sh and ssctl hand curl --proto =https.
DSN = re.compile(
    r"^https://(?P<key>[0-9a-f]{32})(?::[0-9a-f]{32})?"
    r"@(?P<host>[A-Za-z0-9.-]+(?::[0-9]+)?)(?P<path>(?:/[A-Za-z0-9._-]+)*)"
    r"/(?P<project>[0-9]+)/?$"
)


def quarter_round(x: list[int], a: int, b: int, c: int, d: int) -> None:
    def rotl(v: int, n: int) -> int:
        return ((v << n) | (v >> (32 - n))) & 0xFFFFFFFF

    x[a] = (x[a] + x[b]) & 0xFFFFFFFF
    x[d] = rotl(x[d] ^ x[a], 16)
    x[c] = (x[c] + x[d]) & 0xFFFFFFFF
    x[b] = rotl(x[b] ^ x[c], 12)
    x[a] = (x[a] + x[b]) & 0xFFFFFFFF
    x[d] = rotl(x[d] ^ x[a], 8)
    x[c] = (x[c] + x[d]) & 0xFFFFFFFF
    x[b] = rotl(x[b] ^ x[c], 7)


def chacha20_block(key: bytes, counter: int, nonce: bytes) -> bytes:
    state = [0x61707865, 0x3320646E, 0x79622D32, 0x6B206574]
    state += list(struct.unpack("<8I", key))
    state += [counter]
    state += list(struct.unpack("<3I", nonce))
    x = list(state)
    for _ in range(10):
        quarter_round(x, 0, 4, 8, 12)
        quarter_round(x, 1, 5, 9, 13)
        quarter_round(x, 2, 6, 10, 14)
        quarter_round(x, 3, 7, 11, 15)
        quarter_round(x, 0, 5, 10, 15)
        quarter_round(x, 1, 6, 11, 12)
        quarter_round(x, 2, 7, 8, 13)
        quarter_round(x, 3, 4, 9, 14)
    return struct.pack("<16I", *((x[i] + state[i]) & 0xFFFFFFFF for i in range(16)))


def chacha20(key: bytes, nonce: bytes, data: bytes) -> bytes:
    """Encrypts and decrypts alike; the block counter starts at 1, as in RFC 8439 2.4."""
    out = bytearray()
    for start in range(0, len(data), 64):
        stream = chacha20_block(key, 1 + start // 64, nonce)
        out += bytes(a ^ b for a, b in zip(data[start : start + 64], stream))
    return bytes(out)


def self_test() -> None:
    # RFC 8439, section 2.4.2.
    key = bytes(range(32))
    nonce = bytes.fromhex("000000000000004a00000000")
    plain = (
        b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip"
        b" for the future, sunscreen would be it."
    )
    expected = bytes.fromhex(
        "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b"
        "f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8"
        "07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736"
        "5af90bbf74a35be6b40b8eedf2785e42874d"
    )
    if chacha20(key, nonce, plain) != expected:
        sys.exit("self-test failed: ChaCha20 does not match RFC 8439 2.4.2")
    print("self-test passed: ChaCha20 matches RFC 8439 2.4.2")


def shell_value(name: str) -> str:
    match = re.search(rf"^{name}=([0-9a-f]*)$", INSTALL_SH.read_text(), re.MULTILINE)
    if match is None:
        sys.exit(f"{INSTALL_SH.relative_to(ROOT)} has no {name}=<hex> line")
    return match.group(1)


def cipher() -> tuple[bytes, bytes]:
    key = bytes.fromhex(shell_value("FEEDBACK_SEAL_KEY"))
    nonce = bytes.fromhex(shell_value("FEEDBACK_SEAL_NONCE"))
    if len(key) != 32 or len(nonce) != 12:
        sys.exit("FEEDBACK_SEAL_KEY must be 32 bytes and FEEDBACK_SEAL_NONCE 12")
    return key, nonce


def opens(sealed: str) -> bool:
    key, nonce = cipher()
    try:
        return DSN.match(chacha20(key, nonce, bytes.fromhex(sealed)).decode("ascii")) is not None
    except (ValueError, UnicodeDecodeError):
        return False


def write(sealed: str) -> None:
    text = INSTALL_SH.read_text()
    text, count = re.subn(
        r"^FEEDBACK_SEALED=[0-9a-f]*$", f"FEEDBACK_SEALED={sealed}", text, flags=re.MULTILINE
    )
    if count != 1:
        sys.exit(f"{INSTALL_SH.relative_to(ROOT)} must have exactly one FEEDBACK_SEALED= line")
    INSTALL_SH.write_text(text)
    RUST_COPY.write_text(f"{sealed}\n")


def check() -> None:
    shell = shell_value("FEEDBACK_SEALED")
    rust = RUST_COPY.read_text().strip()
    if shell != rust:
        sys.exit(f"the sealed DSN in install.sh and in {RUST_COPY.relative_to(ROOT)} differ")
    if not shell:
        print("no DSN is sealed: error reports are off (nothing asks, nothing is sent)")
    elif opens(shell):
        print("a DSN is sealed, both copies agree and it opens to a valid https DSN")
    else:
        sys.exit("the sealed DSN does not open to a valid https DSN: seal it again")


def read_dsn() -> str:
    if sys.stdin.isatty():
        return getpass.getpass("Sentry DSN (input hidden): ").strip()
    return sys.stdin.readline().strip()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="verify the two copies; print no DSN")
    mode.add_argument("--clear", action="store_true", help="remove the DSN: reports are off")
    mode.add_argument("--self-test", action="store_true", help="check ChaCha20 against RFC 8439")
    arguments = parser.parse_args()

    if arguments.self_test:
        self_test()
        return
    if arguments.check:
        check()
        return
    if arguments.clear:
        write("")
        print("cleared: error reports are off until a DSN is sealed again")
        return

    self_test()
    match = DSN.match(read_dsn())
    if match is None:
        # The input is not echoed back: it may be a token pasted by mistake.
        sys.exit(
            "not a Sentry DSN of the form https://<32 hex>@<host>[/<path>]/<project id>"
            " (Settings -> Projects -> <project> -> Client Keys (DSN))"
        )
    # The legacy secret half is dropped: event submission needs only the
    # public key.
    public = f"https://{match['key']}@{match['host']}{match['path']}/{match['project']}"
    key, nonce = cipher()
    sealed = chacha20(key, nonce, public.encode("ascii")).hex()
    write(sealed)
    check()
    print(f"wrote it to install.sh and {RUST_COPY.relative_to(ROOT)}; commit both")


if __name__ == "__main__":
    main()

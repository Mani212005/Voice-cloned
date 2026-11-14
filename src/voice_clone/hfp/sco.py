"""Linux SCO sockets for HFP call audio (kernel Bluetooth API, no PipeWire).

Constants come from the kernel headers ``include/net/bluetooth/bluetooth.h`` and
``include/net/bluetooth/sco.h``.
"""

from __future__ import annotations

import socket
import struct

SOL_BLUETOOTH = 274
SOL_SCO = 17
SCO_OPTIONS = 1
BT_DEFER_SETUP = 7
BT_VOICE = 11
BT_VOICE_TRANSPARENT = 0x0003
BT_VOICE_CVSD_16BIT = 0x0060

DEFAULT_MTU = 48


def _address(bdaddr: str) -> bytes:
    return bdaddr.upper().encode("ascii")


def new_socket() -> socket.socket:
    return socket.socket(socket.AF_BLUETOOTH, socket.SOCK_SEQPACKET, socket.BTPROTO_SCO)


def set_voice(sock: socket.socket, transparent: bool) -> None:
    setting = BT_VOICE_TRANSPARENT if transparent else BT_VOICE_CVSD_16BIT
    sock.setsockopt(SOL_BLUETOOTH, BT_VOICE, struct.pack("<H", setting))


def listen(adapter_address: str) -> socket.socket:
    """Listen for SCO connections from the phone, deferring setup so the voice
    setting can be chosen per connection before it is accepted."""
    sock = new_socket()
    try:
        sock.setsockopt(SOL_BLUETOOTH, BT_DEFER_SETUP, struct.pack("<I", 1))
        sock.bind(_address(adapter_address))
        sock.listen(1)
        sock.setblocking(False)
    except OSError:
        sock.close()
        raise
    return sock


def authorize(sock: socket.socket, transparent: bool) -> None:
    """Accept a deferred SCO connection: set the voice mode, then a read authorizes it."""
    set_voice(sock, transparent)
    sock.setblocking(True)
    try:
        sock.recv(1)
    finally:
        sock.setblocking(False)


def mtu(sock: socket.socket) -> int:
    try:
        raw = sock.getsockopt(SOL_SCO, SCO_OPTIONS, 2)
        value = struct.unpack("<H", raw[:2])[0]
    except OSError:
        return DEFAULT_MTU
    return value or DEFAULT_MTU


def connect_socket(adapter_address: str, transparent: bool) -> socket.socket:
    """A bound, non-blocking SCO socket ready for ``loop.sock_connect``."""
    sock = new_socket()
    try:
        sock.bind(_address(adapter_address))
        set_voice(sock, transparent)
        sock.setblocking(False)
    except OSError:
        sock.close()
        raise
    return sock


def peer_address(bdaddr: str) -> bytes:
    return _address(bdaddr)

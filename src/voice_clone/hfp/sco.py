"""Linux SCO sockets for HFP call audio (kernel Bluetooth API, no PipeWire).

Constants come from the kernel headers ``include/net/bluetooth/bluetooth.h`` and
``include/net/bluetooth/sco.h``.

The uv-managed CPython builds are compiled without Bluetooth support, so
``socket.AF_BLUETOOTH`` may not exist and Python cannot parse SCO addresses. The
calls that take an address (socket, bind, listen, accept, connect) therefore go
straight to libc; the resulting fd is wrapped in a normal ``socket.socket`` for
reads, writes and socket options, which work for any address family.
"""

from __future__ import annotations

import asyncio
import ctypes
import errno
import os
import socket
import struct

AF_BLUETOOTH = 31
BTPROTO_SCO = 2
SOL_BLUETOOTH = 274
SOL_SCO = 17
SCO_OPTIONS = 1
BT_DEFER_SETUP = 7
BT_VOICE = 11
BT_VOICE_TRANSPARENT = 0x0003
BT_VOICE_CVSD_16BIT = 0x0060

DEFAULT_MTU = 48
SOCKADDR_SCO_SIZE = 8

_libc = ctypes.CDLL(None, use_errno=True)


def pack_sockaddr(bdaddr: str) -> bytes:
    """``struct sockaddr_sco``: family, then the address in little-endian byte order."""
    raw = bytes.fromhex(bdaddr.replace(":", ""))
    if len(raw) != 6:
        raise ValueError(f"not a Bluetooth address: {bdaddr!r}")
    return struct.pack("<H6s", AF_BLUETOOTH, raw[::-1])


def unpack_sockaddr(data: bytes) -> str:
    _, raw = struct.unpack("<H6s", data[:SOCKADDR_SCO_SIZE])
    return ":".join(f"{b:02X}" for b in raw[::-1])


def _check(result: int) -> int:
    if result < 0:
        err = ctypes.get_errno()
        raise OSError(err, os.strerror(err))
    return result


def new_socket() -> socket.socket:
    fd = _check(
        _libc.socket(AF_BLUETOOTH, socket.SOCK_SEQPACKET | socket.SOCK_CLOEXEC, BTPROTO_SCO)
    )
    return socket.socket(fileno=fd)


def _bind(sock: socket.socket, bdaddr: str) -> None:
    addr = pack_sockaddr(bdaddr)
    _check(_libc.bind(sock.fileno(), addr, len(addr)))


def set_voice(sock: socket.socket, transparent: bool) -> None:
    setting = BT_VOICE_TRANSPARENT if transparent else BT_VOICE_CVSD_16BIT
    sock.setsockopt(SOL_BLUETOOTH, BT_VOICE, struct.pack("<H", setting))


def listen(adapter_address: str) -> socket.socket:
    """Listen for SCO connections from the phone, deferring setup so the voice
    setting can be chosen per connection before it is accepted."""
    sock = new_socket()
    try:
        _bind(sock, adapter_address)  # the kernel accepts BT_DEFER_SETUP only once bound
        sock.setsockopt(SOL_BLUETOOTH, BT_DEFER_SETUP, struct.pack("<I", 1))
        _check(_libc.listen(sock.fileno(), 1))
        sock.setblocking(False)
    except OSError:
        sock.close()
        raise
    return sock


def accept(listener: socket.socket) -> tuple[socket.socket, str]:
    """Accept one pending connection; raises BlockingIOError when there is none."""
    addr = ctypes.create_string_buffer(16)
    length = ctypes.c_uint32(len(addr))
    fd = _libc.accept4(listener.fileno(), addr, ctypes.byref(length), socket.SOCK_CLOEXEC)
    if fd < 0:
        err = ctypes.get_errno()
        if err in (errno.EAGAIN, errno.EWOULDBLOCK):
            raise BlockingIOError(err, os.strerror(err))
        raise OSError(err, os.strerror(err))
    return socket.socket(fileno=fd), unpack_sockaddr(addr.raw)


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


async def connect(adapter_address: str, peer: str, transparent: bool) -> socket.socket:
    """Open SCO from the laptop side to ``peer`` without blocking the event loop."""
    loop = asyncio.get_running_loop()
    sock = new_socket()
    try:
        _bind(sock, adapter_address)
        set_voice(sock, transparent)
        sock.setblocking(False)
        addr = pack_sockaddr(peer)
        if _libc.connect(sock.fileno(), addr, len(addr)) < 0:
            err = ctypes.get_errno()
            if err != errno.EINPROGRESS:
                raise OSError(err, os.strerror(err))
            ready = loop.create_future()
            loop.add_writer(sock.fileno(), lambda: ready.done() or ready.set_result(None))
            try:
                await ready
            finally:
                loop.remove_writer(sock.fileno())
            err = sock.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
            if err:
                raise OSError(err, os.strerror(err))
    except BaseException:
        sock.close()
        raise
    return sock

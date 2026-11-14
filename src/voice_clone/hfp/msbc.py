"""mSBC wideband speech for HFP: H2 framing plus a ctypes binding to the system libsbc.

Over a transparent SCO link each 7.5 ms of 16 kHz audio (120 samples) travels as a
60 byte H2 frame: a 2 byte header (0x01 and one of four sequence bytes), a 57 byte
mSBC frame that starts with the 0xAD sync word, and one padding byte.

The codec needs ``libsbc.so.1`` (Ubuntu package libsbc1). Wideband is optional: the
daemon only offers mSBC to the phone when this library loads.
"""

from __future__ import annotations

import ctypes
import ctypes.util
import logging

log = logging.getLogger(__name__)

H2_SYNC = 0x01
H2_SEQUENCE = (0x08, 0x38, 0xC8, 0xF8)
MSBC_SYNC = 0xAD
MSBC_FRAME_BYTES = 57
H2_FRAME_BYTES = 60
SAMPLES_PER_FRAME = 120
PCM_BYTES_PER_FRAME = SAMPLES_PER_FRAME * 2


def h2_pack(msbc_frame: bytes, sequence: int) -> bytes:
    if len(msbc_frame) != MSBC_FRAME_BYTES:
        raise ValueError(f"mSBC frame must be {MSBC_FRAME_BYTES} bytes, got {len(msbc_frame)}")
    return bytes((H2_SYNC, H2_SEQUENCE[sequence % 4])) + msbc_frame + b"\x00"


class H2Reassembler:
    """Recovers whole mSBC frames from SCO packets of any size."""

    def __init__(self) -> None:
        self._buffer = bytearray()
        self.dropped_bytes = 0

    def feed(self, data: bytes) -> list[bytes]:
        self._buffer.extend(data)
        frames: list[bytes] = []
        while True:
            start = self._find_header()
            if start < 0:
                # Keep a possible partial header at the end.
                keep = 2
                if len(self._buffer) > keep:
                    self.dropped_bytes += len(self._buffer) - keep
                    del self._buffer[:-keep]
                break
            if start:
                self.dropped_bytes += start
                del self._buffer[:start]
            if len(self._buffer) < H2_FRAME_BYTES - 1:
                break  # wait for the rest; the padding byte may be missing on the last frame
            frames.append(bytes(self._buffer[2 : 2 + MSBC_FRAME_BYTES]))
            del self._buffer[: min(H2_FRAME_BYTES, len(self._buffer))]
        return frames

    def _find_header(self) -> int:
        buf = self._buffer
        for i in range(len(buf) - 2):
            if buf[i] == H2_SYNC and buf[i + 1] in H2_SEQUENCE and buf[i + 2] == MSBC_SYNC:
                return i
        return -1


class _SbcStruct(ctypes.Structure):
    _fields_ = (
        ("flags", ctypes.c_ulong),
        ("frequency", ctypes.c_uint8),
        ("blocks", ctypes.c_uint8),
        ("subbands", ctypes.c_uint8),
        ("mode", ctypes.c_uint8),
        ("allocation", ctypes.c_uint8),
        ("bitpool", ctypes.c_uint8),
        ("endian", ctypes.c_uint8),
        ("priv", ctypes.c_void_p),
        ("priv_alloc_base", ctypes.c_void_p),
    )


def _load_libsbc() -> ctypes.CDLL | None:
    for name in ("libsbc.so.1", ctypes.util.find_library("sbc")):
        if not name:
            continue
        try:
            lib = ctypes.CDLL(name)
        except OSError:
            continue
        lib.sbc_init_msbc.argtypes = (ctypes.POINTER(_SbcStruct), ctypes.c_ulong)
        lib.sbc_init_msbc.restype = ctypes.c_int
        lib.sbc_encode.argtypes = (
            ctypes.POINTER(_SbcStruct),
            ctypes.c_void_p,
            ctypes.c_size_t,
            ctypes.c_void_p,
            ctypes.c_size_t,
            ctypes.POINTER(ctypes.c_ssize_t),
        )
        lib.sbc_encode.restype = ctypes.c_ssize_t
        lib.sbc_decode.argtypes = (
            ctypes.POINTER(_SbcStruct),
            ctypes.c_void_p,
            ctypes.c_size_t,
            ctypes.c_void_p,
            ctypes.c_size_t,
            ctypes.POINTER(ctypes.c_size_t),
        )
        lib.sbc_decode.restype = ctypes.c_ssize_t
        lib.sbc_finish.argtypes = (ctypes.POINTER(_SbcStruct),)
        lib.sbc_finish.restype = None
        return lib
    return None


_LIB: ctypes.CDLL | None = None
_LIB_LOADED = False


def libsbc() -> ctypes.CDLL | None:
    global _LIB, _LIB_LOADED
    if not _LIB_LOADED:
        _LIB = _load_libsbc()
        _LIB_LOADED = True
    return _LIB


def available() -> bool:
    return libsbc() is not None


class _Codec:
    def __init__(self) -> None:
        lib = libsbc()
        if lib is None:
            raise RuntimeError("libsbc.so.1 is not installed; wideband (mSBC) is unavailable")
        self._lib = lib
        self._sbc = _SbcStruct()
        if lib.sbc_init_msbc(ctypes.byref(self._sbc), 0) != 0:
            raise RuntimeError("sbc_init_msbc failed")

    def close(self) -> None:
        if self._sbc is not None:
            self._lib.sbc_finish(ctypes.byref(self._sbc))
            self._sbc = None  # type: ignore[assignment]

    def __del__(self) -> None:
        self.close()


class MsbcEncoder(_Codec):
    def __init__(self) -> None:
        super().__init__()
        self._sequence = 0

    def encode(self, pcm: bytes) -> bytes:
        """Encode exactly 120 samples of 16 kHz PCM into one 60 byte H2 frame."""
        if len(pcm) != PCM_BYTES_PER_FRAME:
            raise ValueError(f"need {PCM_BYTES_PER_FRAME} PCM bytes, got {len(pcm)}")
        src = ctypes.create_string_buffer(pcm, len(pcm))
        out = ctypes.create_string_buffer(MSBC_FRAME_BYTES)
        written = ctypes.c_ssize_t(0)
        used = self._lib.sbc_encode(
            ctypes.byref(self._sbc), src, len(pcm), out, MSBC_FRAME_BYTES, ctypes.byref(written)
        )
        if used != len(pcm) or written.value != MSBC_FRAME_BYTES:
            raise RuntimeError(f"sbc_encode failed ({used}, {written.value})")
        frame = h2_pack(out.raw, self._sequence)
        self._sequence += 1
        return frame


class MsbcDecoder(_Codec):
    def decode(self, msbc_frame: bytes) -> bytes:
        """Decode one 57 byte mSBC frame; a corrupt frame becomes 7.5 ms of silence."""
        src = ctypes.create_string_buffer(msbc_frame, len(msbc_frame))
        out = ctypes.create_string_buffer(PCM_BYTES_PER_FRAME)
        written = ctypes.c_size_t(0)
        used = self._lib.sbc_decode(
            ctypes.byref(self._sbc),
            src,
            len(msbc_frame),
            out,
            PCM_BYTES_PER_FRAME,
            ctypes.byref(written),
        )
        if used <= 0 or written.value != PCM_BYTES_PER_FRAME:
            return bytes(PCM_BYTES_PER_FRAME)
        return out.raw

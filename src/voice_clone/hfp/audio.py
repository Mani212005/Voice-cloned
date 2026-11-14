"""PCM helpers: WAV in and out, resampling to the SCO rate, SCO packet pacing.

All call audio is 16-bit signed little-endian mono PCM at 8 kHz (CVSD) or 16 kHz
(mSBC). The controller does CVSD itself, so for narrowband the SCO socket carries
plain PCM.
"""

from __future__ import annotations

import io
import math
import wave
from pathlib import Path

import numpy as np
import soxr

NARROWBAND_RATE = 8000
WIDEBAND_RATE = 16000
SAMPLE_WIDTH = 2


def to_mono_int16(samples: np.ndarray) -> np.ndarray:
    """Mix any channel layout down to mono int16."""
    if samples.ndim == 2:
        samples = samples.astype(np.float64).mean(axis=1)
    if samples.dtype != np.int16:
        samples = np.clip(np.round(samples), -32768, 32767).astype(np.int16)
    return samples


def resample(samples: np.ndarray, src_rate: int, dst_rate: int) -> np.ndarray:
    """High quality resampling of mono int16 PCM."""
    if src_rate == dst_rate:
        return samples.astype(np.int16, copy=False)
    out = soxr.resample(samples.astype(np.float32) / 32768.0, src_rate, dst_rate, quality="HQ")
    return np.clip(np.round(out * 32768.0), -32768, 32767).astype(np.int16)


def read_wav(path: Path, rate: int) -> bytes:
    """Load a PCM WAV file as mono int16 at ``rate``, ready for the SCO link."""
    with wave.open(str(path), "rb") as wav:
        width = wav.getsampwidth()
        channels = wav.getnchannels()
        src_rate = wav.getframerate()
        raw = wav.readframes(wav.getnframes())
    if width == 2:
        samples = np.frombuffer(raw, dtype="<i2")
    elif width == 1:
        samples = (np.frombuffer(raw, dtype=np.uint8).astype(np.int16) - 128) * 256
    elif width == 4:
        samples = (np.frombuffer(raw, dtype="<i4") >> 16).astype(np.int16)
    else:
        raise ValueError(f"{path}: unsupported sample width {width}")
    if channels > 1:
        samples = samples.reshape(-1, channels)
    mono = to_mono_int16(samples)
    return resample(mono, src_rate, rate).astype("<i2").tobytes()


def wav_bytes(pcm: bytes, rate: int) -> bytes:
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(SAMPLE_WIDTH)
        wav.setframerate(rate)
        wav.writeframes(pcm)
    return buffer.getvalue()


def tone(freq: float, seconds: float, rate: int, amplitude: float = 0.3) -> bytes:
    """A sine tone with 10 ms fades, as int16 LE PCM."""
    count = round(seconds * rate)
    t = np.arange(count) / rate
    wave_ = np.sin(2 * math.pi * freq * t) * amplitude
    fade = min(count // 2, round(0.01 * rate))
    if fade:
        ramp = np.linspace(0.0, 1.0, fade)
        wave_[:fade] *= ramp
        wave_[-fade:] *= ramp[::-1]
    return np.round(wave_ * 32767).astype("<i2").tobytes()


def silence(seconds: float, rate: int) -> bytes:
    return bytes(round(seconds * rate) * SAMPLE_WIDTH)


def placeholder_intro(rate: int = NARROWBAND_RATE) -> bytes:
    """Stand-in for the cloned-voice intro: three rising tones, not anyone's voice."""
    return b"".join(
        [
            tone(440, 0.25, rate),
            silence(0.1, rate),
            tone(554, 0.25, rate),
            silence(0.1, rate),
            tone(659, 0.4, rate),
            silence(0.3, rate),
        ]
    )


def placeholder_beep(rate: int = NARROWBAND_RATE) -> bytes:
    """Short beep that tells the caller to start speaking."""
    return tone(1000, 0.3, rate) + silence(0.1, rate)


class WavSink:
    """Streams received call audio to a WAV file as it arrives."""

    def __init__(self, path: Path, rate: int) -> None:
        self.path = path
        self.rate = rate
        self._wav = wave.open(str(path), "wb")  # noqa: SIM115 - closed in close()
        self._wav.setnchannels(1)
        self._wav.setsampwidth(SAMPLE_WIDTH)
        self._wav.setframerate(rate)
        self.frames = 0

    def write(self, pcm: bytes, rate: int) -> None:
        if rate != self.rate:
            raise ValueError(f"sink is {self.rate} Hz, got {rate} Hz audio")
        self._wav.writeframes(pcm)
        self.frames += len(pcm) // SAMPLE_WIDTH

    def close(self) -> None:
        self._wav.close()


class PcmSource:
    """Plays queued PCM clips one after another, then silence."""

    def __init__(self) -> None:
        self._buffer = bytearray()

    def queue(self, pcm: bytes) -> None:
        self._buffer.extend(pcm)

    @property
    def pending_bytes(self) -> int:
        return len(self._buffer)

    def read(self, size: int) -> bytes:
        chunk = bytes(self._buffer[:size])
        del self._buffer[:size]
        if len(chunk) < size:
            chunk += bytes(size - len(chunk))
        return chunk


def split_packets(data: bytes, mtu: int) -> list[bytes]:
    """Cut a byte stream into SCO packets of exactly ``mtu`` bytes, zero padding the last."""
    if mtu <= 0:
        raise ValueError("mtu must be positive")
    packets = [data[i : i + mtu] for i in range(0, len(data), mtu)]
    if packets and len(packets[-1]) < mtu:
        packets[-1] = packets[-1] + bytes(mtu - len(packets[-1]))
    return packets

"""Injectable time source so the protocol logic never reads real time directly."""

from __future__ import annotations

import time
from typing import Protocol


class Clock(Protocol):
    def monotonic(self) -> float:
        """Seconds on a clock that never jumps, for timers."""
        ...

    def wall(self) -> float:
        """Unix time in seconds, for records shown to a person."""
        ...


class SystemClock:
    def monotonic(self) -> float:
        return time.monotonic()

    def wall(self) -> float:
        return time.time()


class FakeClock:
    """Manually advanced clock for tests."""

    def __init__(self, start: float = 1000.0, wall_start: float = 1_800_000_000.0) -> None:
        self._now = start
        self._wall_offset = wall_start - start

    def monotonic(self) -> float:
        return self._now

    def wall(self) -> float:
        return self._now + self._wall_offset

    def advance(self, seconds: float) -> None:
        if seconds < 0:
            raise ValueError("time cannot go backwards")
        self._now += seconds

"""AT line framing and parsing for the Hands-Free side of HFP.

The Audio Gateway (the phone) sends result codes framed as ``\\r\\n<text>\\r\\n``.
This module turns a raw RFCOMM byte stream into typed results and builds the
commands the Hands-Free unit sends. It does no I/O.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

# Hands-Free supported features, sent in AT+BRSF (HFP 1.7, section 4.34.1).
HF_FEATURE_EC_NR = 1 << 0
HF_FEATURE_THREE_WAY = 1 << 1
HF_FEATURE_CLI = 1 << 2
HF_FEATURE_VOICE_RECOGNITION = 1 << 3
HF_FEATURE_REMOTE_VOLUME = 1 << 4
HF_FEATURE_ENHANCED_CALL_STATUS = 1 << 5
HF_FEATURE_ENHANCED_CALL_CONTROL = 1 << 6
HF_FEATURE_CODEC_NEGOTIATION = 1 << 7
HF_FEATURE_HF_INDICATORS = 1 << 8
HF_FEATURE_ESCO_S4 = 1 << 9

# Audio Gateway supported features, received in +BRSF.
AG_FEATURE_THREE_WAY = 1 << 0
AG_FEATURE_EC_NR = 1 << 1
AG_FEATURE_VOICE_RECOGNITION = 1 << 2
AG_FEATURE_IN_BAND_RING = 1 << 3
AG_FEATURE_VOICE_TAG = 1 << 4
AG_FEATURE_REJECT_CALL = 1 << 5
AG_FEATURE_ENHANCED_CALL_STATUS = 1 << 6
AG_FEATURE_ENHANCED_CALL_CONTROL = 1 << 7
AG_FEATURE_EXTENDED_ERROR = 1 << 8
AG_FEATURE_CODEC_NEGOTIATION = 1 << 9
AG_FEATURE_HF_INDICATORS = 1 << 10
AG_FEATURE_ESCO_S4 = 1 << 11

CODEC_CVSD = 1
CODEC_MSBC = 2

MAX_LINE = 1024


@dataclass(frozen=True)
class Ok:
    pass


@dataclass(frozen=True)
class Error:
    text: str


@dataclass(frozen=True)
class Ring:
    pass


@dataclass(frozen=True)
class Clip:
    number: str
    number_type: int | None


@dataclass(frozen=True)
class Ciev:
    index: int
    value: int


@dataclass(frozen=True)
class Brsf:
    features: int


@dataclass(frozen=True)
class CindTest:
    """Answer to ``AT+CIND=?``: indicator names in AG order (1-based in +CIEV)."""

    names: tuple[str, ...]


@dataclass(frozen=True)
class CindRead:
    values: tuple[int, ...]


@dataclass(frozen=True)
class Bcs:
    codec: int


@dataclass(frozen=True)
class Chld:
    options: tuple[str, ...]


@dataclass(frozen=True)
class Clcc:
    index: int
    direction: int
    status: int
    number: str | None


@dataclass(frozen=True)
class Unknown:
    text: str


Result = Ok | Error | Ring | Clip | Ciev | Brsf | CindTest | CindRead | Bcs | Chld | Clcc | Unknown

_ERROR_RE = re.compile(r"^(ERROR|NO CARRIER|BUSY|NO ANSWER|DELAYED|BLACKLISTED|\+CME ERROR:.*)$")
_QUOTED_RE = re.compile(r'"([^"]*)"')


def _split_fields(text: str) -> list[str]:
    """Split a comma separated AT parameter list, honouring quotes and parentheses."""
    fields: list[str] = []
    current: list[str] = []
    depth = 0
    quoted = False
    for ch in text:
        if ch == '"':
            quoted = not quoted
        elif not quoted and ch == "(":
            depth += 1
        elif not quoted and ch == ")":
            depth -= 1
        if ch == "," and depth == 0 and not quoted:
            fields.append("".join(current).strip())
            current = []
        else:
            current.append(ch)
    fields.append("".join(current).strip())
    return fields


def _unquote(text: str) -> str:
    text = text.strip()
    if len(text) >= 2 and text[0] == '"' and text[-1] == '"':
        return text[1:-1]
    return text


def _int_or_none(text: str) -> int | None:
    try:
        return int(text.strip())
    except ValueError:
        return None


def parse_line(line: str) -> Result:
    """Parse one AG result line (without the CR LF framing)."""
    line = line.strip()
    if line == "OK":
        return Ok()
    if line == "RING":
        return Ring()
    if _ERROR_RE.match(line):
        return Error(line)
    if ":" not in line:
        return Unknown(line)
    head, _, rest = line.partition(":")
    head = head.strip().upper()
    rest = rest.strip()
    try:
        match head:
            case "+CLIP":
                fields = _split_fields(rest)
                return Clip(
                    _unquote(fields[0]), _int_or_none(fields[1]) if len(fields) > 1 else None
                )
            case "+CIEV":
                index, value = (int(f) for f in _split_fields(rest)[:2])
                return Ciev(index, value)
            case "+BRSF":
                return Brsf(int(rest))
            case "+BCS":
                return Bcs(int(rest))
            case "+CIND":
                if rest.startswith("("):
                    return CindTest(tuple(_QUOTED_RE.findall(rest)))
                return CindRead(tuple(int(f) for f in _split_fields(rest)))
            case "+CHLD":
                inner = rest.strip()
                if inner.startswith("(") and inner.endswith(")"):
                    inner = inner[1:-1]
                return Chld(tuple(f for f in _split_fields(inner) if f))
            case "+CLCC":
                fields = _split_fields(rest)
                number = _unquote(fields[5]) if len(fields) > 5 else None
                return Clcc(int(fields[0]), int(fields[1]), int(fields[2]), number)
    except ValueError, IndexError:
        return Unknown(line)
    return Unknown(line)


class LineReader:
    """Reassemble CR/LF framed lines from arbitrary RFCOMM reads."""

    def __init__(self) -> None:
        self._buffer = bytearray()

    def feed(self, data: bytes) -> list[str]:
        self._buffer.extend(data)
        lines: list[str] = []
        while True:
            positions = [p for p in (self._buffer.find(b"\r"), self._buffer.find(b"\n")) if p >= 0]
            if not positions:
                break
            end = min(positions)
            raw = bytes(self._buffer[:end])
            del self._buffer[: end + 1]
            text = raw.decode("ascii", errors="replace").strip()
            if text:
                lines.append(text)
        if len(self._buffer) > MAX_LINE:
            # A peer that never terminates a line must not grow memory forever.
            self._buffer.clear()
        return lines


def command(text: str) -> bytes:
    """Encode an AT command for the wire (HF to AG commands end with a single CR)."""
    return text.encode("ascii") + b"\r"

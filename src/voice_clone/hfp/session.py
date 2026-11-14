"""One HFP service level connection, Hands-Free side, without I/O.

Feed it bytes read from the RFCOMM socket and call ``tick`` when ``next_deadline``
passes; send ``take_outgoing()`` to the socket and perform ``take_actions()``.
"""

from __future__ import annotations

import enum
import logging
from collections import deque
from dataclasses import dataclass

from . import at
from .call import (
    CallAction,
    CallMachine,
    CallTimings,
    PolicyHook,
    SendAnswer,
    SendHangup,
)
from .clock import Clock

log = logging.getLogger(__name__)

COMMAND_TIMEOUT = 10.0

_INDICATOR_ALIASES = {"call_setup": "callsetup", "callsetup": "callsetup", "call": "call"}


def _redact(line: str) -> str:
    """Keep caller numbers out of logs; they are stored only in the private call records."""
    if line.upper().startswith(("+CLIP", "+CLCC", "+CCWA")):
        return line.split(":", 1)[0] + ": <number hidden>"
    return line


class SlcState(enum.StrEnum):
    CONNECTING = "connecting"
    READY = "ready"
    FAILED = "failed"


@dataclass(frozen=True)
class SlcReady:
    ag_features: int
    codec_negotiation: bool
    indicators: tuple[str, ...]


@dataclass(frozen=True)
class SlcFailed:
    reason: str


@dataclass(frozen=True)
class CodecSelected:
    codec: int


SessionAction = CallAction | SlcReady | SlcFailed | CodecSelected


@dataclass
class _Pending:
    text: str
    sent_at: float
    responses: list[at.Result]


class HfSession:
    def __init__(
        self,
        clock: Clock,
        policy: PolicyHook,
        timings: CallTimings | None = None,
        wideband: bool = False,
    ) -> None:
        self.clock = clock
        self.calls = CallMachine(clock, policy, timings)
        self.wideband = wideband
        self.hf_features = at.HF_FEATURE_CLI | at.HF_FEATURE_ENHANCED_CALL_STATUS
        if wideband:
            self.hf_features |= at.HF_FEATURE_CODEC_NEGOTIATION
        self.ag_features = 0
        self.state = SlcState.CONNECTING
        self.indicator_names: tuple[str, ...] = ()
        self._initial_values: tuple[int, ...] = ()
        self.codec = at.CODEC_CVSD
        self._reader = at.LineReader()
        self._outgoing: list[bytes] = []
        self._actions: list[SessionAction] = []
        self._queue: deque[str] = deque()
        self._pending: _Pending | None = None
        self.transcript: list[tuple[str, str]] = []
        """Every AT line in order as ("hf", text) or ("ag", text), for debugging logs."""

    # ----- public API ---------------------------------------------------------------

    def start(self) -> None:
        self._queue_command(f"AT+BRSF={self.hf_features}")

    def feed(self, data: bytes) -> None:
        for line in self._reader.feed(data):
            self.transcript.append(("ag", line))
            log.debug("AG> %s", _redact(line))
            self._handle(at.parse_line(line))

    def tick(self) -> None:
        if self._pending and self.clock.monotonic() - self._pending.sent_at >= COMMAND_TIMEOUT:
            pending = self._pending
            log.warning("no answer to %s", pending.text)
            self._complete(at.Error("TIMEOUT"))
        self._run_call_actions(self.calls.tick())

    def next_deadline(self) -> float | None:
        candidates = []
        if self._pending:
            candidates.append(self._pending.sent_at + COMMAND_TIMEOUT)
        call_deadline = self.calls.next_deadline()
        if call_deadline is not None:
            candidates.append(call_deadline)
        return min(candidates) if candidates else None

    def link_lost(self) -> None:
        self._run_call_actions(self.calls.on_link_lost())
        self._queue.clear()
        self._pending = None

    def audio_connected(self) -> None:
        self._run_call_actions(self.calls.on_audio_connected())

    def audio_closed(self) -> None:
        self._run_call_actions(self.calls.on_audio_closed())

    def conversation_done(self) -> None:
        self._run_call_actions(self.calls.on_conversation_done())

    def request_audio(self) -> bool:
        """Ask the AG to open SCO (codec connection). Returns False if unsupported."""
        if self._codec_negotiation():
            self._queue_command("AT+BCC")
            return True
        return False

    def take_outgoing(self) -> list[bytes]:
        out, self._outgoing = self._outgoing, []
        return out

    def take_actions(self) -> list[SessionAction]:
        out, self._actions = self._actions, []
        return out

    # ----- command queue ------------------------------------------------------------

    def _queue_command(self, text: str) -> None:
        self._queue.append(text)
        self._send_next()

    def _send_next(self) -> None:
        if self._pending is not None or not self._queue:
            return
        text = self._queue.popleft()
        self._pending = _Pending(text, self.clock.monotonic(), [])
        self.transcript.append(("hf", text))
        log.debug("HF> %s", text)
        self._outgoing.append(at.command(text))

    def _complete(self, final: at.Result) -> None:
        pending = self._pending
        assert pending is not None
        self._pending = None
        self._on_command_done(pending.text, pending.responses, final)
        self._send_next()

    # ----- result handling ----------------------------------------------------------

    def _handle(self, result: at.Result) -> None:
        match result:
            case at.Ok() | at.Error():
                if self._pending is not None:
                    self._complete(result)
                else:
                    log.debug("unsolicited final result %s", result)
            case at.Ring():
                self._run_call_actions(self.calls.on_ring())
            case at.Clip(number=number):
                self._run_call_actions(self.calls.on_clip(number or None))
            case at.Ciev(index=index, value=value):
                self._on_ciev(index, value)
            case at.Bcs(codec=codec):
                self._on_bcs(codec)
            case _:
                if self._pending is not None:
                    self._pending.responses.append(result)
                else:
                    log.debug("ignored %s", result)

    def _on_ciev(self, index: int, value: int) -> None:
        if not 1 <= index <= len(self.indicator_names):
            log.debug("+CIEV for unknown indicator %d", index)
            return
        name = _INDICATOR_ALIASES.get(self.indicator_names[index - 1])
        if name is None:
            return
        self._run_call_actions(self.calls.on_indicator(name, value))

    def _on_bcs(self, codec: int) -> None:
        supported = (at.CODEC_CVSD, at.CODEC_MSBC) if self.wideband else (at.CODEC_CVSD,)
        if codec in supported:
            self.codec = codec
            self._queue_command(f"AT+BCS={codec}")
            self._actions.append(CodecSelected(codec))
        else:
            # Tell the AG again what we support; it will retry with one of those.
            self._queue_command(self._bac_command())

    def _codec_negotiation(self) -> bool:
        return bool(
            self.hf_features & at.HF_FEATURE_CODEC_NEGOTIATION
            and self.ag_features & at.AG_FEATURE_CODEC_NEGOTIATION
        )

    def _bac_command(self) -> str:
        return (
            f"AT+BAC={at.CODEC_CVSD},{at.CODEC_MSBC}"
            if self.wideband
            else f"AT+BAC={at.CODEC_CVSD}"
        )

    def _fail(self, reason: str) -> None:
        if self.state != SlcState.FAILED:
            self.state = SlcState.FAILED
            self._queue.clear()
            self._actions.append(SlcFailed(reason))

    def _on_command_done(self, text: str, responses: list[at.Result], final: at.Result) -> None:
        ok = isinstance(final, at.Ok)
        if self.state == SlcState.CONNECTING:
            self._slc_step(text, responses, ok, final)
            return
        if text == "ATA":
            self._run_call_actions(self.calls.on_answer_result(ok))
        elif not ok:
            log.warning("%s failed: %s", text, final)

    def _slc_step(self, text: str, responses: list[at.Result], ok: bool, final: at.Result) -> None:
        if text.startswith("AT+BRSF"):
            brsf = next((r for r in responses if isinstance(r, at.Brsf)), None)
            if not ok or brsf is None:
                self._fail(f"AT+BRSF failed: {final}")
                return
            self.ag_features = brsf.features
            if self._codec_negotiation():
                self._queue_command(self._bac_command())
            self._queue_command("AT+CIND=?")
        elif text.startswith("AT+BAC"):
            if not ok:
                self._fail(f"AT+BAC failed: {final}")
                return
        elif text == "AT+CIND=?":
            test = next((r for r in responses if isinstance(r, at.CindTest)), None)
            if not ok or test is None:
                self._fail(f"AT+CIND=? failed: {final}")
                return
            self.indicator_names = test.names
            self._queue_command("AT+CIND?")
        elif text == "AT+CIND?":
            read = next((r for r in responses if isinstance(r, at.CindRead)), None)
            if not ok or read is None:
                self._fail(f"AT+CIND? failed: {final}")
                return
            self._initial_values = read.values
            self._queue_command("AT+CMER=3,0,0,1")
        elif text.startswith("AT+CMER"):
            if not ok:
                self._fail(f"AT+CMER failed: {final}")
                return
            # SLC is up (HFP 1.7 4.2.1.6: AT+CHLD=? is only needed when both do 3-way calling,
            # which this HF does not advertise).
            self.state = SlcState.READY
            self._actions.append(
                SlcReady(self.ag_features, self._codec_negotiation(), self.indicator_names)
            )
            self._queue_command("AT+CLIP=1")
            if self.ag_features & at.AG_FEATURE_EXTENDED_ERROR:
                self._queue_command("AT+CMEE=1")
            self._adopt_initial_state()

    def _adopt_initial_state(self) -> None:
        values = dict(zip(self.indicator_names, self._initial_values, strict=False))
        named = {_INDICATOR_ALIASES.get(k): v for k, v in values.items() if k in _INDICATOR_ALIASES}
        self._run_call_actions(
            self.calls.sync_from_status(named.get("call", 0), named.get("callsetup"))
        )

    # ----- call actions -------------------------------------------------------------

    def _run_call_actions(self, actions: list[CallAction]) -> None:
        for action in actions:
            match action:
                case SendAnswer():
                    self._queue_command("ATA")
                case SendHangup():
                    self._queue_command("AT+CHUP")
                case _:
                    self._actions.append(action)

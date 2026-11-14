"""Glue between one phone's RFCOMM/SCO sockets and the protocol logic, on asyncio."""

from __future__ import annotations

import asyncio
import contextlib
import logging
import socket
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

from . import assets, at, audio, sco
from .call import (
    CallFinished,
    CallRecord,
    CallTimings,
    ConnectAudio,
    Outcome,
    StartConversation,
    StopConversation,
)
from .clock import Clock
from .conversation import AudioHook, Conversation, CvsdLink, MsbcLink, exchange
from .policy import Action
from .session import CodecSelected, HfSession, SlcFailed, SlcReady
from .store import CallStore

log = logging.getLogger(__name__)


@dataclass
class Settings:
    answer_delay: float = 20.0
    max_message: float = 30.0
    wideband: bool = False
    intro_wav: Path | None = None
    beep_wav: Path | None = None
    audio_hooks: list[AudioHook] = field(default_factory=list)
    """Extra consumers of the caller's audio, for local speech to text later."""


def redact(number: str | None) -> str:
    if not number:
        return "withheld"
    return f"...{number[-2:]}" if len(number) > 2 else "..."


class Prompts:
    """Intro and beep PCM at a given rate, from WAV files (default: shipped placeholders)."""

    def __init__(self, intro_wav: Path | None = None, beep_wav: Path | None = None) -> None:
        self.intro_wav = intro_wav or assets.INTRO_PLACEHOLDER
        self.beep_wav = beep_wav or assets.BEEP_PLACEHOLDER
        self._cache: dict[int, list[bytes]] = {}

    def at_rate(self, rate: int) -> list[bytes]:
        if rate not in self._cache:
            self._cache[rate] = [
                audio.read_wav(self.intro_wav, rate),
                audio.read_wav(self.beep_wav, rate),
            ]
        return self._cache[rate]


class HfpConnection:
    def __init__(
        self,
        loop: asyncio.AbstractEventLoop,
        address: str,
        rfcomm: socket.socket,
        settings: Settings,
        clock: Clock,
        policy: Callable[[str | None], Action],
        store: CallStore,
        prompts: Prompts,
        open_sco: Callable[[str, bool], asyncio.Future[socket.socket]] | None = None,
        on_closed: Callable[[HfpConnection], None] | None = None,
    ) -> None:
        self.loop = loop
        self.address = address
        self.rfcomm = rfcomm
        self.settings = settings
        self.clock = clock
        self.store = store
        self.prompts = prompts
        self.open_sco = open_sco
        self.on_closed = on_closed
        self.session = HfSession(
            clock,
            policy,
            CallTimings(answer_delay=settings.answer_delay),
            wideband=settings.wideband,
        )
        self.sco_sock: socket.socket | None = None
        self.sco_mtu = sco.DEFAULT_MTU
        self.link: CvsdLink | MsbcLink | None = None
        self.conversation: Conversation | None = None
        self.sink: audio.WavSink | None = None
        self.record: CallRecord | None = None
        self.closed = False
        self._timer: asyncio.TimerHandle | None = None
        self._tasks: set[asyncio.Task[None]] = set()

    # ----- lifecycle ----------------------------------------------------------------

    def start(self) -> None:
        self.rfcomm.setblocking(False)
        self.loop.add_reader(self.rfcomm.fileno(), self._on_rfcomm_readable)
        self.session.start()
        self._pump()

    def close(self) -> None:
        if self.closed:
            return
        self.closed = True
        if self._timer:
            self._timer.cancel()
        with contextlib.suppress(ValueError, OSError):
            self.loop.remove_reader(self.rfcomm.fileno())
        self.session.link_lost()
        self._pump(writes=False)
        self._close_sco()
        self.rfcomm.close()
        log.info("%s: hands-free link closed", self.address)
        if self.on_closed:
            self.on_closed(self)

    # ----- RFCOMM -------------------------------------------------------------------

    def _on_rfcomm_readable(self) -> None:
        try:
            data = self.rfcomm.recv(1024)
        except BlockingIOError:
            return
        except OSError as exc:
            log.warning("%s: RFCOMM error %s", self.address, exc)
            data = b""
        if not data:
            self.close()
            return
        self.session.feed(data)
        self._pump()

    def on_timer(self) -> None:
        self._timer = None
        if self.closed:
            return
        self.session.tick()
        self._pump()

    def _pump(self, writes: bool = True) -> None:
        """Send queued AT commands and perform actions until the session is quiet."""
        while True:
            outgoing = self.session.take_outgoing()
            actions = self.session.take_actions()
            if not outgoing and not actions:
                break
            if writes:
                for data in outgoing:
                    try:
                        self.rfcomm.sendall(data)
                    except OSError as exc:
                        log.warning("%s: RFCOMM write failed: %s", self.address, exc)
                        writes = False
                        self.loop.call_soon(self.close)
                        break
            for action in actions:
                self._perform(action)
        self._schedule()

    def _schedule(self) -> None:
        if self._timer:
            self._timer.cancel()
            self._timer = None
        if self.closed:
            return
        deadline = self.session.next_deadline()
        if deadline is not None:
            delay = max(0.0, deadline - self.clock.monotonic())
            self._timer = self.loop.call_later(delay, self.on_timer)

    # ----- actions ------------------------------------------------------------------

    def _perform(self, action: object) -> None:
        match action:
            case SlcReady(ag_features=features, codec_negotiation=negotiation):
                log.info(
                    "%s: hands-free link ready (AG features 0x%x, codec negotiation %s)",
                    self.address,
                    features,
                    negotiation,
                )
            case SlcFailed(reason=reason):
                log.warning("%s: service level connection failed: %s", self.address, reason)
                self.loop.call_soon(self.close)
            case CodecSelected(codec=codec):
                log.info("%s: phone selected codec %s", self.address, _codec_name(codec))
            case StartConversation(record=record):
                self._start_conversation(record)
            case ConnectAudio():
                self._connect_audio()
            case StopConversation(reason=reason):
                self._stop_conversation(reason)
            case CallFinished(record=record):
                self._call_finished(record)

    def _start_conversation(self, record: CallRecord) -> None:
        self.record = record
        log.info("%s: AI answered caller %s", self.address, redact(record.number))
        self._begin_conversation()

    def _begin_conversation(self) -> None:
        """Start recording and prompts once the call is answered and the audio link exists,
        so the sample rate is the one the link actually carries."""
        if self.record is None or self.link is None or self.conversation is not None:
            return
        rate = self.link.rate
        path = self.store.audio_path(self.record.call_id)
        self.store.ensure()
        self.sink = audio.WavSink(path, rate)
        path.chmod(0o600)
        self.conversation = Conversation(
            rate,
            self.prompts.at_rate(rate),
            self.settings.max_message,
            [self.sink.write, *self.settings.audio_hooks],
        )

    def _stop_conversation(self, reason: Outcome) -> None:
        log.info("%s: conversation stopped (%s)", self.address, reason)
        self.conversation = None
        if self.sink:
            self.sink.close()
            self.sink = None
        if reason != Outcome.USER_TOOK_OVER:
            self._close_sco()

    def _call_finished(self, record: CallRecord) -> None:
        log.info(
            "%s: call from %s finished: %s", self.address, redact(record.number), record.outcome
        )
        audio_path = self.store.audio_path(record.call_id)
        extra: dict[str, object] = {"phone": self.address}
        if audio_path.exists():
            extra["audio"] = str(audio_path)
        self.store.save_record(record, extra)
        self.record = None

    def _wideband_active(self) -> bool:
        return self.settings.wideband and self.session.codec == at.CODEC_MSBC

    def _connect_audio(self) -> None:
        if self.sco_sock is not None:
            return
        if self.session.request_audio():
            log.info("%s: asked the phone to open call audio (AT+BCC)", self.address)
            return
        if self.open_sco is None:
            return
        log.info("%s: opening call audio from the laptop side", self.address)
        future = self.open_sco(self.address, self._wideband_active())

        async def wait() -> None:
            try:
                sock = await future
            except OSError as exc:
                log.warning("%s: could not open SCO: %s", self.address, exc)
                return
            self.attach_sco(sock)

        task = self.loop.create_task(wait())
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)

    # ----- SCO ----------------------------------------------------------------------

    def accepts_sco(self) -> bool:
        return not self.closed and self.session.calls.accepts_audio()

    def wants_transparent_sco(self) -> bool:
        return self._wideband_active()

    def attach_sco(self, sock: socket.socket) -> None:
        if self.sco_sock is not None or not self.accepts_sco():
            sock.close()
            return
        sock.setblocking(False)
        self.sco_sock = sock
        self.sco_mtu = sco.mtu(sock)
        self.link = MsbcLink() if self._wideband_active() else CvsdLink()
        log.info(
            "%s: call audio connected (%s, MTU %d)",
            self.address,
            "mSBC 16 kHz" if isinstance(self.link, MsbcLink) else "CVSD 8 kHz",
            self.sco_mtu,
        )
        self.loop.add_reader(sock.fileno(), self._on_sco_readable)
        self._begin_conversation()
        self.session.audio_connected()
        self._pump()

    def _on_sco_readable(self) -> None:
        sock = self.sco_sock
        if sock is None:
            return
        try:
            packet = sock.recv(max(self.sco_mtu, 1024))
        except BlockingIOError:
            return
        except OSError as exc:
            log.info("%s: SCO read ended: %s", self.address, exc)
            packet = b""
        if not packet:
            self._close_sco()
            self.session.audio_closed()
            self._pump()
            return
        if self.conversation is None or self.link is None:
            return
        try:
            reply = exchange(self.link, self.conversation, packet)
        except Exception:
            # Never leave a caller on a dead line: end the conversation and hang up.
            log.exception("%s: call audio processing failed, hanging up", self.address)
            self.conversation.done = True
        else:
            try:
                sock.send(reply)
            except BlockingIOError:
                pass
            except OSError as exc:
                log.info("%s: SCO write failed: %s", self.address, exc)
        if self.conversation.done:
            self.session.conversation_done()
            self._pump()

    def _close_sco(self) -> None:
        sock = self.sco_sock
        if sock is None:
            return
        self.sco_sock = None
        self.link = None
        with contextlib.suppress(ValueError, OSError):
            self.loop.remove_reader(sock.fileno())
        sock.close()


def _codec_name(codec: int) -> str:
    return {at.CODEC_CVSD: "CVSD", at.CODEC_MSBC: "mSBC"}.get(codec, str(codec))

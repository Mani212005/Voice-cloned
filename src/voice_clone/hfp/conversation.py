"""What happens on the line once the AI has answered.

Plays the intro and the beep, then records the caller for up to ``max_message``
seconds. Time is measured in received audio samples, so it follows the call audio
itself and needs no wall clock.
"""

from __future__ import annotations

import logging
from collections.abc import Callable

from .audio import SAMPLE_WIDTH, PcmSource
from .msbc import PCM_BYTES_PER_FRAME, H2Reassembler, MsbcDecoder, MsbcEncoder

log = logging.getLogger(__name__)

AudioHook = Callable[[bytes, int], None]
"""Called with each chunk of received caller PCM and its sample rate (future local STT)."""


class Conversation:
    def __init__(
        self,
        rate: int,
        prompts: list[bytes],
        max_message: float,
        sinks: list[AudioHook],
    ) -> None:
        self.rate = rate
        self.max_message_samples = round(max_message * rate)
        self._source = PcmSource()
        for pcm in prompts:
            self._source.queue(pcm)
        self._sinks = sinks
        self.received_samples = 0
        self.message_samples = 0
        self.done = False

    @property
    def prompts_finished(self) -> bool:
        return self._source.pending_bytes == 0

    def pull(self, size: int) -> bytes:
        """Next ``size`` bytes of PCM to send to the caller."""
        return self._source.read(size)

    def push(self, pcm: bytes) -> None:
        """PCM received from the caller."""
        samples = len(pcm) // SAMPLE_WIDTH
        self.received_samples += samples
        for sink in self._sinks:
            sink(pcm, self.rate)
        if self.prompts_finished:
            self.message_samples += samples
            if not self.done and self.message_samples >= self.max_message_samples:
                log.info("message time is up")
                self.done = True


class CvsdLink:
    """Narrowband: the controller does CVSD, the socket carries 8 kHz PCM."""

    rate = 8000

    def incoming(self, packet: bytes) -> bytes:
        return packet[: len(packet) - len(packet) % SAMPLE_WIDTH]

    def outgoing(self, size: int, pull: Callable[[int], bytes]) -> bytes:
        return pull(size)


class MsbcLink:
    """Wideband: transparent SCO carrying H2 framed mSBC at 16 kHz."""

    rate = 16000

    def __init__(self) -> None:
        self._encoder = MsbcEncoder()
        self._decoder = MsbcDecoder()
        self._reassembler = H2Reassembler()
        self._encoded = bytearray()

    def incoming(self, packet: bytes) -> bytes:
        return b"".join(self._decoder.decode(f) for f in self._reassembler.feed(packet))

    def outgoing(self, size: int, pull: Callable[[int], bytes]) -> bytes:
        while len(self._encoded) < size:
            self._encoded.extend(self._encoder.encode(pull(PCM_BYTES_PER_FRAME)))
        out = bytes(self._encoded[:size])
        del self._encoded[:size]
        return out


def exchange(link: CvsdLink | MsbcLink, conversation: Conversation, packet: bytes) -> bytes:
    """Handle one received SCO packet and return the packet to send back.

    SCO has no flow control of its own, so the usual pacing is to send exactly as
    many bytes as were received: the phone's clock then drives both directions.
    """
    conversation.push(link.incoming(packet))
    return link.outgoing(len(packet), conversation.pull)

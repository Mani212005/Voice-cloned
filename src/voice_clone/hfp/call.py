"""Incoming-call state machine for the Hands-Free unit.

Pure logic: it is fed AG events plus the current time from an injected clock and
returns actions for the caller to perform. The rules:

* A ring starts a timer of ``answer_delay`` seconds while the phone keeps ringing.
* If he answers on the phone first, or the caller gives up, nothing is sent.
* ``deny`` rejects the call once the caller number is known; ``ring`` never answers.
* Otherwise ``ATA`` is sent when the timer expires and the AI conversation starts
  once the AG reports the call active.
* When the conversation is done the daemon hangs up with ``AT+CHUP``.
* If the phone moves the audio back to its handset, he took over: stop, do not hang up.
"""

from __future__ import annotations

import enum
import itertools
import logging
from collections.abc import Callable
from dataclasses import dataclass, field

from .clock import Clock
from .policy import Action

log = logging.getLogger(__name__)


class Phase(enum.StrEnum):
    IDLE = "idle"
    RINGING = "ringing"
    ANSWERING = "answering"
    AI_ACTIVE = "ai_active"
    HANGING_UP = "hanging_up"
    USER_ACTIVE = "user_active"


class Outcome(enum.StrEnum):
    MESSAGE_TAKEN = "message_taken"
    CALLER_HUNG_UP = "caller_hung_up"
    CALLER_GAVE_UP = "caller_gave_up"
    USER_ANSWERED = "user_answered"
    USER_TOOK_OVER = "user_took_over"
    RING_ONLY = "ring_only"
    DENIED = "denied"
    ANSWER_FAILED = "answer_failed"
    LINK_LOST = "link_lost"
    AUDIO_FAILED = "audio_failed"


@dataclass(frozen=True)
class CallTimings:
    answer_delay: float = 20.0
    """Seconds of ringing before the AI answers."""
    ring_gap: float = 10.0
    """Without a callsetup indicator, ringing ends when no RING arrived for this long."""
    answer_timeout: float = 10.0
    """Give up if the AG does not report the call active this long after ATA."""
    audio_wait: float = 3.0
    """Ask to connect SCO ourselves if the AG has not done it this long after answering."""
    max_call: float = 120.0
    """Hard cap on an AI call in case audio never flows."""
    hangup_timeout: float = 5.0
    end_grace: float = 1.0
    """Some phones report callsetup=0 just before call=1; wait this long before
    concluding that the caller gave up."""
    takeover_grace: float = 1.0
    """When call audio closes during an AI call, wait this long for a +CIEV call=0 before
    concluding that he moved the audio to the phone."""


@dataclass
class CallRecord:
    call_id: str
    number: str | None
    started_wall: float
    action: Action | None = None
    answered_wall: float | None = None
    ended_wall: float | None = None
    outcome: Outcome | None = None
    notes: list[str] = field(default_factory=list)


# Actions returned to the session.
@dataclass(frozen=True)
class SendAnswer:
    pass


@dataclass(frozen=True)
class SendHangup:
    pass


@dataclass(frozen=True)
class StartConversation:
    record: CallRecord


@dataclass(frozen=True)
class ConnectAudio:
    """The AG has not opened SCO; the daemon should request or open it."""


@dataclass(frozen=True)
class StopConversation:
    reason: Outcome


@dataclass(frozen=True)
class CallFinished:
    record: CallRecord


CallAction = (
    SendAnswer | SendHangup | StartConversation | ConnectAudio | StopConversation | CallFinished
)

PolicyHook = Callable[[str | None], Action]

_call_counter = itertools.count(1)


class CallMachine:
    def __init__(self, clock: Clock, policy: PolicyHook, timings: CallTimings | None = None):
        self.clock = clock
        self.policy = policy
        self.timings = timings or CallTimings()
        self.phase = Phase.IDLE
        self.record: CallRecord | None = None
        self._action: Action | None = None
        self._ring_started = 0.0
        self._last_ring = 0.0
        self._phase_started = 0.0
        self._audio_connected = False
        self._audio_requested = False
        self._audio_closed_at: float | None = None
        self._decision_done = False
        self._setup_ended_at: float | None = None
        self._audio_failed = False
        self._end_outcome = Outcome.MESSAGE_TAKEN
        self.callsetup_known = False
        self.call = 0
        self.callsetup = 0

    # ----- helpers -----------------------------------------------------------------

    def _now(self) -> float:
        return self.clock.monotonic()

    def _enter(self, phase: Phase) -> None:
        log.info("call phase %s -> %s", self.phase, phase)
        self.phase = phase
        self._phase_started = self._now()

    def _decide(self, number: str | None) -> Action:
        try:
            action = self.policy(number)
        except Exception:
            # A broken policy file must never make the AI answer someone it should not.
            log.exception("policy lookup failed, letting the phone ring")
            action = Action.RING
        assert self.record is not None
        self.record.action = action
        self._action = action
        log.info("policy for caller: %s", action)
        return action

    def _finish(self, outcome: Outcome) -> list[CallAction]:
        actions: list[CallAction] = []
        if self.phase in (Phase.ANSWERING, Phase.AI_ACTIVE, Phase.HANGING_UP):
            actions.append(StopConversation(outcome))
        record = self.record
        if record is not None:
            record.outcome = outcome
            record.ended_wall = self.clock.wall()
            actions.append(CallFinished(record))
        self.record = None
        self._action = None
        self._audio_connected = False
        self._audio_requested = False
        self._audio_closed_at = None
        self._decision_done = False
        self._setup_ended_at = None
        self._audio_failed = False
        self._end_outcome = Outcome.MESSAGE_TAKEN
        self._enter(Phase.IDLE)
        return actions

    def _start_ringing(self) -> list[CallAction]:
        now = self._now()
        self._ring_started = now
        self._last_ring = now
        self.record = CallRecord(
            call_id=f"{int(self.clock.wall())}-{next(_call_counter)}",
            number=None,
            started_wall=self.clock.wall(),
        )
        self._enter(Phase.RINGING)
        return []

    def _setup_ended(self) -> bool:
        """The AG reported the incoming call gone and no call=1 followed within the grace."""
        return (
            self._setup_ended_at is not None
            and self._now() - self._setup_ended_at >= self.timings.end_grace
        )

    def _ringing_over(self) -> bool:
        if self.callsetup_known:
            return self._setup_ended()
        return self._now() - self._last_ring >= self.timings.ring_gap

    # ----- events from the AG -------------------------------------------------------

    def on_ring(self) -> list[CallAction]:
        if self.phase == Phase.IDLE and self.call == 0:
            return self._start_ringing()
        if self.phase == Phase.RINGING:
            self._last_ring = self._now()
        return []

    def on_clip(self, number: str | None) -> list[CallAction]:
        if self.phase != Phase.RINGING or self.record is None:
            return []
        if self.record.number is None and number:
            self.record.number = number
        if self._action is None and self._decide(self.record.number) == Action.DENY:
            self._decision_done = True
            return [SendHangup()]
        return []

    def on_indicator(self, name: str, value: int) -> list[CallAction]:
        if name == "callsetup":
            self.callsetup_known = True
            self.callsetup = value
        elif name == "call":
            self.call = value
        else:
            return []

        actions: list[CallAction] = []
        match self.phase:
            case Phase.IDLE:
                if name == "callsetup" and value == 1 and self.call == 0:
                    actions += self._start_ringing()
                elif name == "call" and value == 1:
                    # A call we did not see ring (outgoing, or connected before us).
                    self._enter(Phase.USER_ACTIVE)
            case Phase.RINGING:
                if name == "call" and value == 1:
                    outcome = (
                        Outcome.RING_ONLY if self._action == Action.RING else Outcome.USER_ANSWERED
                    )
                    actions += self._finish(outcome)
                    self._enter(Phase.USER_ACTIVE)
                elif name == "callsetup" and value == 0 and self.call == 0:
                    self._setup_ended_at = self._now()
                elif name == "callsetup" and value != 0:
                    self._setup_ended_at = None
            case Phase.ANSWERING:
                if name == "call" and value == 1:
                    actions += self._go_active()
                elif name == "callsetup" and value == 0 and self.call == 0:
                    self._setup_ended_at = self._now()
            case Phase.AI_ACTIVE:
                if name == "call" and value == 0:
                    actions += self._finish(Outcome.CALLER_HUNG_UP)
            case Phase.HANGING_UP:
                if name == "call" and value == 0:
                    actions += self._finish(self._end_outcome)
            case Phase.USER_ACTIVE:
                if name == "call" and value == 0:
                    self._enter(Phase.IDLE)
        return actions

    def on_answer_result(self, ok: bool) -> list[CallAction]:
        if self.phase != Phase.ANSWERING:
            return []
        if not ok:
            assert self.record is not None
            self.record.notes.append("ATA was refused by the phone")
            return self._finish(Outcome.ANSWER_FAILED)
        if not self.callsetup_known and self.call == 0:
            # An AG without indicator reporting: OK is all we will get.
            return self._go_active()
        return []

    def on_link_lost(self) -> list[CallAction]:
        actions: list[CallAction] = []
        if self.record is not None:
            actions += self._finish(Outcome.LINK_LOST)
        else:
            self._enter(Phase.IDLE)
        self.call = 0
        self.callsetup = 0
        self.callsetup_known = False
        return actions

    # ----- events from the audio side -------------------------------------------------

    def accepts_audio(self) -> bool:
        """Whether an SCO connection from the phone should be accepted now."""
        return self.phase in (Phase.ANSWERING, Phase.AI_ACTIVE)

    def on_audio_connected(self) -> list[CallAction]:
        self._audio_connected = True
        self._audio_closed_at = None
        return []

    def on_audio_closed(self) -> list[CallAction]:
        """SCO closed. During an AI call that may mean he moved the audio to the phone;
        ``tick`` decides once no +CIEV call=0 followed within ``takeover_grace``."""
        was_connected = self._audio_connected
        self._audio_connected = False
        if self.phase == Phase.AI_ACTIVE and was_connected and self.call == 1:
            self._audio_closed_at = self._now()
        return []

    def on_audio_failed(self) -> list[CallAction]:
        """The call audio could not be set up: hang up instead of leaving a dead line.

        Before the AG reports the call active the hang-up waits for that report.
        """
        if self.record is None:
            return []
        if self.phase == Phase.ANSWERING:
            self._audio_failed = True
            return []
        if self.phase != Phase.AI_ACTIVE:
            return []
        return self._hang_up_after_audio_failure()

    def on_conversation_done(self) -> list[CallAction]:
        if self.phase != Phase.AI_ACTIVE:
            return []
        self._enter(Phase.HANGING_UP)
        return [SendHangup()]

    # ----- timers ---------------------------------------------------------------------

    def next_deadline(self) -> float | None:
        t = self.timings
        match self.phase:
            case Phase.RINGING:
                candidates = []
                # While a possible end of the call is pending, answering is on hold, so only
                # the grace deadline matters.
                if not self._decision_done and self._setup_ended_at is None:
                    candidates.append(self._ring_started + t.answer_delay)
                if not self.callsetup_known:
                    candidates.append(self._last_ring + t.ring_gap)
                if self._setup_ended_at is not None:
                    candidates.append(self._setup_ended_at + t.end_grace)
                return min(candidates) if candidates else None
            case Phase.ANSWERING:
                candidates = [self._phase_started + t.answer_timeout]
                if self._setup_ended_at is not None:
                    candidates.append(self._setup_ended_at + t.end_grace)
                return min(candidates)
            case Phase.AI_ACTIVE:
                candidates = [self._phase_started + t.max_call]
                if self._audio_closed_at is not None:
                    candidates.append(self._audio_closed_at + t.takeover_grace)
                elif not self._audio_connected and not self._audio_requested:
                    candidates.append(self._phase_started + t.audio_wait)
                return min(candidates)
            case Phase.HANGING_UP:
                return self._phase_started + t.hangup_timeout
        return None

    def tick(self) -> list[CallAction]:
        now = self._now()
        t = self.timings
        match self.phase:
            case Phase.RINGING:
                if self._ringing_over():
                    return self._finish(self._unanswered_outcome())
                if self._setup_ended_at is not None:
                    # The incoming call may be gone; never answer while that is unresolved.
                    return []
                if not self._decision_done and now - self._ring_started >= t.answer_delay:
                    self._decision_done = True
                    action = self._action
                    if action is None:
                        assert self.record is not None
                        action = self._decide(self.record.number)
                    if action == Action.AI:
                        self._enter(Phase.ANSWERING)
                        return [SendAnswer()]
                    if action == Action.DENY:
                        return [SendHangup()]
            case Phase.ANSWERING:
                if self._setup_ended():
                    return self._finish(Outcome.CALLER_GAVE_UP)
                if now - self._phase_started >= t.answer_timeout:
                    assert self.record is not None
                    self.record.notes.append("phone did not report the call active after ATA")
                    return self._finish(Outcome.ANSWER_FAILED)
            case Phase.AI_ACTIVE:
                if now - self._phase_started >= t.max_call:
                    assert self.record is not None
                    self.record.notes.append("hit the maximum AI call length")
                    self._enter(Phase.HANGING_UP)
                    return [StopConversation(Outcome.MESSAGE_TAKEN), SendHangup()]
                if (
                    self._audio_closed_at is not None
                    and now - self._audio_closed_at >= t.takeover_grace
                ):
                    actions = self._finish(Outcome.USER_TOOK_OVER)
                    self._enter(Phase.USER_ACTIVE)
                    return actions
                if (
                    self._audio_closed_at is None
                    and not self._audio_connected
                    and not self._audio_requested
                    and now - self._phase_started >= t.audio_wait
                ):
                    self._audio_requested = True
                    return [ConnectAudio()]
            case Phase.HANGING_UP:
                if now - self._phase_started >= t.hangup_timeout:
                    return self._finish(self._end_outcome)
        return []

    # ----- internal -------------------------------------------------------------------

    def _hang_up_after_audio_failure(self) -> list[CallAction]:
        assert self.record is not None
        self.record.notes.append("call audio could not be set up")
        self._end_outcome = Outcome.AUDIO_FAILED
        self._enter(Phase.HANGING_UP)
        return [StopConversation(Outcome.AUDIO_FAILED), SendHangup()]

    def _go_active(self) -> list[CallAction]:
        assert self.record is not None
        self.record.answered_wall = self.clock.wall()
        if self._audio_failed:
            return self._hang_up_after_audio_failure()
        self._enter(Phase.AI_ACTIVE)
        return [StartConversation(self.record)]

    def _unanswered_outcome(self) -> Outcome:
        if self._action == Action.DENY:
            return Outcome.DENIED
        if self._action == Action.RING:
            return Outcome.RING_ONLY
        return Outcome.CALLER_GAVE_UP

    def sync_from_status(self, call: int, callsetup: int | None) -> list[CallAction]:
        """Adopt the AG's state right after the service level connection comes up."""
        self.call = call
        if callsetup is not None:
            self.callsetup_known = True
            self.callsetup = callsetup
        if call == 1:
            self._enter(Phase.USER_ACTIVE)
            return []
        if callsetup == 1:
            return self._start_ringing()
        return []

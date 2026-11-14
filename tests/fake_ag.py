"""A scripted Audio Gateway (phone) for driving HfSession in tests."""

from __future__ import annotations

from voice_clone.hfp import at
from voice_clone.hfp.call import CallTimings
from voice_clone.hfp.clock import FakeClock
from voice_clone.hfp.policy import Action
from voice_clone.hfp.session import HfSession

INDICATORS = ("service", "call", "callsetup", "callheld", "signal", "roam", "battchg")
AG_FEATURES = (
    at.AG_FEATURE_THREE_WAY
    | at.AG_FEATURE_REJECT_CALL
    | at.AG_FEATURE_ENHANCED_CALL_STATUS
    | at.AG_FEATURE_EXTENDED_ERROR
)


class FakeAg:
    def __init__(
        self,
        policy=lambda number: Action.AI,
        answer_delay=20.0,
        ag_features=AG_FEATURES,
        wideband=False,
        initial=(1, 0, 0, 0, 4, 0, 5),
        indicators=INDICATORS,
    ):
        self.clock = FakeClock()
        self.session = HfSession(
            self.clock, policy, CallTimings(answer_delay=answer_delay), wideband=wideband
        )
        self.ag_features = ag_features
        self.initial = initial
        self.indicators = indicators
        self.sent: list[str] = []
        self.actions: list = []
        self.auto_ok = True
        self.ata_reply = "OK"

    # --- plumbing -------------------------------------------------------------------

    def _drain(self):
        replies = []
        for raw in self.session.take_outgoing():
            text = raw.decode().rstrip("\r")
            self.sent.append(text)
            replies.extend(self._reply(text))
        self.actions.extend(self.session.take_actions())
        for line in replies:
            self.send(line)

    def _reply(self, text):
        if not self.auto_ok:
            return []
        if text.startswith("AT+BRSF"):
            return [f"+BRSF: {self.ag_features}", "OK"]
        if text == "AT+CIND=?":
            names = ",".join(f'("{n}",(0,1))' for n in self.indicators)
            return [f"+CIND: {names}", "OK"]
        if text == "AT+CIND?":
            return ["+CIND: " + ",".join(str(v) for v in self.initial), "OK"]
        if text == "ATA":
            return [self.ata_reply]
        return ["OK"]

    def send(self, line):
        self.session.feed(f"\r\n{line}\r\n".encode())
        self._drain()

    def connect(self):
        self.session.start()
        self._drain()
        return self

    def ciev(self, name, value):
        self.send(f"+CIEV: {self.indicators.index(name) + 1},{value}")

    def advance(self, seconds):
        """Move time forward, firing every deadline on the way like the daemon's timer."""
        target = self.clock.monotonic() + seconds
        while True:
            deadline = self.session.next_deadline()
            if deadline is None or deadline > target:
                break
            self.clock.advance(max(0.0, deadline - self.clock.monotonic()))
            self.session.tick()
            self._drain()
            after = self.session.next_deadline()
            if after is not None and after <= self.clock.monotonic():
                raise AssertionError(f"deadline {after} did not move past now: daemon would spin")
        self.clock.advance(target - self.clock.monotonic())
        self.session.tick()
        self._drain()

    def take(self, kind):
        found = [a for a in self.actions if isinstance(a, kind)]
        self.actions = [a for a in self.actions if not isinstance(a, kind)]
        return found

    def incoming_call(self, number="+919876543210"):
        self.ciev("callsetup", 1)
        self.send("RING")
        self.send(f'+CLIP: "{number}",145')

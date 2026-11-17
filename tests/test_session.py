import pytest
from fake_ag import AG_FEATURES, FakeAg

from voice_clone.hfp import at
from voice_clone.hfp.call import (
    CallFinished,
    ConnectAudio,
    Outcome,
    Phase,
    StartConversation,
    StopConversation,
)
from voice_clone.hfp.policy import Action
from voice_clone.hfp.session import CodecSelected, SlcFailed, SlcReady, SlcState


def outcome(ag):
    finished = ag.take(CallFinished)
    assert len(finished) == 1, finished
    return finished[0].record


# ----- service level connection ---------------------------------------------------------


def test_slc_command_sequence_narrowband():
    ag = FakeAg().connect()
    assert ag.sent == [
        f"AT+BRSF={at.HF_FEATURE_CLI | at.HF_FEATURE_ENHANCED_CALL_STATUS}",
        "AT+CIND=?",
        "AT+CIND?",
        "AT+CMER=3,0,0,1",
        "AT+CLIP=1",
        "AT+CMEE=1",
    ]
    assert ag.session.state == SlcState.READY
    ready = ag.take(SlcReady)
    assert ready[0].codec_negotiation is False


def test_slc_offers_msbc_only_when_wideband_and_ag_supports_it():
    ag = FakeAg(wideband=True, ag_features=AG_FEATURES | at.AG_FEATURE_CODEC_NEGOTIATION).connect()
    assert ag.sent[1] == "AT+BAC=1,2"
    assert ag.take(SlcReady)[0].codec_negotiation is True
    ag.send("+BCS: 2")
    assert ag.sent[-1] == "AT+BCS=2"
    assert ag.take(CodecSelected) == [CodecSelected(at.CODEC_MSBC)]
    assert ag.session.codec == at.CODEC_MSBC

    plain = FakeAg(wideband=True).connect()
    assert "AT+BAC=1,2" not in plain.sent


def test_narrowband_hf_rejects_msbc_selection():
    ag = FakeAg().connect()
    ag.send("+BCS: 2")
    assert ag.sent[-1] == "AT+BAC=1"
    assert ag.session.codec == at.CODEC_CVSD


def test_slc_fails_when_ag_rejects_brsf():
    ag = FakeAg()
    ag.auto_ok = False
    ag.connect()
    ag.send("ERROR")
    assert ag.take(SlcFailed)
    assert ag.session.state == SlcState.FAILED


def test_slc_command_timeout_fails_without_real_sleeping():
    ag = FakeAg()
    ag.auto_ok = False
    ag.connect()
    ag.advance(10)
    assert ag.take(SlcFailed)


# ----- ring -> wait N -> answer ----------------------------------------------------------


def test_ring_waits_answer_delay_then_answers_and_hangs_up_after_message():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(19.9)
    assert "ATA" not in ag.sent
    ag.advance(0.1)
    assert ag.sent[-1] == "ATA"
    assert ag.session.calls.phase == Phase.ANSWERING
    ag.ciev("call", 1)
    ag.ciev("callsetup", 0)
    started = ag.take(StartConversation)
    assert started[0].record.number == "+919876543210"
    assert ag.session.calls.phase == Phase.AI_ACTIVE

    ag.session.audio_connected()
    ag.session.conversation_done()
    ag._drain()
    assert ag.sent[-1] == "AT+CHUP"
    ag.ciev("call", 0)
    record = outcome(ag)
    assert record.outcome == Outcome.MESSAGE_TAKEN
    assert record.action == Action.AI
    assert record.answered_wall is not None
    assert ag.session.calls.phase == Phase.IDLE


def test_caller_hangs_up_before_answer_delay():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(8)
    ag.ciev("callsetup", 0)
    ag.advance(60)
    assert "ATA" not in ag.sent
    assert outcome(ag).outcome == Outcome.CALLER_GAVE_UP
    assert not ag.take(StartConversation)


def test_he_answers_on_the_phone_first():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(12)
    ag.ciev("call", 1)
    ag.ciev("callsetup", 0)
    ag.advance(60)
    assert "ATA" not in ag.sent
    assert outcome(ag).outcome == Outcome.USER_ANSWERED
    assert ag.session.calls.phase == Phase.USER_ACTIVE
    assert not ag.take(StartConversation)
    ag.ciev("call", 0)
    assert ag.session.calls.phase == Phase.IDLE


def test_phone_reporting_callsetup_end_before_call_active_is_not_a_missed_call():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(19.5)
    ag.ciev("callsetup", 0)  # some phones send this first...
    ag.advance(0.6)  # ...past the answer delay: we must not answer while unresolved
    ag.ciev("call", 1)  # ...then call=1 because he picked up
    ag.advance(60)
    assert "ATA" not in ag.sent
    assert outcome(ag).outcome == Outcome.USER_ANSWERED


def test_link_drop_while_ringing_never_answers():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(5)
    ag.session.link_lost()
    ag._drain()
    assert outcome(ag).outcome == Outcome.LINK_LOST
    ag.advance(60)
    assert "ATA" not in ag.sent


def test_link_drop_during_ai_call_stops_conversation():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.take(StartConversation)
    ag.session.link_lost()
    ag._drain()
    assert ag.take(StopConversation) == [StopConversation(Outcome.LINK_LOST)]
    assert outcome(ag).outcome == Outcome.LINK_LOST


def test_caller_hangs_up_during_ai_call():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.ciev("call", 0)
    assert ag.take(StopConversation) == [StopConversation(Outcome.CALLER_HUNG_UP)]
    assert outcome(ag).outcome == Outcome.CALLER_HUNG_UP
    assert "AT+CHUP" not in ag.sent


def test_he_takes_over_by_moving_audio_to_phone():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.session.audio_connected()
    ag.session.audio_closed()
    ag._drain()
    assert ag.session.calls.phase == Phase.AI_ACTIVE
    assert not ag.take(StopConversation)
    ag.advance(1)
    assert ag.take(StopConversation) == [StopConversation(Outcome.USER_TOOK_OVER)]
    assert outcome(ag).outcome == Outcome.USER_TOOK_OVER
    assert "AT+CHUP" not in ag.sent
    assert ag.session.calls.phase == Phase.USER_ACTIVE


def test_audio_closing_just_before_caller_hangs_up_is_not_a_takeover():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.session.audio_connected()
    ag.session.audio_closed()
    ag._drain()
    ag.advance(0.5)
    ag.ciev("call", 0)
    assert ag.take(StopConversation) == [StopConversation(Outcome.CALLER_HUNG_UP)]
    assert outcome(ag).outcome == Outcome.CALLER_HUNG_UP
    ag.advance(5)
    assert not ag.take(StopConversation)


def test_no_audio_request_while_a_takeover_is_pending():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.advance(1)
    ag.session.audio_connected()
    ag.session.audio_closed()
    ag._drain()
    ag.advance(5)
    assert not ag.take(ConnectAudio)


def test_audio_reconnecting_cancels_the_takeover():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.session.audio_connected()
    ag.session.audio_closed()
    ag.advance(0.5)
    ag.session.audio_connected()
    ag.advance(5)
    assert ag.session.calls.phase == Phase.AI_ACTIVE


def test_answer_refused_by_phone():
    ag = FakeAg().connect()
    ag.ata_reply = "ERROR"
    ag.incoming_call()
    ag.advance(20)
    assert outcome(ag).outcome == Outcome.ANSWER_FAILED


def test_answer_without_call_active_report_times_out():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.advance(10)
    assert ag.take(StopConversation) == [StopConversation(Outcome.ANSWER_FAILED)]
    assert outcome(ag).outcome == Outcome.ANSWER_FAILED


def test_requests_audio_when_phone_does_not_open_it():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.advance(2.9)
    assert not ag.take(ConnectAudio)
    ag.advance(0.1)
    assert ag.take(ConnectAudio) == [ConnectAudio()]
    ag.advance(10)
    assert not ag.take(ConnectAudio)


def test_max_call_length_hangs_up():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.session.audio_connected()
    ag.advance(120)
    assert ag.sent[-1] == "AT+CHUP"
    ag.advance(5)
    assert StopConversation(Outcome.MESSAGE_TAKEN) in ag.take(StopConversation)
    assert outcome(ag).outcome == Outcome.MESSAGE_TAKEN


def test_message_taken_stops_the_conversation():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.take(StartConversation)
    ag.session.conversation_done()
    ag._drain()
    assert not ag.take(StopConversation)
    ag.ciev("call", 0)
    assert ag.take(StopConversation) == [StopConversation(Outcome.MESSAGE_TAKEN)]
    assert outcome(ag).outcome == Outcome.MESSAGE_TAKEN


def test_unanswered_hangup_still_stops_the_conversation():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.take(StartConversation)
    ag.session.conversation_done()
    ag._drain()
    ag.advance(5)
    assert ag.take(StopConversation) == [StopConversation(Outcome.MESSAGE_TAKEN)]
    assert outcome(ag).outcome == Outcome.MESSAGE_TAKEN


def test_international_number_without_plus_is_matched_as_international():
    seen = []

    def policy(number):
        seen.append(number)
        return Action.AI

    ag = FakeAg(policy=policy).connect()
    ag.ciev("callsetup", 1)
    ag.send("RING")
    ag.send('+CLIP: "919876543210",145')
    assert seen == ["+919876543210"]
    ag.advance(20)
    assert ag.sent[-1] == "ATA"


def test_national_number_is_left_alone():
    seen = []
    ag = FakeAg(policy=lambda n: seen.append(n) or Action.AI).connect()
    ag.ciev("callsetup", 1)
    ag.send("RING")
    ag.send('+CLIP: "9876543210",129')
    assert seen == ["9876543210"]


# ----- policy -----------------------------------------------------------------------


def test_deny_rejects_as_soon_as_number_is_known():
    ag = FakeAg(policy=lambda n: Action.DENY).connect()
    ag.incoming_call()
    assert ag.sent[-1] == "AT+CHUP"
    ag.ciev("callsetup", 0)
    ag.advance(30)
    assert ag.sent.count("AT+CHUP") == 1
    assert "ATA" not in ag.sent
    assert outcome(ag).outcome == Outcome.DENIED


def test_ring_only_never_answers():
    ag = FakeAg(policy=lambda n: Action.RING).connect()
    ag.incoming_call()
    ag.advance(40)
    assert "ATA" not in ag.sent
    assert ag.session.next_deadline() is None  # no busy loop after the decision
    ag.ciev("callsetup", 0)
    ag.advance(2)
    assert outcome(ag).outcome == Outcome.RING_ONLY


def test_withheld_number_is_decided_at_the_deadline():
    seen = []

    def policy(number):
        seen.append(number)
        return Action.AI

    ag = FakeAg(policy=policy).connect()
    ag.ciev("callsetup", 1)
    ag.send("RING")
    ag.advance(20)
    assert seen == [None]
    assert ag.sent[-1] == "ATA"


def test_broken_policy_lets_the_phone_ring():
    def policy(number):
        raise ValueError("bad file")

    ag = FakeAg(policy=policy).connect()
    ag.incoming_call()
    ag.advance(40)
    assert "ATA" not in ag.sent


@pytest.mark.parametrize("delay", [5.0, 30.0])
def test_answer_delay_is_configurable(delay):
    ag = FakeAg(answer_delay=delay).connect()
    ag.incoming_call()
    ag.advance(delay - 0.01)
    assert "ATA" not in ag.sent
    ag.advance(0.01)
    assert ag.sent[-1] == "ATA"


# ----- phones without indicator reporting / state at connect ---------------------------


def test_ring_only_phone_without_callsetup_indicator():
    ag = FakeAg(indicators=("service", "call"), initial=(1, 0)).connect()
    ag.send("RING")
    ag.advance(5)
    ag.send("RING")
    ag.advance(5)
    ag.send("RING")
    ag.advance(9)
    assert "ATA" not in ag.sent
    ag.advance(10)
    assert outcome(ag).outcome == Outcome.CALLER_GAVE_UP
    assert "ATA" not in ag.sent


def test_connect_during_existing_call_is_left_alone():
    ag = FakeAg(initial=(1, 1, 0, 0, 4, 0, 5)).connect()
    assert ag.session.calls.phase == Phase.USER_ACTIVE
    ag.advance(60)
    assert "ATA" not in ag.sent


def test_connect_while_phone_is_ringing_starts_the_timer():
    ag = FakeAg(initial=(1, 0, 1, 0, 4, 0, 5)).connect()
    assert ag.session.calls.phase == Phase.RINGING
    ag.advance(20)
    assert ag.sent[-1] == "ATA"


def test_second_incoming_call_during_ai_call_is_ignored():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.ciev("callsetup", 0)
    ag.ciev("callsetup", 1)
    ag.send("RING")
    ag.advance(30)
    assert ag.sent.count("ATA") == 1


# ----- call audio that cannot be set up ---------------------------------------------------


def test_audio_failure_during_an_ai_call_hangs_up_and_ends_idle():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.take(StartConversation)
    ag.session.audio_failed()
    ag._drain()
    assert ag.sent[-1] == "AT+CHUP"
    assert ag.take(StopConversation) == [StopConversation(Outcome.AUDIO_FAILED)]
    ag.ciev("call", 0)
    record = outcome(ag)
    assert record.outcome == Outcome.AUDIO_FAILED
    assert "call audio could not be set up" in record.notes
    assert ag.session.calls.phase == Phase.IDLE


def test_audio_failure_hangup_that_the_phone_never_confirms_still_ends_idle():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    ag.ciev("call", 1)
    ag.session.audio_failed()
    ag._drain()
    ag.advance(5)
    assert outcome(ag).outcome == Outcome.AUDIO_FAILED
    assert ag.session.calls.phase == Phase.IDLE


def test_audio_failure_before_the_call_is_active_hangs_up_once_it_is():
    ag = FakeAg().connect()
    ag.incoming_call()
    ag.advance(20)
    assert ag.sent[-1] == "ATA"
    ag.session.audio_failed()
    ag._drain()
    assert ag.sent[-1] == "ATA"  # nothing to hang up yet
    ag.ciev("call", 1)
    assert ag.sent[-1] == "AT+CHUP"
    assert not ag.take(StartConversation)
    ag.ciev("call", 0)
    assert outcome(ag).outcome == Outcome.AUDIO_FAILED
    assert ag.session.calls.phase == Phase.IDLE


def test_audio_failure_outside_an_ai_call_is_ignored_and_does_not_taint_the_next_call():
    ag = FakeAg().connect()
    ag.session.audio_failed()
    ag.incoming_call()
    ag.session.audio_failed()  # still ringing
    ag._drain()
    assert "AT+CHUP" not in ag.sent
    ag.advance(20)
    ag.ciev("call", 1)
    ag.take(StartConversation)
    ag.session.conversation_done()
    ag._drain()
    ag.ciev("call", 0)
    assert outcome(ag).outcome == Outcome.MESSAGE_TAKEN

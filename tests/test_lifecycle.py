"""Every way a call can end must release its audio: SCO socket, mSBC state, recording, tasks.

All deterministic: fake RFCOMM and SCO sockets, a fake clock, and a fake libsbc that counts
codec states. Nothing here touches Bluetooth or the system libsbc.
"""

import asyncio
import json
import socket
import wave

import fake_sbc
import pytest
from test_connection import (
    answer_ai_call,
    bring_up,
    bring_up_wideband,
    make_harness,
    run,
    settle,
    teardown_harness,
)

from voice_clone.hfp.call import Phase
from voice_clone.hfp.connection import Prompts
from voice_clone.hfp.conversation import CvsdLink, MsbcLink


@pytest.fixture
def sbc(monkeypatch):
    return fake_sbc.install(monkeypatch)


@pytest.fixture
def narrow(tmp_path):
    parts = make_harness(tmp_path)
    yield parts
    loop, conn, phone, *_ = parts
    teardown_harness(loop, conn, phone)


@pytest.fixture
def wide(tmp_path, sbc):
    parts = make_harness(tmp_path, wideband=True)
    yield parts
    loop, conn, phone, *_ = parts
    teardown_harness(loop, conn, phone)


def sco_pair():
    return socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)


def records(store):
    return [json.loads(p.read_text()) for p in sorted(store.root.glob("*.json"))]


def assert_wav_complete(path, rate):
    """The WAV was flushed and closed: its header is patched and the file parses."""
    with wave.open(str(path), "rb") as wav:
        assert wav.getframerate() == rate
        assert wav.getnframes() == len(wav.readframes(wav.getnframes())) // 2


def assert_call_ended_clean(conn, sbc=None):
    assert conn.session.calls.phase in (Phase.IDLE, Phase.USER_ACTIVE)
    assert conn.sco_sock is None and conn.link is None
    assert conn.conversation is None and conn.sink is None and conn.record is None
    assert not conn._tasks
    if sbc is not None:
        assert not sbc.live, "a codec state was leaked"
        assert sbc.double_finishes == 0


def start_call(loop, conn, phone, clock, *, wideband_codec=None):
    """Ring, answer and attach a fake SCO link; returns (our end, phone's end)."""
    sco_hf, sco_phone = sco_pair()

    async def go():
        if wideband_codec is not None:
            phone.say(f"+BCS: {wideband_codec}")
            await settle()
            assert phone.commands() == [f"AT+BCS={wideband_codec}"]
            phone.say("OK")
            await settle()
        await answer_ai_call(phone, conn, clock)()
        conn.attach_sco(sco_hf)

    run(loop, go)
    assert conn.sco_sock is sco_hf
    return sco_hf, sco_phone


def say(loop, phone, *lines):
    async def go():
        phone.say(*lines)
        await settle()

    run(loop, go)


# ----- wideband: codec state is released on every way out ------------------------------


def test_wideband_codec_state_is_released_when_the_caller_hangs_up(wide, sbc):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)
    assert isinstance(conn.link, MsbcLink) and len(sbc.live) == 2

    say(loop, phone, "+CIEV: 2,0")

    assert_call_ended_clean(conn, sbc)
    assert sco_hf.fileno() == -1
    assert sbc.finishes == 2
    assert_wav_complete(next(store.root.glob("*.wav")), 16000)
    assert [r["outcome"] for r in records(store)] == ["caller_hung_up"]
    sco_phone.close()


def test_wideband_codec_state_is_released_when_the_message_is_taken(wide, sbc):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)

    async def go():
        conn.conversation.done = True
        sco_phone.send(bytes(48))
        await settle()
        assert phone.commands() == ["AT+CHUP"]
        phone.say("OK", "+CIEV: 2,0")  # the phone leaves its end of the audio open
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn, sbc)
    assert sco_hf.fileno() == -1
    assert [r["outcome"] for r in records(store)] == ["message_taken"]
    sco_phone.close()


def test_wideband_codec_state_is_released_when_the_link_drops(wide, sbc):
    loop, conn, phone, clock, store, closed = wide
    bring_up_wideband(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)
    assert len(sbc.live) == 2

    phone.sock.close()
    run(loop, settle)

    assert conn.closed and closed == [conn]
    assert_call_ended_clean(conn, sbc)
    assert sco_hf.fileno() == -1
    assert [r["outcome"] for r in records(store)] == ["link_lost"]
    assert_wav_complete(next(store.root.glob("*.wav")), 16000)
    sco_phone.close()


def test_wideband_codec_state_is_released_when_the_phone_closes_audio_and_he_takes_over(wide, sbc):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    _sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)

    async def go():
        sco_phone.close()
        await settle()
        assert not sbc.live  # released as soon as the audio is gone, not at call end
        clock.advance(1)
        conn.on_timer()
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn, sbc)
    assert [r["outcome"] for r in records(store)] == ["user_took_over"]


def test_wideband_codec_state_is_released_when_the_hangup_is_never_confirmed(wide, sbc):
    loop, conn, phone, clock, _store, _ = wide
    bring_up_wideband(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)

    async def go():
        conn.conversation.done = True
        sco_phone.send(bytes(48))
        await settle()
        clock.advance(5)
        conn.on_timer()
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn, sbc)
    assert sco_hf.fileno() == -1
    sco_phone.close()


def test_closing_the_connection_twice_releases_once(wide, sbc):
    loop, conn, phone, clock, _store, closed = wide
    bring_up_wideband(loop, conn, phone)
    _sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)
    conn.close()
    conn.close()
    assert closed == [conn]
    assert_call_ended_clean(conn, sbc)
    assert sbc.finishes == 2
    sco_phone.close()


# ----- wideband: codec renegotiation -----------------------------------------------------


def test_codec_change_mid_call_releases_the_old_audio_and_restarts_at_the_new_rate(wide, sbc):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    old_hf, old_phone = start_call(loop, conn, phone, clock, wideband_codec=2)
    old_sink = conn.sink
    assert conn.conversation.rate == 16000

    async def renegotiate():
        phone.say("+BCS: 1")
        await settle()
        assert phone.commands() == ["AT+BCS=1"]
        phone.say("OK")
        await settle()

    run(loop, renegotiate)
    # The old link, its codec state and its socket are gone before the new audio arrives.
    assert conn.link is None and conn.sco_sock is None and old_hf.fileno() == -1
    assert not sbc.live and sbc.finishes == 2
    assert conn.session.calls.phase == Phase.AI_ACTIVE  # the call itself carries on

    new_hf, new_phone = sco_pair()
    run(loop, lambda: asyncio.sleep(0))
    conn.attach_sco(new_hf)
    assert isinstance(conn.link, CvsdLink)
    assert old_sink.closed and conn.sink is not old_sink
    assert conn.conversation.rate == 8000

    say(loop, phone, "+CIEV: 2,0")
    assert_call_ended_clean(conn, sbc)
    assert_wav_complete(next(store.root.glob("*.wav")), 8000)
    (record,) = records(store)
    assert record["outcome"] == "caller_hung_up"
    assert any("restarted" in note for note in record["notes"])
    old_phone.close()
    new_phone.close()


def test_codec_change_from_narrowband_to_wideband_mid_call(wide, sbc):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    old_hf, old_phone = start_call(loop, conn, phone, clock, wideband_codec=1)
    assert isinstance(conn.link, CvsdLink)

    say(loop, phone, "+BCS: 2")
    assert phone.commands() == ["AT+BCS=2"]
    assert conn.link is None and old_hf.fileno() == -1

    new_hf, new_phone = sco_pair()
    conn.attach_sco(new_hf)
    assert isinstance(conn.link, MsbcLink) and len(sbc.live) == 2
    assert conn.conversation.rate == 16000

    say(loop, phone, "+CIEV: 2,0")
    assert_call_ended_clean(conn, sbc)
    assert_wav_complete(next(store.root.glob("*.wav")), 16000)
    old_phone.close()
    new_phone.close()


def test_reconfirming_the_same_codec_keeps_the_audio_running(wide, sbc):
    loop, conn, phone, clock, _store, _ = wide
    bring_up_wideband(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)
    link, sink = conn.link, conn.sink

    say(loop, phone, "+BCS: 2")

    assert conn.link is link and conn.sink is sink and conn.sco_sock is sco_hf
    assert len(sbc.live) == 2 and sbc.finishes == 0
    say(loop, phone, "+CIEV: 2,0")
    assert_call_ended_clean(conn, sbc)
    sco_phone.close()


# ----- wideband: errors ----------------------------------------------------------------------


def test_a_half_built_wideband_link_is_released_and_the_call_hangs_up(wide, sbc):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    sbc.fail_init_numbers = {2}  # the decoder of the new link cannot be built
    sco_hf, sco_phone = sco_pair()

    async def go():
        phone.say("+BCS: 2")
        await settle()
        phone.say("OK")
        await settle()
        phone.commands()
        await answer_ai_call(phone, conn, clock)()
        conn.attach_sco(sco_hf)
        await settle()
        assert phone.commands() == ["AT+CHUP"]
        phone.say("OK", "+CIEV: 2,0")
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn, sbc)
    assert sco_hf.fileno() == -1
    assert sbc.inits == 2 and sbc.finishes == 1  # the encoder that did get built was freed
    (record,) = records(store)
    assert record["outcome"] == "audio_failed"
    sco_phone.close()


def test_audio_that_cannot_be_built_before_the_call_is_active_hangs_up_when_it_is(wide, sbc):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    sbc.fail_init_numbers = {1}
    sco_hf, sco_phone = sco_pair()

    async def go():
        phone.say("+BCS: 2")
        await settle()
        phone.say("OK")
        await settle()
        phone.commands()
        phone.say("+CIEV: 3,1", "RING", '+CLIP: "+919876543210",145')
        await settle()
        clock.advance(20)
        conn.on_timer()
        await settle()
        assert phone.commands() == ["ATA"]
        conn.attach_sco(sco_hf)  # the phone opened audio before reporting the call active
        assert sco_hf.fileno() == -1
        assert phone.commands() == []
        phone.say("OK", "+CIEV: 2,1", "+CIEV: 3,0")
        await settle()
        assert phone.commands() == ["AT+CHUP"]
        phone.say("OK", "+CIEV: 2,0")
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn, sbc)
    assert [r["outcome"] for r in records(store)] == ["audio_failed"]
    assert not list(store.root.glob("*.wav"))
    sco_phone.close()


def test_a_codec_error_while_talking_hangs_up_and_still_releases_everything(wide, sbc):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)

    async def go():
        sbc.fail_encode = True
        sco_phone.send(bytes(48))
        await settle()
        assert phone.commands() == ["AT+CHUP"]
        phone.say("OK", "+CIEV: 2,0")
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn, sbc)
    assert sco_hf.fileno() == -1
    assert_wav_complete(next(store.root.glob("*.wav")), 16000)
    sco_phone.close()


# ----- narrowband and shared call cleanup ------------------------------------------------


def test_missing_prompt_files_release_the_audio_close_the_recording_and_hang_up(narrow, tmp_path):
    loop, conn, phone, clock, store, _ = narrow
    bring_up(loop, conn, phone)
    conn.prompts = Prompts(intro_wav=tmp_path / "missing.wav")
    sco_hf, sco_phone = sco_pair()

    async def go():
        await answer_ai_call(phone, conn, clock)()
        conn.attach_sco(sco_hf)
        await settle()
        assert phone.commands() == ["AT+CHUP"]
        phone.say("OK", "+CIEV: 2,0")
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn)
    assert sco_hf.fileno() == -1
    assert_wav_complete(next(store.root.glob("*.wav")), 8000)  # opened, then closed properly
    assert [r["outcome"] for r in records(store)] == ["audio_failed"]
    sco_phone.close()


def test_a_missing_prompt_file_is_also_caught_when_the_audio_was_attached_first(narrow, tmp_path):
    loop, conn, phone, clock, store, _ = narrow
    bring_up(loop, conn, phone)
    conn.prompts = Prompts(intro_wav=tmp_path / "missing.wav")
    sco_hf, sco_phone = sco_pair()

    async def go():
        phone.say("+CIEV: 3,1", "RING", '+CLIP: "+919876543210",145')
        await settle()
        clock.advance(20)
        conn.on_timer()
        await settle()
        phone.commands()
        conn.attach_sco(sco_hf)  # phone opened audio before the call was reported active
        phone.say("OK", "+CIEV: 2,1", "+CIEV: 3,0")  # StartConversation fails here
        await settle()
        assert phone.commands() == ["AT+CHUP"]
        phone.say("OK", "+CIEV: 2,0")
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn)
    assert sco_hf.fileno() == -1
    assert [r["outcome"] for r in records(store)] == ["audio_failed"]
    sco_phone.close()


def test_a_recording_that_fails_to_close_does_not_keep_the_audio_open(narrow, monkeypatch):
    loop, conn, phone, clock, store, _ = narrow
    bring_up(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock)

    def broken_close():
        raise OSError("disk full")

    monkeypatch.setattr(conn.sink, "close", broken_close)
    say(loop, phone, "+CIEV: 2,0")

    assert sco_hf.fileno() == -1 and conn.sco_sock is None
    assert conn.session.calls.phase == Phase.IDLE
    assert [r["outcome"] for r in records(store)] == ["caller_hung_up"]  # the record still lands
    sco_phone.close()


def test_a_record_that_fails_to_save_still_closes_the_link_and_the_audio(narrow, monkeypatch):
    loop, conn, phone, clock, store, closed = narrow
    bring_up(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock)

    def broken_save(*_args, **_kwargs):
        raise OSError("disk full")

    monkeypatch.setattr(store, "save_record", broken_save)
    phone.sock.close()
    run(loop, settle)

    assert conn.closed and closed == [conn]
    assert_call_ended_clean(conn)
    assert sco_hf.fileno() == -1 and conn.rfcomm.fileno() == -1
    sco_phone.close()


def test_he_taking_over_releases_audio_even_if_it_is_somehow_still_open(narrow):
    from voice_clone.hfp.call import Outcome as O

    loop, conn, phone, clock, _store, _ = narrow
    bring_up(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock)
    conn._stop_conversation(O.USER_TOOK_OVER)
    assert conn.sco_sock is None and sco_hf.fileno() == -1
    assert conn.conversation is None and conn.sink is None
    sco_phone.close()


# ----- audio closing and the call ending at the same moment -----------------------------------


def hangup_burst(order):
    """The audio closes and the caller hangs up in the same instant, in either order."""

    def go(loop, phone, sco_phone):
        async def inner():
            if order == "audio-first":
                sco_phone.close()
                phone.say("+CIEV: 2,0")
            else:
                phone.say("+CIEV: 2,0")
                sco_phone.close()
            await settle()

        run(loop, inner)

    return go


@pytest.mark.parametrize("order", ["audio-first", "call-end-first"])
def test_audio_close_and_call_end_together_leave_a_clean_idle_state(narrow, order):
    loop, conn, phone, clock, store, _ = narrow
    bring_up(loop, conn, phone)
    sco_hf, sco_phone = start_call(loop, conn, phone, clock)

    hangup_burst(order)(loop, phone, sco_phone)

    assert_call_ended_clean(conn)
    assert sco_hf.fileno() == -1
    assert [r["outcome"] for r in records(store)] == ["caller_hung_up"]
    assert_wav_complete(next(store.root.glob("*.wav")), 8000)
    # Nothing is left pending: no late timer may turn this into a takeover.
    clock.advance(10)
    conn.on_timer()
    run(loop, settle)
    assert [r["outcome"] for r in records(store)] == ["caller_hung_up"]
    assert phone.commands() == []
    sco_phone.close()


@pytest.mark.parametrize("order", ["audio-first", "call-end-first"])
def test_audio_close_and_call_end_together_in_wideband_release_the_codec(wide, sbc, order):
    loop, conn, phone, clock, store, _ = wide
    bring_up_wideband(loop, conn, phone)
    _sco_hf, sco_phone = start_call(loop, conn, phone, clock, wideband_codec=2)

    hangup_burst(order)(loop, phone, sco_phone)

    assert_call_ended_clean(conn, sbc)
    assert [r["outcome"] for r in records(store)] == ["caller_hung_up"]
    assert sbc.finishes == 2


def test_audio_closing_while_hanging_up_ends_as_a_taken_message(narrow):
    loop, conn, phone, clock, store, _ = narrow
    bring_up(loop, conn, phone)
    _sco_hf, sco_phone = start_call(loop, conn, phone, clock)

    async def go():
        conn.conversation.done = True
        sco_phone.send(bytes(48))
        await settle()
        assert phone.commands() == ["AT+CHUP"]
        sco_phone.close()  # the phone drops audio before it confirms the hang-up
        await settle()
        phone.say("OK", "+CIEV: 2,0")
        await settle()

    run(loop, go)
    assert_call_ended_clean(conn)
    assert [r["outcome"] for r in records(store)] == ["message_taken"]


def test_a_late_call_end_after_a_takeover_returns_to_idle_and_the_next_call_is_answered(narrow):
    loop, conn, phone, clock, store, _ = narrow
    bring_up(loop, conn, phone)
    _sco_hf, sco_phone = start_call(loop, conn, phone, clock)

    async def go():
        sco_phone.close()
        await settle()
        clock.advance(1)
        conn.on_timer()
        await settle()

    run(loop, go)
    assert conn.session.calls.phase == Phase.USER_ACTIVE
    say(loop, phone, "+CIEV: 2,0")
    assert conn.session.calls.phase == Phase.IDLE
    assert conn.session.calls.record is None

    second_hf, second_phone = sco_pair()

    async def second():
        await answer_ai_call(phone, conn, clock)()
        conn.attach_sco(second_hf)

    run(loop, second)
    assert conn.sco_sock is second_hf and conn.conversation is not None
    assert len(list(store.root.glob("*.wav"))) == 2
    second_phone.close()


# ----- audio that is still being opened when the call ends ---------------------------------------


def pending_connect(loop, conn, phone, clock):
    """A call whose laptop-side SCO connect is still in flight; returns the pending future."""
    pending = loop.create_future()
    conn.open_sco = lambda _address, _transparent: pending

    async def go():
        await answer_ai_call(phone, conn, clock)()
        clock.advance(3)  # the phone has not opened audio: we try ourselves
        conn.on_timer()
        await settle()

    run(loop, go)
    assert conn._tasks and not pending.done()
    return pending


def test_a_pending_audio_connect_is_cancelled_when_the_call_ends(narrow):
    loop, conn, phone, clock, _store, _ = narrow
    bring_up(loop, conn, phone)
    pending = pending_connect(loop, conn, phone, clock)

    say(loop, phone, "+CIEV: 2,0")

    assert pending.cancelled()
    assert_call_ended_clean(conn)


def test_a_pending_audio_connect_is_cancelled_when_the_link_drops(narrow):
    loop, conn, phone, clock, _store, _ = narrow
    bring_up(loop, conn, phone)
    pending = pending_connect(loop, conn, phone, clock)

    phone.sock.close()
    run(loop, settle)

    assert pending.cancelled()
    assert_call_ended_clean(conn)


def test_the_socket_of_an_in_flight_audio_connect_is_released_when_the_call_ends(narrow):
    loop, conn, phone, clock, _store, _ = narrow
    bring_up(loop, conn, phone)
    half_open_hf, half_open_phone = sco_pair()
    never = loop.create_future()

    def open_sco(_address, _transparent):
        async def connect():  # shaped like sco.connect: closes its socket if cancelled
            try:
                await never
            except BaseException:
                half_open_hf.close()
                raise
            return half_open_hf

        return loop.create_task(connect())

    conn.open_sco = open_sco

    async def go():
        await answer_ai_call(phone, conn, clock)()
        clock.advance(3)
        conn.on_timer()
        await settle()
        assert half_open_hf.fileno() != -1
        phone.say("+CIEV: 2,0")
        await settle()

    run(loop, go)
    assert half_open_hf.fileno() == -1
    assert_call_ended_clean(conn)
    half_open_phone.close()

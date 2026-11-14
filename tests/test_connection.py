"""HfpConnection over fake RFCOMM and SCO sockets (Unix socketpairs), no Bluetooth."""

import asyncio
import json
import socket
import wave

import pytest

from voice_clone.hfp import audio
from voice_clone.hfp.call import Phase
from voice_clone.hfp.clock import FakeClock
from voice_clone.hfp.connection import HfpConnection, Prompts, Settings
from voice_clone.hfp.policy import Action
from voice_clone.hfp.store import CallStore

CIND_TEST = '+CIND: ("service",(0,1)),("call",(0,1)),("callsetup",(0-3)),("callheld",(0-2))'


class Phone:
    """The AG end of the fake RFCOMM link."""

    def __init__(self, sock: socket.socket):
        self.sock = sock
        self.sock.setblocking(False)
        self.buffer = b""

    def commands(self) -> list[str]:
        try:
            while chunk := self.sock.recv(4096):
                self.buffer += chunk
        except BlockingIOError:
            pass
        parts = self.buffer.split(b"\r")
        self.buffer = parts.pop()
        return [p.decode() for p in parts]

    def say(self, *lines: str) -> None:
        self.sock.sendall(b"".join(f"\r\n{line}\r\n".encode() for line in lines))


async def settle():
    for _ in range(10):
        await asyncio.sleep(0)


@pytest.fixture
def harness(tmp_path):
    loop = asyncio.new_event_loop()
    hf_sock, ag_sock = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    clock = FakeClock()
    store = CallStore(tmp_path / "calls")
    closed = []
    conn = HfpConnection(
        loop,
        "AA:BB:CC:DD:EE:FF",
        hf_sock,
        Settings(answer_delay=20.0, max_message=0.03),
        clock,
        lambda number: Action.AI,
        store,
        Prompts(),
        on_closed=closed.append,
    )
    phone = Phone(ag_sock)
    yield loop, conn, phone, clock, store, closed
    conn.close()
    ag_sock.close()
    loop.close()


def run(loop, coro_fn):
    return loop.run_until_complete(coro_fn())


def bring_up(loop, conn, phone):
    async def go():
        conn.start()
        await settle()
        assert phone.commands() == ["AT+BRSF=36"]
        phone.say("+BRSF: 871", "OK")
        await settle()
        assert phone.commands() == ["AT+CIND=?"]
        phone.say(CIND_TEST, "OK")
        await settle()
        assert phone.commands() == ["AT+CIND?"]
        phone.say("+CIND: 1,0,0,0", "OK")
        await settle()
        assert phone.commands() == ["AT+CMER=3,0,0,1"]
        phone.say("OK")
        await settle()
        assert phone.commands() == ["AT+CLIP=1"]
        phone.say("OK")
        await settle()
        assert phone.commands() == ["AT+CMEE=1"]
        phone.say("OK")
        await settle()

    run(loop, go)


def test_full_ai_call_over_fake_sockets(harness):
    loop, conn, phone, clock, store, _ = harness
    bring_up(loop, conn, phone)
    sco_hf, sco_phone = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    intro = Prompts().at_rate(8000)

    async def go():
        phone.say("+CIEV: 3,1", "RING", '+CLIP: "+919876543210",145')
        await settle()
        assert phone.commands() == []
        # SCO for an in-band ringtone before we answer is refused.
        early_hf, early_phone = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        conn.attach_sco(early_hf)
        assert early_hf.fileno() == -1
        early_phone.close()

        clock.advance(20)
        conn.on_timer()
        await settle()
        assert phone.commands() == ["ATA"]
        phone.say("OK", "+CIEV: 2,1", "+CIEV: 3,0")
        await settle()
        assert conn.session.calls.phase == Phase.AI_ACTIVE

        conn.attach_sco(sco_hf)
        sent = b""
        for i in range(1000):  # 1.8 s of prompts is 600 packets of 3 ms
            sco_phone.send(bytes([i % 256, 0]) * 24)
            await settle()
            sco_phone.settimeout(1)
            sent += sco_phone.recv(1024)
            if conn.session.calls.phase == Phase.HANGING_UP:
                break
        assert phone.commands() == ["AT+CHUP"]
        assert sent[:96] == (intro[0] + intro[1])[:96]
        phone.say("OK", "+CIEV: 2,0")
        await settle()
        return i + 1

    packets = run(loop, go)
    assert conn.session.calls.phase == Phase.IDLE
    records = list(store.root.glob("*.json"))
    assert len(records) == 1
    data = json.loads(records[0].read_text())
    assert data["outcome"] == "message_taken"
    assert data["number"] == "+919876543210"
    with wave.open(data["audio"], "rb") as wav:
        assert wav.getframerate() == 8000
        assert wav.getnframes() == packets * 24
    sco_phone.close()


def test_rfcomm_eof_is_a_link_drop(harness):
    loop, conn, phone, _clock, store, closed = harness
    bring_up(loop, conn, phone)

    async def go():
        phone.say("+CIEV: 3,1", "RING")
        await settle()
        phone.sock.close()
        await settle()

    run(loop, go)
    assert conn.closed
    assert closed == [conn]
    data = json.loads(next(store.root.glob("*.json")).read_text())
    assert data["outcome"] == "link_lost"
    assert "audio" not in data


def test_takeover_keeps_call_up(harness):
    loop, conn, phone, clock, store, _ = harness
    bring_up(loop, conn, phone)
    sco_hf, sco_phone = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)

    async def go():
        phone.say("+CIEV: 3,1", "RING")
        await settle()
        clock.advance(20)
        conn.on_timer()
        await settle()
        phone.commands()
        phone.say("OK", "+CIEV: 2,1", "+CIEV: 3,0")
        await settle()
        conn.attach_sco(sco_hf)
        sco_phone.send(bytes(48))
        await settle()
        sco_phone.close()  # he pressed "audio: phone"
        await settle()
        assert phone.commands() == []

    run(loop, go)
    assert conn.session.calls.phase == Phase.USER_ACTIVE
    data = json.loads(next(store.root.glob("*.json")).read_text())
    assert data["outcome"] == "user_took_over"


def test_placeholder_rate_conversion_for_wideband():
    prompts = Prompts()
    nb = prompts.at_rate(8000)
    wb = prompts.at_rate(16000)
    assert abs(len(wb[0]) - 2 * len(nb[0])) <= 4
    assert nb[0] == audio.placeholder_intro(8000)

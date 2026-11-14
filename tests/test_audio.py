import gc
import wave

import fake_sbc
import numpy as np
import pytest

from voice_clone.hfp import audio, msbc
from voice_clone.hfp.conversation import Conversation, CvsdLink, MsbcLink, exchange


def dominant_freq(pcm: bytes, rate: int) -> float:
    samples = np.frombuffer(pcm, dtype="<i2").astype(np.float64)
    spectrum = np.abs(np.fft.rfft(samples * np.hanning(len(samples))))
    return float(np.fft.rfftfreq(len(samples), 1 / rate)[np.argmax(spectrum)])


def write_wav(path, samples: np.ndarray, rate: int, channels: int = 1):
    with wave.open(str(path), "wb") as wav:
        wav.setnchannels(channels)
        wav.setsampwidth(2)
        wav.setframerate(rate)
        wav.writeframes(samples.astype("<i2").tobytes())


@pytest.mark.parametrize("dst_rate", [audio.NARROWBAND_RATE, audio.WIDEBAND_RATE])
def test_read_wav_resamples_stereo_44k_to_sco_rate(tmp_path, dst_rate):
    rate = 44100
    t = np.arange(rate) / rate
    mono = (np.sin(2 * np.pi * 1000 * t) * 10000).astype(np.int16)
    stereo = np.stack([mono, mono], axis=1).reshape(-1)
    path = tmp_path / "clip.wav"
    write_wav(path, stereo, rate, channels=2)
    pcm = audio.read_wav(path, dst_rate)
    assert abs(len(pcm) // 2 - dst_rate) <= 2
    assert abs(dominant_freq(pcm, dst_rate) - 1000) < 5
    peak = np.abs(np.frombuffer(pcm, dtype="<i2")).max()
    assert 9000 < peak < 11000


def test_resampling_removes_content_above_new_nyquist():
    rate = 48000
    t = np.arange(rate) / rate
    high = (np.sin(2 * np.pi * 6000 * t) * 10000).astype(np.int16)
    out = audio.resample(high, rate, audio.NARROWBAND_RATE)
    # Skip the filter's edge transient where the test tone starts and stops abruptly.
    assert np.abs(out[100:-100]).max() < 300  # 6 kHz cannot exist at 8 kHz, must not alias down


def test_read_wav_same_rate_is_lossless(tmp_path):
    samples = np.arange(-500, 500, dtype=np.int16)
    path = tmp_path / "same.wav"
    write_wav(path, samples, 8000)
    assert audio.read_wav(path, 8000) == samples.astype("<i2").tobytes()


def test_split_packets_pads_last_packet():
    packets = audio.split_packets(bytes(range(100)), 48)
    assert [len(p) for p in packets] == [48, 48, 48]
    assert packets[2][:4] == bytes([96, 97, 98, 99])
    assert packets[2][4:] == bytes(44)
    with pytest.raises(ValueError):
        audio.split_packets(b"x", 0)


def test_pcm_source_plays_queue_then_silence():
    source = audio.PcmSource()
    source.queue(b"\x01\x02\x03\x04")
    assert source.read(2) == b"\x01\x02"
    assert source.read(4) == b"\x03\x04\x00\x00"
    assert source.read(2) == b"\x00\x00"


def test_placeholder_prompts_are_tones_not_voice():
    intro = audio.placeholder_intro(8000)
    assert abs(dominant_freq(intro[: 2 * 2000], 8000) - 440) < 10
    beep = audio.placeholder_beep(16000)
    assert abs(dominant_freq(beep, 16000) - 1000) < 10


def test_shipped_placeholder_wavs_match_the_generator():
    from voice_clone.hfp import assets

    for path, expected in (
        (assets.INTRO_PLACEHOLDER, audio.placeholder_intro(8000)),
        (assets.BEEP_PLACEHOLDER, audio.placeholder_beep(8000)),
    ):
        with wave.open(str(path), "rb") as wav:
            assert (wav.getnchannels(), wav.getsampwidth(), wav.getframerate()) == (1, 2, 8000)
            assert wav.readframes(wav.getnframes()) == expected


def test_wav_sink_writes_playable_file(tmp_path):
    sink = audio.WavSink(tmp_path / "rec.wav", 8000)
    sink.write(b"\x01\x00" * 80, 8000)
    sink.close()
    with wave.open(str(tmp_path / "rec.wav"), "rb") as wav:
        assert wav.getframerate() == 8000
        assert wav.getnframes() == 80


# ----- SCO exchange ---------------------------------------------------------------------


def test_cvsd_exchange_sends_prompt_audio_paced_by_received_packets():
    prompt = bytes(range(256)) * 2
    received = []
    conv = Conversation(
        8000, [prompt], max_message=0.006, sinks=[lambda pcm, r: received.append(pcm)]
    )
    link = CvsdLink()
    replies = [exchange(link, conv, b"\x10\x00" * 24) for _ in range(11)]
    assert all(len(r) == 48 for r in replies)
    assert b"".join(replies)[: len(prompt)] == prompt
    assert b"".join(received) == b"\x10\x00" * 24 * 11
    # Prompt is 256 samples = 10.67 packets; message time counts only after it.
    assert not conv.done
    exchange(link, conv, b"\x00" * 48)
    exchange(link, conv, b"\x00" * 48)
    assert conv.done


needs_libsbc = pytest.mark.skipif(not msbc.available(), reason="libsbc.so.1 not installed")


def test_h2_pack_and_reassemble_across_arbitrary_packet_sizes():
    frames = [bytes([msbc.MSBC_SYNC]) + bytes([i]) * 56 for i in range(9)]
    stream = b"".join(msbc.h2_pack(f, i) for i, f in enumerate(frames))
    assert stream[1] == 0x08 and stream[61] == 0x38 and stream[121] == 0xC8 and stream[181] == 0xF8
    rng = np.random.default_rng(7)
    for _ in range(20):
        reassembler = msbc.H2Reassembler()
        out = []
        pos = 0
        # Leading garbage must be skipped.
        out += reassembler.feed(b"\x00\x55\x01")
        while pos < len(stream):
            size = int(rng.integers(1, 80))
            out += reassembler.feed(stream[pos : pos + size])
            pos += size
        assert out == frames


def test_h2_pack_rejects_wrong_size():
    with pytest.raises(ValueError):
        msbc.h2_pack(b"\xad", 0)


@needs_libsbc
def test_msbc_roundtrip_keeps_a_tone():
    encoder = msbc.MsbcEncoder()
    decoder = msbc.MsbcDecoder()
    tone = audio.tone(1000, 0.3, 16000, amplitude=0.5)
    tone = tone[: len(tone) - len(tone) % msbc.PCM_BYTES_PER_FRAME]
    decoded = []
    for i in range(0, len(tone), msbc.PCM_BYTES_PER_FRAME):
        frame = encoder.encode(tone[i : i + msbc.PCM_BYTES_PER_FRAME])
        assert len(frame) == msbc.H2_FRAME_BYTES
        decoded.append(decoder.decode(frame[2:59]))
    pcm = b"".join(decoded)
    assert abs(dominant_freq(pcm, 16000) - 1000) < 20


@needs_libsbc
def test_msbc_link_produces_exact_packet_sizes():
    conv = Conversation(16000, [audio.tone(500, 0.2, 16000)], max_message=1.0, sinks=[])
    link = MsbcLink()
    silence_frame = msbc.MsbcEncoder().encode(bytes(msbc.PCM_BYTES_PER_FRAME))
    incoming = silence_frame * 4
    out = b""
    for i in range(0, len(incoming), 24):
        reply = exchange(link, conv, incoming[i : i + 24])
        assert len(reply) == 24
        out += reply
    assert msbc.H2Reassembler().feed(out)  # what we send parses as H2 mSBC
    assert conv.received_samples == 4 * msbc.SAMPLES_PER_FRAME


def test_sco_sockaddr_round_trip_uses_kernel_byte_order():
    from voice_clone.hfp import sco

    packed = sco.pack_sockaddr("14:13:33:8B:27:B4")
    assert packed == bytes([31, 0, 0xB4, 0x27, 0x8B, 0x33, 0x13, 0x14])
    assert sco.unpack_sockaddr(packed) == "14:13:33:8B:27:B4"
    with pytest.raises(ValueError):
        sco.pack_sockaddr("14:13")


# ----- codec lifecycle (fake libsbc, so it runs without the system library) ---------------


@pytest.fixture
def sbc(monkeypatch):
    return fake_sbc.install(monkeypatch)


def test_codec_close_releases_exactly_once(sbc):
    encoder = msbc.MsbcEncoder()
    assert sbc.live == {1}
    encoder.close()
    encoder.close()
    assert sbc.finishes == 1 and not sbc.live
    del encoder
    gc.collect()
    assert sbc.finishes == 1 and sbc.double_finishes == 0


def test_codec_use_after_close_fails_loudly_instead_of_touching_freed_state(sbc):
    encoder = msbc.MsbcEncoder()
    decoder = msbc.MsbcDecoder()
    encoder.close()
    decoder.close()
    with pytest.raises(RuntimeError, match="after close"):
        encoder.encode(bytes(msbc.PCM_BYTES_PER_FRAME))
    with pytest.raises(RuntimeError, match="after close"):
        decoder.decode(bytes(msbc.MSBC_FRAME_BYTES))


def test_codec_that_fails_to_initialise_holds_nothing(sbc):
    sbc.fail_init_numbers = {1}
    with pytest.raises(RuntimeError, match="sbc_init_msbc"):
        msbc.MsbcEncoder()
    gc.collect()
    assert sbc.finishes == 0 and not sbc.live


def test_dropping_a_codec_without_close_still_releases_it(sbc):
    msbc.MsbcDecoder()
    gc.collect()
    assert sbc.finishes == 1 and not sbc.live


def test_msbc_link_close_releases_encoder_and_decoder(sbc):
    link = MsbcLink()
    assert len(sbc.live) == 2
    link.close()
    link.close()
    assert not sbc.live and sbc.finishes == 2 and sbc.double_finishes == 0
    with pytest.raises(RuntimeError, match="after close"):
        link.outgoing(48, bytes)


def test_msbc_link_that_cannot_build_its_decoder_releases_its_encoder(sbc):
    sbc.fail_init_numbers = {2}
    with pytest.raises(RuntimeError):
        MsbcLink()
    assert not sbc.live and sbc.finishes == 1


def test_cvsd_link_close_is_a_no_op():
    link = CvsdLink()
    link.close()
    assert link.incoming(b"\x01\x00\x02") == b"\x01\x00"

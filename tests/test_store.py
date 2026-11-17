import asyncio
import json
import os
import stat
import time

import pytest

from voice_clone import main
from voice_clone.hfp.call import CallRecord, Outcome
from voice_clone.hfp.store import MARKER, CallStore, StoreError, default_calls_dir

NOW = 1_800_000_000.0
OLD = 48 * 3600
# Seconds since the epoch, as CallMachine builds call ids.
ID_A, ID_B, ID_C, ID_D = (f"{1_799_000_000 + n}-{n}" for n in (1, 2, 3, 4))


def _touch(path, age=OLD):
    path.write_bytes(b"x")
    os.utime(path, (NOW - age, NOW - age))


def _claimed(tmp_path, name="calls"):
    store = CallStore(tmp_path / name)
    store.claim()
    return store


def _listing(directory):
    return sorted(p.name for p in directory.iterdir())


# ----- what the deleter deletes -----------------------------------------------------


def test_cleanup_deletes_only_files_older_than_24h(tmp_path):
    store = _claimed(tmp_path)
    old_wav = store.root / f"{ID_A}.wav"
    old_json = store.root / f"{ID_A}.json"
    fresh = store.root / f"{ID_B}.wav"
    edge = store.root / f"{ID_C}.wav"
    _touch(old_wav, 25 * 3600)
    _touch(old_json, 24 * 3600 + 1)
    _touch(fresh, 23 * 3600)
    _touch(edge, 24 * 3600)
    removed = store.cleanup(NOW)
    assert sorted(p.name for p in removed) == sorted([old_json.name, old_wav.name, edge.name])
    assert _listing(store.root) == sorted([MARKER, fresh.name])


def test_cleanup_removes_a_stale_tmp_record_and_keeps_the_marker(tmp_path):
    store = _claimed(tmp_path)
    stale_tmp = store.root / f"{ID_A}.json.tmp"
    _touch(stale_tmp)
    marker = store.root / MARKER
    os.utime(marker, (NOW - OLD, NOW - OLD))
    assert [p.name for p in store.cleanup(NOW)] == [stale_tmp.name]
    assert marker.exists()


@pytest.mark.parametrize(
    "name",
    [
        "notes.txt",
        f"{ID_A}.wav.keep",
        f"{ID_A}.wav.bak",
        f"x{ID_A}.wav",
        f"{ID_A}.WAV",
        f"{ID_A}.txt",
        f".{ID_A}.wav",
        f"{ID_A}.json.tmp.old",
        "2024-05.json",  # looks like a call id but is too short to be seconds
        "123456789-1.wav",
        "12345678901-1.wav",
        "1799000001-.wav",
        "-1.wav",
        "\u0661\u0667\u0669\u0669\u0660\u0660\u0660\u0660\u0660\u0661-\u0661.wav",  # not ASCII
        "1799000001-1 .wav",
        "1799000001-1.wav\n",
    ],
)
def test_cleanup_never_touches_files_the_store_did_not_create(tmp_path, name):
    store = _claimed(tmp_path)
    foreign = store.root / name
    _touch(foreign)
    assert store.cleanup(NOW) == []
    assert foreign.exists()


def test_cleanup_leaves_foreign_files_alongside_expiring_own_files(tmp_path):
    store = _claimed(tmp_path)
    mine = store.root / f"{ID_A}.wav"
    other = store.root / "notes.txt"
    _touch(mine)
    _touch(other)
    assert [p.name for p in store.cleanup(NOW)] == [mine.name]
    assert other.exists()


# ----- symlinks ---------------------------------------------------------------------


def test_cleanup_never_follows_or_removes_a_symlink_named_like_a_call_file(tmp_path):
    store = _claimed(tmp_path)
    victim = tmp_path / "victim.txt"
    _touch(victim)
    link = store.root / f"{ID_A}.wav"
    link.symlink_to(victim)
    os.utime(link, (NOW - OLD, NOW - OLD), follow_symlinks=False)
    dangling = store.root / f"{ID_B}.json"
    dangling.symlink_to(tmp_path / "missing")
    assert store.cleanup(NOW) == []
    assert link.is_symlink() and dangling.is_symlink()
    assert victim.read_bytes() == b"x"


def test_cleanup_removes_only_the_name_of_a_hard_link(tmp_path):
    store = _claimed(tmp_path)
    outside = tmp_path / "outside.bin"
    _touch(outside)
    linked = store.root / f"{ID_A}.wav"
    os.link(outside, linked)
    assert [p.name for p in store.cleanup(NOW)] == [linked.name]
    assert outside.read_bytes() == b"x"


def test_cleanup_refuses_a_symlinked_store_directory(tmp_path):
    real = tmp_path / "real"
    real.mkdir()
    real.chmod(0o755)
    (real / MARKER).write_bytes(b"x")
    victim = real / f"{ID_A}.wav"
    _touch(victim)
    link = tmp_path / "calls"
    link.symlink_to(real, target_is_directory=True)
    with pytest.raises(StoreError, match="symlink"):
        CallStore(link).cleanup(NOW)
    assert victim.exists()


def test_claim_refuses_a_symlinked_directory_and_leaves_the_target_alone(tmp_path):
    real = tmp_path / "real"
    real.mkdir()
    real.chmod(0o755)
    link = tmp_path / "calls"
    link.symlink_to(real, target_is_directory=True)
    with pytest.raises(StoreError, match="symlink"):
        CallStore(link).claim()
    assert stat.S_IMODE(real.stat().st_mode) == 0o755
    assert _listing(real) == []


def test_a_marker_that_is_a_symlink_or_directory_does_not_make_a_store(tmp_path):
    for kind in ("symlink", "dir"):
        root = tmp_path / kind
        root.mkdir()
        _touch(root / f"{ID_A}.wav")
        if kind == "symlink":
            (root / MARKER).symlink_to(tmp_path)
        else:
            (root / MARKER).mkdir()
        with pytest.raises(StoreError):
            CallStore(root).cleanup(NOW)
        assert (root / f"{ID_A}.wav").exists()


# ----- nested directories -------------------------------------------------------------


def test_cleanup_does_not_remove_or_enter_directories(tmp_path):
    store = _claimed(tmp_path)
    named_like_a_call = store.root / f"{ID_A}.wav"
    named_like_a_call.mkdir()
    inner = named_like_a_call / f"{ID_B}.wav"
    _touch(inner)
    nested = store.root / "sub"
    nested.mkdir()
    deep = nested / f"{ID_C}.wav"
    _touch(deep)
    os.utime(named_like_a_call, (NOW - OLD, NOW - OLD))
    assert store.cleanup(NOW) == []
    assert inner.exists() and deep.exists() and named_like_a_call.is_dir()


# ----- a directory that is not the store's own ----------------------------------------


def _fake_home(tmp_path):
    home = tmp_path / "home"
    home.mkdir()
    home.chmod(0o755)
    for name in (".bashrc", "notes.txt", f"{ID_A}.wav", f"{ID_B}.json", "2024-05.json"):
        _touch(home / name)
    (home / "Documents").mkdir()
    _touch(home / "Documents" / f"{ID_C}.wav")
    return home


def test_cleanup_refuses_a_directory_the_store_did_not_claim(tmp_path, monkeypatch):
    home = _fake_home(tmp_path)
    monkeypatch.setenv("HOME", str(home))
    before = _listing(home)
    with pytest.raises(StoreError, match="not a call store"):
        CallStore(home).cleanup(NOW)
    assert _listing(home) == before
    assert (home / "Documents" / f"{ID_C}.wav").exists()


def test_claim_refuses_a_directory_with_other_files_and_keeps_its_mode(tmp_path):
    home = _fake_home(tmp_path)
    before = _listing(home)
    with pytest.raises(StoreError, match="pick a dedicated directory"):
        CallStore(home).claim()
    assert stat.S_IMODE(home.stat().st_mode) == 0o755
    assert _listing(home) == before
    assert not (home / MARKER).exists()


def test_ensure_never_claims_a_directory_with_other_files(tmp_path):
    home = _fake_home(tmp_path)
    with pytest.raises(StoreError):
        CallStore(home).ensure()
    assert stat.S_IMODE(home.stat().st_mode) == 0o755


def test_cleanup_command_refuses_a_misconfigured_directory(tmp_path, capsys):
    home = _fake_home(tmp_path)
    before = _listing(home)
    assert main(["hfp", "cleanup", "--calls-dir", str(home)]) == 2
    assert "not a call store" in capsys.readouterr().err
    assert _listing(home) == before


def test_cleanup_command_deletes_from_a_claimed_store(tmp_path, capsys):
    store = _claimed(tmp_path)
    expired = store.root / f"{ID_A}.wav"
    expired.write_bytes(b"x")
    os.utime(expired, (time.time() - OLD, time.time() - OLD))  # the command reads the real clock
    assert main(["hfp", "cleanup", "--calls-dir", str(store.root)]) == 0
    assert "deleted 1 file(s)" in capsys.readouterr().out


# ----- claiming and the rest of the store ---------------------------------------------


def test_claim_accepts_a_new_empty_or_own_directory(tmp_path):
    fresh = CallStore(tmp_path / "new")
    fresh.claim()
    assert stat.S_IMODE(fresh.root.stat().st_mode) == 0o700
    assert stat.S_IMODE((fresh.root / MARKER).stat().st_mode) == 0o600
    (fresh.root / f"{ID_A}.wav").write_bytes(b"x")
    fresh.claim()
    empty = tmp_path / "empty"
    empty.mkdir()
    CallStore(empty).claim()
    assert stat.S_IMODE(empty.stat().st_mode) == 0o700
    legacy = tmp_path / "legacy"  # made by a version without the marker, only call files in it
    legacy.mkdir()
    _touch(legacy / f"{ID_A}.wav")
    CallStore(legacy).claim()
    assert (legacy / MARKER).exists()


def test_cleanup_on_missing_dir_is_a_no_op(tmp_path):
    assert CallStore(tmp_path / "nope").cleanup(NOW) == []
    assert not (tmp_path / "nope").exists()


def test_cleanup_on_a_file_instead_of_a_directory_is_refused(tmp_path):
    path = tmp_path / "calls"
    path.write_bytes(b"x")
    with pytest.raises(StoreError):
        CallStore(path).cleanup(NOW)


def test_store_is_private_and_records_are_json(tmp_path):
    store = CallStore(tmp_path / "calls")
    record = CallRecord(call_id=ID_A, number="+91123", started_wall=NOW)
    record.outcome = Outcome.MESSAGE_TAKEN
    path = store.save_record(record, {"audio": "x.wav"})
    assert stat.S_IMODE(store.root.stat().st_mode) == 0o700
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    data = json.loads(path.read_text())
    assert data["number"] == "+91123"
    assert data["outcome"] == "message_taken"
    assert data["audio"] == "x.wav"


def test_saved_records_expire_through_cleanup(tmp_path):
    store = CallStore(tmp_path / "calls")
    path = store.save_record(CallRecord(call_id=ID_A, number=None, started_wall=NOW))
    os.utime(path, (NOW - OLD, NOW - OLD))
    assert store.cleanup(NOW) == [path]


def test_default_dir_is_outside_any_repo(monkeypatch, tmp_path):
    monkeypatch.delenv("XDG_DATA_HOME", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    assert default_calls_dir() == tmp_path / ".local" / "share" / "voiceclone-calls"
    monkeypatch.setenv("XDG_DATA_HOME", str(tmp_path / "data"))
    assert default_calls_dir() == tmp_path / "data" / "voiceclone-calls"


def test_the_hourly_cleanup_loop_survives_a_failed_round(monkeypatch, tmp_path):
    from voice_clone.hfp import bluez
    from voice_clone.hfp.clock import FakeClock
    from voice_clone.hfp.connection import Settings

    monkeypatch.setattr(bluez, "CLEANUP_INTERVAL", 0)
    daemon = bluez.Daemon(
        bluez.DaemonConfig(Settings(), None, tmp_path / "calls"), clock=FakeClock()
    )
    rounds = []
    finished = asyncio.Event()

    class Flaky:
        def cleanup(self, now_wall):
            rounds.append(now_wall)
            if len(rounds) == 1:
                raise StoreError("marker went missing")
            if len(rounds) == 2:
                raise OSError("disk went away")
            finished.set()
            return []

    daemon.store = Flaky()

    async def go():
        task = asyncio.create_task(daemon._cleanup_loop())
        await asyncio.wait_for(finished.wait(), 5)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

    asyncio.run(go())
    assert len(rounds) == 3

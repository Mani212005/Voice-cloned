import json
import os
import stat

import pytest

from voice_clone.hfp.call import CallRecord, Outcome
from voice_clone.hfp.store import CallStore, StoreError, default_calls_dir

NOW = 1_800_000_000.0


def _touch(path, age):
    path.write_bytes(b"x")
    os.utime(path, (NOW - age, NOW - age))


def test_cleanup_deletes_only_files_older_than_24h(tmp_path):
    store = CallStore(tmp_path / "calls")
    store.ensure()
    old_wav = store.root / "1-1.wav"
    old_json = store.root / "1-1.json"
    fresh = store.root / "2-2.wav"
    edge = store.root / "3-3.wav"
    _touch(old_wav, 25 * 3600)
    _touch(old_json, 24 * 3600 + 1)
    _touch(fresh, 23 * 3600)
    _touch(edge, 24 * 3600)
    removed = store.cleanup(NOW)
    assert sorted(p.name for p in removed) == ["1-1.json", "1-1.wav", "3-3.wav"]
    assert fresh.exists()


def test_cleanup_never_touches_files_the_store_did_not_create(tmp_path):
    store = CallStore(tmp_path)
    mine = tmp_path / "1-1.wav.keep"
    other = tmp_path / "notes.txt"
    stale_tmp = tmp_path / "1-2.json.tmp"
    _touch(mine, 48 * 3600)
    _touch(other, 48 * 3600)
    _touch(stale_tmp, 48 * 3600)
    assert [p.name for p in store.cleanup(NOW)] == ["1-2.json.tmp"]
    assert mine.exists() and other.exists()


def test_claim_refuses_a_directory_with_other_files_and_keeps_its_mode(tmp_path):
    docs = tmp_path / "docs"
    docs.mkdir(mode=0o755)
    docs.chmod(0o755)
    (docs / "report.pdf").write_bytes(b"x")
    with pytest.raises(StoreError):
        CallStore(docs).claim()
    assert stat.S_IMODE(docs.stat().st_mode) == 0o755


def test_claim_accepts_a_new_empty_or_existing_store_directory(tmp_path):
    fresh = CallStore(tmp_path / "new")
    fresh.claim()
    assert stat.S_IMODE(fresh.root.stat().st_mode) == 0o700
    (fresh.root / "1-1.wav").write_bytes(b"x")
    fresh.claim()
    empty = tmp_path / "empty"
    empty.mkdir()
    CallStore(empty).claim()
    assert stat.S_IMODE(empty.stat().st_mode) == 0o700


def test_cleanup_on_missing_dir_is_a_no_op(tmp_path):
    assert CallStore(tmp_path / "nope").cleanup(NOW) == []


def test_store_is_private_and_records_are_json(tmp_path):
    store = CallStore(tmp_path / "calls")
    record = CallRecord(call_id="1-1", number="+91123", started_wall=NOW)
    record.outcome = Outcome.MESSAGE_TAKEN
    path = store.save_record(record, {"audio": "x.wav"})
    assert stat.S_IMODE(store.root.stat().st_mode) == 0o700
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    data = json.loads(path.read_text())
    assert data["number"] == "+91123"
    assert data["outcome"] == "message_taken"
    assert data["audio"] == "x.wav"


def test_default_dir_is_outside_any_repo(monkeypatch, tmp_path):
    monkeypatch.delenv("XDG_DATA_HOME", raising=False)
    monkeypatch.setenv("HOME", str(tmp_path))
    assert default_calls_dir() == tmp_path / ".local" / "share" / "voiceclone-calls"
    monkeypatch.setenv("XDG_DATA_HOME", str(tmp_path / "data"))
    assert default_calls_dir() == tmp_path / "data" / "voiceclone-calls"

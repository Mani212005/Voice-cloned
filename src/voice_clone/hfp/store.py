"""Private storage for call audio and call records, with 24 hour expiry.

Everything lives outside the repository, by default in
``~/.local/share/voiceclone-calls``, in a directory only this user can read.
"""

from __future__ import annotations

import json
import logging
import os
import re
from dataclasses import asdict
from pathlib import Path

from .call import CallRecord

log = logging.getLogger(__name__)

DEFAULT_MAX_AGE = 24 * 3600.0

_STORE_FILE = re.compile(r"\d+-\d+\.(?:wav|json|json\.tmp)")


class StoreError(ValueError):
    pass


def default_calls_dir() -> Path:
    base = os.environ.get("XDG_DATA_HOME") or str(Path.home() / ".local" / "share")
    return Path(base) / "voiceclone-calls"


class CallStore:
    def __init__(self, root: Path, max_age: float = DEFAULT_MAX_AGE) -> None:
        self.root = root
        self.max_age = max_age

    def ensure(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.root.chmod(0o700)

    def claim(self) -> None:
        """Create the private directory, refusing an existing one that holds other files."""
        if self.root.is_dir():
            foreign = [e.name for e in self.root.iterdir() if not _STORE_FILE.fullmatch(e.name)]
            if foreign:
                raise StoreError(
                    f"{self.root} already holds files that are not call records "
                    f"(for example {foreign[0]}); pick a dedicated directory"
                )
        self.ensure()

    def audio_path(self, call_id: str) -> Path:
        return self.root / f"{call_id}.wav"

    def record_path(self, call_id: str) -> Path:
        return self.root / f"{call_id}.json"

    def save_record(self, record: CallRecord, extra: dict[str, object] | None = None) -> Path:
        self.ensure()
        data = asdict(record)
        if extra:
            data.update(extra)
        path = self.record_path(record.call_id)
        tmp = path.with_suffix(".json.tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(data, fh, indent=2, default=str)
        tmp.replace(path)
        return path

    def cleanup(self, now_wall: float) -> list[Path]:
        """Delete the store's own files older than ``max_age``; returns what was removed."""
        removed: list[Path] = []
        if not self.root.is_dir():
            return removed
        for entry in self.root.iterdir():
            if not _STORE_FILE.fullmatch(entry.name) or not entry.is_file() or entry.is_symlink():
                continue
            try:
                age = now_wall - entry.stat().st_mtime
            except FileNotFoundError:
                continue
            if age >= self.max_age:
                try:
                    entry.unlink()
                    removed.append(entry)
                except FileNotFoundError:
                    pass
        if removed:
            log.info("deleted %d call files older than %.0f h", len(removed), self.max_age / 3600)
        return removed

"""Private storage for call audio and call records, with 24 hour expiry.

Everything lives outside the repository, by default in
``~/.local/share/voiceclone-calls``, in a directory only this user can read.

The expiry deleter is deliberately narrow. It only ever deletes a regular file that
(1) sits directly in the store directory, (2) has exactly the name the store gives its
own files (``<unix seconds>-<counter>.wav``, ``.json`` or ``.json.tmp``) and (3) lives in
a directory the store itself claimed, which is marked by ``MARKER``. It never follows
a symlink (not for the directory, not for a file), never descends into a subdirectory
and refuses a directory without the marker, such as a home folder named by mistake.
"""

from __future__ import annotations

import errno
import json
import logging
import os
import re
import stat
from dataclasses import asdict
from pathlib import Path

from .call import CallRecord

log = logging.getLogger(__name__)

DEFAULT_MAX_AGE = 24 * 3600.0

MARKER = ".voiceclone-store"
"""Present in a directory this store claimed; cleanup refuses directories without it."""
_MARKER_TEXT = b"voice-clone call store: files named <seconds>-<n>.wav/.json here expire\n"

# call_id is "<unix seconds>-<counter>" (see CallMachine). ASCII digits only: \d would
# also match digits of other scripts, and the seconds are ten digits until the year 2286.
_STORE_FILE = re.compile(r"[0-9]{10}-[0-9]+\.(?:wav|json|json\.tmp)", re.ASCII)

_DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC


class StoreError(ValueError):
    pass


def default_calls_dir() -> Path:
    base = os.environ.get("XDG_DATA_HOME") or str(Path.home() / ".local" / "share")
    return Path(base) / "voiceclone-calls"


def _is_store_file(name: str) -> bool:
    return _STORE_FILE.fullmatch(name) is not None


class CallStore:
    def __init__(self, root: Path, max_age: float = DEFAULT_MAX_AGE) -> None:
        self.root = root
        self.max_age = max_age

    # ----- the directory ------------------------------------------------------------

    def _open_dir(self) -> int:
        """Open the store directory itself, refusing a symlink or a non-directory."""
        try:
            return os.open(self.root, _DIR_FLAGS)
        except OSError as exc:
            if exc.errno in (errno.ELOOP, errno.ENOTDIR):
                raise StoreError(
                    f"{self.root} is a symlink or not a directory; pick a real, dedicated directory"
                ) from exc
            raise

    @staticmethod
    def _has_marker(dir_fd: int) -> bool:
        try:
            info = os.stat(MARKER, dir_fd=dir_fd, follow_symlinks=False)
        except FileNotFoundError:
            return False
        return stat.S_ISREG(info.st_mode)

    def claim(self) -> None:
        """Create or adopt the private directory.

        A new or empty directory, or one holding only this store's own files, is claimed
        (made owner-only and marked). An existing directory with anything else in it is
        refused untouched: its permissions stay as they are and nothing in it is ever deleted.
        """
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        dir_fd = self._open_dir()
        try:
            if not self._has_marker(dir_fd):
                with os.scandir(dir_fd) as entries:
                    foreign = [
                        e.name for e in entries if e.name != MARKER and not _is_store_file(e.name)
                    ]
                if foreign:
                    raise StoreError(
                        f"{self.root} already holds files that are not call records "
                        f"(for example {foreign[0]}); pick a dedicated directory"
                    )
                self._write_marker(dir_fd)
            os.fchmod(dir_fd, 0o700)
        finally:
            os.close(dir_fd)

    def ensure(self) -> None:
        """Make sure the directory is a claimed store before anything is written to it."""
        self.claim()

    @staticmethod
    def _write_marker(dir_fd: int) -> None:
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC
        try:
            fd = os.open(MARKER, flags, 0o600, dir_fd=dir_fd)
        except FileExistsError as exc:  # a symlink or directory squatting on the name
            raise StoreError(f"{MARKER} exists but is not a regular file") from exc
        with os.fdopen(fd, "wb") as fh:
            fh.write(_MARKER_TEXT)

    # ----- files --------------------------------------------------------------------

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
        """Delete the store's own files older than ``max_age``; returns what was removed.

        Read-only towards everything else: a directory that is missing is a no-op, one
        that is not a claimed store (or is a symlink) raises ``StoreError``.
        """
        removed: list[Path] = []
        try:
            dir_fd = self._open_dir()
        except FileNotFoundError:
            return removed
        try:
            if not self._has_marker(dir_fd):
                raise StoreError(
                    f"{self.root} is not a call store (no {MARKER} marker); "
                    "refusing to delete anything from it"
                )
            with os.scandir(dir_fd) as entries:
                for entry in entries:
                    if not _is_store_file(entry.name):
                        continue
                    if self._expire(dir_fd, entry.name, now_wall):
                        removed.append(self.root / entry.name)
        finally:
            os.close(dir_fd)
        if removed:
            log.info("deleted %d call files older than %.0f h", len(removed), self.max_age / 3600)
        return removed

    def _expire(self, dir_fd: int, name: str, now_wall: float) -> bool:
        try:
            info = os.stat(name, dir_fd=dir_fd, follow_symlinks=False)
            if not stat.S_ISREG(info.st_mode) or now_wall - info.st_mtime < self.max_age:
                return False
            os.unlink(name, dir_fd=dir_fd)  # never follows: it removes the name itself
        except FileNotFoundError:
            return False
        return True

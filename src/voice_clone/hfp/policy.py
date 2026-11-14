"""Per-number answering policy.

File format, one rule per line, ``#`` starts a comment::

    # number          action
    *                 ai        # default for every caller
    withheld          ring      # callers with no caller ID
    +91 98765 43210   deny

Actions:

``ai``
    Let the phone ring for the configured delay, then answer with the AI.
``ring``
    Never answer; the phone just rings as if the daemon were not there.
``deny``
    Reject the call as soon as the caller number is known.

Numbers are compared after removing spaces, dashes, dots and parentheses. A rule
without a leading ``+`` also matches a number that ends with it, so ``9876543210``
matches ``+919876543210``. Without any matching rule the action is ``ai``.
"""

from __future__ import annotations

import enum
import re
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import Path


class Action(enum.StrEnum):
    AI = "ai"
    RING = "ring"
    DENY = "deny"


DEFAULT_ACTION = Action.AI
WILDCARD = "*"
WITHHELD = "withheld"

_SEPARATORS = re.compile(r"[\s\-.()]")


class PolicyError(ValueError):
    pass


def normalize_number(number: str) -> str:
    return _SEPARATORS.sub("", number)


@dataclass(frozen=True)
class Policy:
    rules: dict[str, Action]
    default: Action = DEFAULT_ACTION
    withheld: Action | None = None

    def decide(self, number: str | None) -> Action:
        if number is None or normalize_number(number) == "":
            return self.withheld if self.withheld is not None else self.default
        wanted = normalize_number(number)
        exact = self.rules.get(wanted)
        if exact is not None:
            return exact
        # Longest suffix rule wins, so a local number matches its international form.
        best: tuple[int, Action] | None = None
        for pattern, action in self.rules.items():
            if (
                not pattern.startswith("+")
                and wanted.endswith(pattern)
                and (best is None or len(pattern) > best[0])
            ):
                best = (len(pattern), action)
        return best[1] if best else self.default


def parse_policy(lines: Iterable[str]) -> Policy:
    rules: dict[str, Action] = {}
    default = DEFAULT_ACTION
    withheld: Action | None = None
    for lineno, raw in enumerate(lines, start=1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) < 2:
            raise PolicyError(f"line {lineno}: expected '<number> <action>'")
        action_text = parts[-1].lower()
        key = "".join(parts[:-1])
        try:
            action = Action(action_text)
        except ValueError:
            raise PolicyError(
                f"line {lineno}: unknown action {action_text!r}, use ai, ring or deny"
            ) from None
        if key == WILDCARD:
            default = action
        elif key.lower() == WITHHELD:
            withheld = action
        else:
            number = normalize_number(key)
            if not re.fullmatch(r"\+?[0-9*#]+", number):
                raise PolicyError(f"line {lineno}: {key!r} is not a phone number")
            rules[number] = action
    return Policy(rules=rules, default=default, withheld=withheld)


class PolicyStore:
    """Reads the policy file on every decision, so edits apply without a restart."""

    def __init__(self, path: Path | None) -> None:
        self.path = path

    def load(self) -> Policy:
        if self.path is None or not self.path.exists():
            return Policy(rules={})
        return parse_policy(self.path.read_text(encoding="utf-8").splitlines())

    def decide(self, number: str | None) -> Action:
        return self.load().decide(number)

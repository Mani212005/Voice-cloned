"""Voice clone command line."""

import argparse
import asyncio
import logging
import os
import sys
from pathlib import Path


def _default_policy_path() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "voiceclone" / "policy.txt"


def _parser() -> argparse.ArgumentParser:
    from .hfp.store import default_calls_dir

    parser = argparse.ArgumentParser(prog="voice-clone")
    sub = parser.add_subparsers(dest="command", required=True)

    hfp = sub.add_parser("hfp", help="Bluetooth hands-free call answerer for your own phone")
    hfp_sub = hfp.add_subparsers(dest="hfp_command", required=True)

    run = hfp_sub.add_parser("run", help="run the hands-free daemon")
    run.add_argument("--adapter", default="hci0")
    run.add_argument(
        "--answer-delay", type=float, default=20.0, help="seconds of ringing before the AI answers"
    )
    run.add_argument(
        "--max-message", type=float, default=30.0, help="seconds of caller message to record"
    )
    run.add_argument(
        "--policy",
        type=Path,
        default=_default_policy_path(),
        help="per-number policy file (default: %(default)s; missing means AI for everyone)",
    )
    run.add_argument(
        "--calls-dir",
        type=Path,
        default=default_calls_dir(),
        help="private directory for call audio and records (default: %(default)s)",
    )
    run.add_argument("--intro", type=Path, help="intro WAV (default: placeholder tones)")
    run.add_argument("--beep", type=Path, help="record-now beep WAV (default: placeholder tone)")
    run.add_argument(
        "--wideband", action="store_true", help="offer mSBC 16 kHz (needs libsbc and phone support)"
    )
    run.add_argument(
        "--observe",
        action="store_true",
        help="never answer: only log the hands-free conversation (test plan step 2)",
    )
    run.add_argument("-v", "--verbose", action="store_true", help="log every AT line")

    doctor = hfp_sub.add_parser("doctor", help="read-only checks of Bluetooth readiness")
    doctor.add_argument("--adapter", default="hci0")

    cleanup = hfp_sub.add_parser("cleanup", help="delete call files older than 24 hours")
    cleanup.add_argument("--calls-dir", type=Path, default=default_calls_dir())
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    logging.basicConfig(
        level=logging.DEBUG if getattr(args, "verbose", False) else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    if args.hfp_command == "doctor":
        from .hfp import doctor

        return doctor.run(args.adapter)
    if args.hfp_command == "cleanup":
        import time

        from .hfp.store import CallStore, StoreError

        try:
            removed = CallStore(args.calls_dir).cleanup(time.time())
        except StoreError as exc:
            print(exc, file=sys.stderr)
            return 2
        print(f"deleted {len(removed)} file(s) from {args.calls_dir}")
        return 0

    from .hfp.bluez import Daemon, DaemonConfig
    from .hfp.connection import Settings
    from .hfp.policy import PolicyError, PolicyStore
    from .hfp.store import CallStore, StoreError

    try:
        PolicyStore(args.policy).load()
    except PolicyError as exc:
        print(f"{args.policy}: {exc}", file=sys.stderr)
        return 2
    try:
        CallStore(args.calls_dir).claim()
    except StoreError as exc:
        print(exc, file=sys.stderr)
        return 2
    config = DaemonConfig(
        settings=Settings(
            answer_delay=args.answer_delay,
            max_message=args.max_message,
            wideband=args.wideband,
            intro_wav=args.intro,
            beep_wav=args.beep,
        ),
        policy_path=args.policy,
        calls_dir=args.calls_dir,
        adapter=args.adapter,
        observe=args.observe,
    )
    asyncio.run(Daemon(config).run())
    return 0


if __name__ == "__main__":
    sys.exit(main())

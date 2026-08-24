#!/usr/bin/env python3
"""Native Herdr plugin bootstrap and entrypoint wrapper.

Imports durable state from the standalone ~/.herdr-coordinator directory once,
then replaces this process with the requested coordinator entrypoint.
"""

from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time


ROOT = Path(os.environ.get("HERDR_PLUGIN_ROOT") or Path(__file__).resolve().parents[1])
STATE = Path(
    os.environ.get("HERDR_PLUGIN_STATE_DIR")
    or os.environ.get("HERDR_COORDINATOR_HOME")
    or Path.home() / ".herdr-coordinator"
).expanduser()
LEGACY = Path(
    os.environ.get("HERDR_COORDINATOR_HOME") or Path.home() / ".herdr-coordinator"
).expanduser()
MARKER = STATE / "legacy-import.json"
LOCK = STATE / "plugin-bootstrap.lock"
DURABLE_FILES = (
    "fleets.json",
    "claims.json",
    "decisions.json",
    "attention.json",
    "inbox.jsonl",
    "inbox.cursor",
    "dispatch.json",
    "summaries.json",
)


def atomic_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".plugin-bootstrap-", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(value, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def import_legacy_state() -> list[str]:
    """Import standalone durable state once; never copy stale runtime/PID data."""
    if not os.environ.get("HERDR_PLUGIN_STATE_DIR"):
        return []
    if STATE.resolve() == LEGACY.resolve() or not LEGACY.is_dir():
        return []
    STATE.mkdir(parents=True, exist_ok=True)
    with LOCK.open("a+", encoding="utf-8") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        if MARKER.exists():
            return []
        imported: list[str] = []
        for name in DURABLE_FILES:
            source = LEGACY / name
            target = STATE / name
            if not source.is_file() or target.exists():
                continue
            fd, temporary = tempfile.mkstemp(prefix=f".import-{name}-", dir=STATE)
            os.close(fd)
            try:
                shutil.copyfile(source, temporary)
                with open(temporary, "rb") as handle:
                    os.fsync(handle.fileno())
                os.replace(temporary, target)
                imported.append(name)
            finally:
                try:
                    os.unlink(temporary)
                except FileNotFoundError:
                    pass
        atomic_json(
            MARKER,
            {
                "source": str(LEGACY.resolve()),
                "imported": imported,
                "imported_at_unix_ms": int(time.time() * 1000),
            },
        )
        return imported


def main() -> None:
    if len(sys.argv) != 2 or sys.argv[1] not in {"reconcile", "event", "tower"}:
        raise SystemExit("usage: plugin_runtime.py reconcile|event|tower")
    import_legacy_state()
    mode = sys.argv[1]
    if mode == "tower":
        target = ROOT / "tower" / "plugin_tower.py"
        argv = [sys.executable, str(target)]
    else:
        target = ROOT / "fleet"
        command = "plugin-reconcile" if mode == "reconcile" else "plugin-event"
        argv = [sys.executable, str(target), command]
    os.execv(sys.executable, argv)


if __name__ == "__main__":
    main()

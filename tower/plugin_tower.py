#!/usr/bin/env python3
"""Curses control tower used by the native Herdr plugin pane."""

from __future__ import annotations

import curses
import json
import os
from pathlib import Path
import subprocess
import time
import unicodedata
from typing import Any


PLUGIN_ROOT = Path(os.environ.get("HERDR_PLUGIN_ROOT") or Path(__file__).resolve().parents[1])
FLEET = PLUGIN_ROOT / "fleet"
DATA_DIR = Path(
    os.environ.get("HERDR_PLUGIN_STATE_DIR")
    or os.environ.get("HERDR_COORDINATOR_HOME")
    or Path.home() / ".herdr-coordinator"
)


def load_json(name: str, default: Any) -> Any:
    try:
        return json.loads((DATA_DIR / name).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return default


def run_fleet(*args: str, timeout: float = 15) -> tuple[bool, str]:
    try:
        result = subprocess.run(
            [str(FLEET), *args],
            capture_output=True,
            text=True,
            timeout=timeout,
            env=os.environ.copy(),
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, str(exc)
    text = (result.stdout or result.stderr).strip()
    return result.returncode == 0, text


def char_width(ch: str) -> int:
    if not ch or ord(ch) < 32 or ord(ch) == 0xFE0F:
        return 0
    return 2 if unicodedata.east_asian_width(ch) in {"W", "F"} else 1


def truncate(value: str, width: int) -> str:
    if width <= 0:
        return ""
    used = 0
    output: list[str] = []
    for ch in value:
        size = char_width(ch)
        if used + size > width:
            if width >= 1 and output:
                output[-1] = "…"
            elif width >= 1:
                output.append("…")
            break
        output.append(ch)
        used += size
    return "".join(output)


def add(stdscr, row: int, col: int, text: str, attr: int = 0) -> None:
    height, width = stdscr.getmaxyx()
    if row < 0 or row >= height or col >= width:
        return
    try:
        stdscr.addstr(row, col, truncate(text, max(0, width - col - 1)), attr)
    except curses.error:
        pass


def age(value: str | None) -> str:
    if not value:
        return ""
    try:
        parsed = time.mktime(time.strptime(value, "%Y-%m-%d %H:%M:%S"))
    except ValueError:
        return ""
    seconds = max(0, int(time.time() - parsed))
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m"
    if seconds < 86400:
        return f"{seconds // 3600}h"
    return f"{seconds // 86400}d"


def init_colors() -> dict[str, int]:
    colors = {name: curses.A_NORMAL for name in ("title", "good", "warn", "bad", "dim", "focus")}
    if not curses.has_colors():
        return colors
    curses.start_color()
    curses.use_default_colors()
    pairs = {
        "title": (curses.COLOR_CYAN, -1),
        "good": (curses.COLOR_GREEN, -1),
        "warn": (curses.COLOR_YELLOW, -1),
        "bad": (curses.COLOR_RED, -1),
        "dim": (curses.COLOR_WHITE, -1),
        "focus": (curses.COLOR_BLACK, curses.COLOR_CYAN),
    }
    for index, (name, (fg, bg)) in enumerate(pairs.items(), start=1):
        try:
            curses.init_pair(index, fg, bg)
            colors[name] = curses.color_pair(index)
        except curses.error:
            pass
    return colors


def status_attr(status: str, colors: dict[str, int]) -> int:
    if status in {"done", "idle", "accepted"}:
        return colors["good"]
    if status in {"need_decision", "verified", "blocked"}:
        return colors["warn"]
    if status in {"offline", "reported"}:
        return colors["bad"]
    return curses.A_NORMAL


def draw(stdscr, colors: dict[str, int], message: str) -> None:
    stdscr.erase()
    height, width = stdscr.getmaxyx()
    runtime = load_json("runtime.json", {})
    registry = load_json("fleets.json", {})
    attention = load_json("attention.json", [])
    inbox_lines: list[dict[str, Any]] = []
    try:
        for line in (DATA_DIR / "inbox.jsonl").read_text(encoding="utf-8").splitlines():
            try:
                inbox_lines.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    except OSError:
        pass

    connected = bool(runtime.get("connected"))
    projects_runtime = runtime.get("projects", {}) if isinstance(runtime, dict) else {}
    agents = runtime.get("agents", []) if connected else []
    working = sum(1 for agent in agents if agent.get("agent_status") == "working")
    blocked = sum(1 for agent in agents if agent.get("agent_status") == "blocked")

    header = (
        f"HERDR COORDINATOR  {time.strftime('%H:%M:%S')}   "
        f"projects {len(registry)}   agents {len(agents)}   working {working}   blocked {blocked}"
    )
    add(stdscr, 0, 0, header, colors["title"] | curses.A_BOLD)
    connection = "connected" if connected else f"disconnected: {runtime.get('error') or 'not reconciled'}"
    add(stdscr, 1, 0, f"runtime {connection}   state {DATA_DIR}", colors["good"] if connected else colors["bad"])

    row = 3
    add(stdscr, row, 0, f"ATTENTION  {len(attention)}", colors["warn"] | curses.A_BOLD)
    row += 1
    if not attention:
        add(stdscr, row, 2, "No human intervention required.", colors["dim"])
        row += 1
    else:
        max_attention = max(1, min(8, (height - 12) // 2))
        for item in attention[:max_attention]:
            priority = item.get("priority", 0)
            actor = item.get("required_actor", "?")
            line = (
                f"P{priority:<3} {item.get('project','?'):<14} "
                f"{item.get('kind','?')} · {item.get('reason','')} · → {actor}"
            )
            add(stdscr, row, 2, line, colors["bad"] if priority >= 88 else colors["warn"])
            row += 1
            add(stdscr, row, 6, str(item.get("action", "")), colors["dim"])
            row += 1

    if row < height - 7:
        row += 1
        add(stdscr, row, 0, "PROJECTS", colors["title"] | curses.A_BOLD)
        row += 1
        for project, entry in sorted(registry.items()):
            if row >= height - 6:
                break
            live = projects_runtime.get(project, {}) if isinstance(projects_runtime, dict) else {}
            business = entry.get("status", "idle")
            claim_state = entry.get("claim_state") or "-"
            live_status = live.get("live_status", "unknown") if connected else "unknown"
            line = (
                f"{project:<16} business={business:<14} live={live_status:<9} "
                f"claim={claim_state:<9} commander={entry.get('commander','?')} "
                f"{age(entry.get('updated_at'))}  {entry.get('note','')}"
            )
            add(stdscr, row, 2, line, status_attr(business, colors))
            row += 1

    if row < height - 4:
        row += 1
        add(stdscr, row, 0, "LATEST EVENTS", colors["title"] | curses.A_BOLD)
        row += 1
        for event in reversed(inbox_lines[-3:]):
            if row >= height - 2:
                break
            add(
                stdscr,
                row,
                2,
                f"{event.get('ts','')[11:16]} {event.get('project','?')} · "
                f"{event.get('type','event')} · {event.get('summary','')}",
                colors["dim"],
            )
            row += 1

    footer = "q quit   r reconcile snapshot   a rebuild attention   auto-refresh 1s"
    if message:
        footer += f"   | {message}"
    add(stdscr, height - 1, 0, footer, colors["dim"])
    stdscr.refresh()


def main(stdscr) -> None:
    curses.curs_set(0)
    stdscr.nodelay(True)
    stdscr.timeout(250)
    colors = init_colors()
    ok, text = run_fleet("plugin-reconcile")
    message = text if not ok else ""
    last_refresh = 0.0
    while True:
        if time.monotonic() - last_refresh >= 1:
            draw(stdscr, colors, message)
            last_refresh = time.monotonic()
        key = stdscr.getch()
        if key in (ord("q"), 27):
            return
        if key == ord("r"):
            ok, text = run_fleet("plugin-reconcile")
            message = text if not ok else "reconciled"
            last_refresh = 0
        elif key == ord("a"):
            ok, text = run_fleet("attention", "--json")
            message = "attention rebuilt" if ok else text
            last_refresh = 0
        elif key == curses.KEY_RESIZE:
            last_refresh = 0


if __name__ == "__main__":
    curses.wrapper(main)

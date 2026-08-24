#!/usr/bin/env python3
"""Herdr Coordinator 的注册表、可信状态协议与注意力队列。

用法:
  fleet init
  fleet register <项目> --tab <tab_id> --commander <机长名> --cwd <路径>
  fleet set-status <项目> <状态> [备注]
  fleet unregister <项目>
  fleet list
  fleet route <项目> <给机长的指令>
  fleet sync
  fleet report <项目> <状态> <一行摘要> [--confidence <0..1>] [--evidence <类型:内容>]...
  fleet claims [项目] [--all] [--json]
  fleet verify <项目> [--claim <claim_id>] [--label <标签>] -- <命令> [参数...]
  fleet accept <项目> [--claim <claim_id>] [--note <说明>] [--force]
  fleet ask <项目> <问题> [--option <选项>]...
  fleet decisions [--all]
  fleet resolve <决策编号> <答案>
  fleet attention [--json]
  fleet inbox [--all]
  fleet plugin-reconcile
  fleet plugin-event
  fleet watch
  fleet watch-start | watch-status | watch-stop

状态: working|idle|done|blocked|need_decision

事实边界:
  - observed: Herdr 事件/快照只更新 runtime.json，不改写项目业务状态。
  - claimed: 机长 fleet report 产生 claims.json 中的声明。
  - verified: fleet verify 由塔台进程实际执行命令并保存机器证据。
  - governed: fleet ask/resolve 与 fleet accept 保存人的决策和验收。
  - fleet 不拆任务、不管理 worktree、不测试或合并代码；verify 只执行显式命令并记录结果。
"""

from __future__ import annotations

import contextlib
import datetime as dt
import fcntl
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from typing import Any, Iterable


DATA_DIR = (
    os.environ.get("HERDR_PLUGIN_STATE_DIR")
    or os.environ.get("HERDR_COORDINATOR_HOME")
    or os.path.expanduser("~/.herdr-coordinator")
)
HERDR_BIN = os.environ.get("HERDR_BIN_PATH", "herdr")
REG = os.path.join(DATA_DIR, "fleets.json")
INBOX = os.path.join(DATA_DIR, "inbox.jsonl")
CURSOR = os.path.join(DATA_DIR, "inbox.cursor")
DECISIONS = os.path.join(DATA_DIR, "decisions.json")
CLAIMS = os.path.join(DATA_DIR, "claims.json")
RUNTIME = os.path.join(DATA_DIR, "runtime.json")
ATTENTION = os.path.join(DATA_DIR, "attention.json")
WATCH_PID = os.path.join(DATA_DIR, "watch.pid")
WATCH_LOG = os.path.join(DATA_DIR, "watch.log")
LOCK = os.path.join(DATA_DIR, "state.lock")

STATUSES = {"working", "idle", "done", "blocked", "need_decision"}
REPORTED_STATUSES = STATUSES - {"need_decision"}
CLAIM_STATES = {"reported", "verified", "accepted"}
TOPOLOGY_EVENTS = {
    "workspace.created",
    "workspace.closed",
    "tab.created",
    "tab.closed",
    "pane.created",
    "pane.closed",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
}
EVENT_ALIASES = {
    event.replace(".", "_"): event
    for event in TOPOLOGY_EVENTS | {"pane.agent_status_changed"}
}


class ReloadWatch(Exception):
    pass


class StopWatch(Exception):
    pass


def now() -> str:
    return dt.datetime.now().strftime("%Y-%m-%d %H:%M:%S")


def utc_ms() -> int:
    return int(time.time() * 1000)


def parse_time(value: str | None) -> dt.datetime | None:
    if not value:
        return None
    for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%z"):
        try:
            return dt.datetime.strptime(value, fmt).replace(tzinfo=None)
        except ValueError:
            pass
    return None


def age_seconds(value: str | None) -> int:
    parsed = parse_time(value)
    if parsed is None:
        return 0
    return max(0, int((dt.datetime.now() - parsed).total_seconds()))


def ensure_data_dir() -> None:
    os.makedirs(DATA_DIR, exist_ok=True)


def load_json(path: str, default: Any) -> Any:
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return default


def atomic_json(path: str, value: Any) -> None:
    ensure_data_dir()
    fd, temp_path = tempfile.mkstemp(prefix=".fleet-", dir=DATA_DIR, text=True)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(value, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_path, path)
    finally:
        if os.path.exists(temp_path):
            os.unlink(temp_path)


@contextlib.contextmanager
def state_lock():
    ensure_data_dir()
    with open(LOCK, "a+", encoding="utf-8") as handle:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def load_reg() -> dict[str, dict[str, Any]]:
    reg = load_json(REG, {})
    if not isinstance(reg, dict):
        return {}
    for entry in reg.values():
        if not isinstance(entry, dict):
            continue
        status = entry.get("status", "idle")
        entry.setdefault(
            "reported_status", status if status in REPORTED_STATUSES else "idle"
        )
        state = entry.get("claim_state")
        if state not in CLAIM_STATES:
            entry["claim_state"] = None
    return reg


def load_decisions() -> list[dict[str, Any]]:
    values = load_json(DECISIONS, [])
    return values if isinstance(values, list) else []


def load_claims() -> list[dict[str, Any]]:
    values = load_json(CLAIMS, [])
    return values if isinstance(values, list) else []


def open_decisions(
    decisions: Iterable[dict[str, Any]], project: str | None = None
) -> list[dict[str, Any]]:
    return [
        item
        for item in decisions
        if item.get("state") == "open"
        and (project is None or item.get("project") == project)
    ]


def latest_claim(
    claims: Iterable[dict[str, Any]], project: str, claim_id: str | None = None
) -> dict[str, Any] | None:
    values = [item for item in claims if item.get("project") == project]
    if claim_id:
        return next((item for item in values if item.get("id") == claim_id), None)
    return values[-1] if values else None


def effective_status(
    entry: dict[str, Any], decisions: Iterable[dict[str, Any]], project: str
) -> str:
    if open_decisions(decisions, project):
        return "need_decision"
    status = entry.get("reported_status", entry.get("status", "idle"))
    return status if status in REPORTED_STATUSES else "idle"


def append_inbox(entry: dict[str, Any]) -> None:
    ensure_data_dir()
    with open(INBOX, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(entry, ensure_ascii=False) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def require_project(reg: dict[str, dict[str, Any]], name: str) -> dict[str, Any]:
    if name not in reg:
        raise SystemExit(f"未注册的项目: {name}（先 fleet register）")
    return reg[name]


def migrate_legacy_state() -> None:
    """把旧版仅存在注册表中的 need_decision 提升为正式决策。"""
    ensure_data_dir()
    with state_lock():
        reg = load_json(REG, {})
        decisions = load_json(DECISIONS, [])
        claims = load_json(CLAIMS, [])
        if not isinstance(reg, dict):
            reg = {}
        if not isinstance(decisions, list):
            decisions = []
        if not isinstance(claims, list):
            claims = []
        changed = False
        for project, entry in reg.items():
            if not isinstance(entry, dict):
                continue
            status = entry.get("status", "idle")
            entry.setdefault(
                "reported_status", status if status in REPORTED_STATUSES else "idle"
            )
            entry.setdefault("claim_id", None)
            entry.setdefault("claim_state", None)
            if status != "need_decision":
                continue
            if any(
                item.get("project") == project and item.get("state") == "open"
                for item in decisions
            ):
                continue
            created = entry.get("updated_at") or now()
            decisions.append(
                {
                    "id": f"{project}-legacy-{uuid.uuid4().hex[:8]}",
                    "project": project,
                    "state": "open",
                    "question": entry.get("note") or "需要用户拍板",
                    "options": [],
                    "source": "legacy_registry_migration",
                    "created_at": created,
                    "resolved_at": None,
                    "resolution": None,
                }
            )
            changed = True
        if changed or not os.path.exists(DECISIONS):
            atomic_json(DECISIONS, decisions)
        if not os.path.exists(CLAIMS):
            atomic_json(CLAIMS, claims)
        if changed or (reg and not os.path.exists(REG)):
            atomic_json(REG, reg)


def init_state_files() -> None:
    ensure_data_dir()
    with state_lock():
        for path, default in (
            (REG, {}),
            (DECISIONS, []),
            (CLAIMS, []),
            (ATTENTION, []),
        ):
            if not os.path.exists(path):
                atomic_json(path, default)
        open(INBOX, "a", encoding="utf-8").close()
        if not os.path.exists(CURSOR):
            with open(CURSOR, "w", encoding="utf-8") as handle:
                handle.write("0")


def cmd_init(_args: list[str]) -> None:
    init_state_files()
    print(f"ok: {DATA_DIR}")


def option_map(args: list[str]) -> dict[str, str]:
    if len(args) % 2:
        raise SystemExit(__doc__)
    return dict(zip(args[::2], args[1::2]))


def cmd_register(args: list[str]) -> None:
    if len(args) < 7:
        raise SystemExit(__doc__)
    name, opts = args[0], option_map(args[1:])
    required = ("--tab", "--commander", "--cwd")
    if any(not opts.get(key) for key in required):
        raise SystemExit(__doc__)
    with state_lock():
        reg = load_reg()
        entry = reg.get(name, {})
        entry.update(
            {
                "tab_id": opts["--tab"],
                "commander": opts["--commander"],
                "cwd": os.path.abspath(os.path.expanduser(opts["--cwd"])),
                "status": entry.get("status", "idle"),
                "reported_status": entry.get("reported_status", "idle"),
                "note": entry.get("note", ""),
                "claim_id": entry.get("claim_id"),
                "claim_state": entry.get("claim_state"),
                "updated_at": now(),
            }
        )
        reg[name] = entry
        atomic_json(REG, reg)
    notify_watcher()
    refresh_attention()
    print(f"registered: {name}（机长 {entry['commander']}）")


def cmd_set_status(args: list[str]) -> None:
    if len(args) < 2:
        raise SystemExit(__doc__)
    name, status = args[0], args[1]
    note = " ".join(args[2:]).strip()
    if status not in STATUSES:
        raise SystemExit(f"状态必须是: {'/'.join(sorted(STATUSES))}")
    if status == "need_decision":
        if not note:
            raise SystemExit("need_decision 必须提供需要拍板的问题")
        return cmd_ask([name, note])
    with state_lock():
        reg, decisions = load_reg(), load_decisions()
        entry = require_project(reg, name)
        entry["reported_status"] = status
        entry["status"] = "need_decision" if open_decisions(decisions, name) else status
        if note:
            entry["note"] = note
        entry["updated_at"] = now()
        atomic_json(REG, reg)
    refresh_attention()
    print(f"ok: {name} -> {status}")


def cmd_unregister(args: list[str]) -> None:
    if len(args) != 1:
        raise SystemExit(__doc__)
    with state_lock():
        reg = load_reg()
        if reg.pop(args[0], None) is None:
            raise SystemExit(f"未注册的项目: {args[0]}")
        atomic_json(REG, reg)
    notify_watcher()
    refresh_attention()
    print(f"removed: {args[0]}")


# ── Herdr runtime projection ────────────────────────────────────────────────

def runtime_agents(snapshot: dict[str, Any]) -> list[dict[str, Any]]:
    values = snapshot.get("agents", []) if isinstance(snapshot, dict) else []
    return values if isinstance(values, list) else []


def build_runtime(
    snapshot: dict[str, Any],
    connected: bool = True,
    error: str | None = None,
    *,
    source: str = "snapshot",
    last_event: dict[str, Any] | None = None,
) -> dict[str, Any]:
    reg = load_reg()
    previous = load_json(RUNTIME, {})
    previous_projects = previous.get("projects", {}) if isinstance(previous, dict) else {}
    agents = runtime_agents(snapshot)
    by_name = {a.get("name"): a for a in agents if a.get("name")}
    observed = now()
    projects: dict[str, Any] = {}
    for project, entry in reg.items():
        commander = entry.get("commander", "")
        agent = by_name.get(commander)
        live_status = agent.get("agent_status", "unknown") if agent else "offline"
        pane_id = agent.get("pane_id") if agent else None
        old = previous_projects.get(project, {}) if isinstance(previous_projects, dict) else {}
        unchanged = (
            old.get("live_status") == live_status
            and old.get("pane_id") == pane_id
            and bool(old.get("online")) == bool(agent)
        )
        status_since = old.get("live_status_since") if unchanged else observed
        projects[project] = {
            "commander": commander,
            "online": bool(agent),
            "live_status": live_status,
            "live_status_since": status_since or observed,
            "pane_id": pane_id,
            "workspace_id": agent.get("workspace_id") if agent else None,
            "state_labels": agent.get("state_labels", {}) if agent else {},
            "observed_at": observed,
        }
    return {
        "connected": connected,
        "error": error,
        "source": source,
        "updated_at": observed,
        "herdr_version": snapshot.get("version") if isinstance(snapshot, dict) else None,
        "protocol": snapshot.get("protocol") if isinstance(snapshot, dict) else None,
        "last_event": last_event,
        "agents": agents,
        "projects": projects,
    }


def write_runtime(
    snapshot: dict[str, Any],
    connected: bool = True,
    error: str | None = None,
    *,
    source: str = "snapshot",
    last_event: dict[str, Any] | None = None,
) -> dict[str, Any]:
    runtime = build_runtime(
        snapshot,
        connected,
        error,
        source=source,
        last_event=last_event,
    )
    atomic_json(RUNTIME, runtime)
    return runtime


def parse_snapshot_payload(stdout: str) -> dict[str, Any]:
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"无法解析 Herdr 快照: {exc}") from exc
    if isinstance(payload, dict):
        result = payload.get("result", payload)
        if isinstance(result, dict):
            snapshot = result.get("snapshot", result)
            if isinstance(snapshot, dict):
                return snapshot
    raise SystemExit("Herdr 未返回有效 snapshot")


def snapshot_from_cli() -> dict[str, Any]:
    result = subprocess.run(
        [HERDR_BIN, "api", "snapshot"], capture_output=True, text=True
    )
    if result.returncode != 0:
        raise SystemExit(result.stderr.strip() or "herdr api snapshot 失败")
    return parse_snapshot_payload(result.stdout)


def cmd_sync(_args: list[str]) -> None:
    runtime = write_runtime(snapshot_from_cli(), source="manual_sync")
    refresh_attention()
    print(f"已从 Herdr 快照恢复 {len(runtime['agents'])} 个 Agent 的实时基线")


def plugin_event_payload() -> dict[str, Any] | None:
    raw = os.environ.get("HERDR_PLUGIN_EVENT_JSON", "").strip()
    if not raw:
        return None
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"HERDR_PLUGIN_EVENT_JSON 无效: {exc}") from exc
    return value if isinstance(value, dict) else None


def cmd_plugin_reconcile(_args: list[str]) -> None:
    init_state_files()
    snapshot = snapshot_from_cli()
    runtime = write_runtime(snapshot, source="plugin_startup")
    refresh_attention()
    print(f"plugin reconciled: {len(runtime['agents'])} agents")


def cmd_plugin_event(_args: list[str]) -> None:
    init_state_files()
    event = plugin_event_payload()
    event_name_value = os.environ.get("HERDR_PLUGIN_EVENT")
    if event is not None and not event_name_value:
        raw_name = event.get("event")
        event_name_value = str(raw_name) if raw_name else None
    # Event hooks may run concurrently and can be observed out of order. Never apply
    # the event payload as authoritative state; serialize a fresh snapshot instead.
    with state_lock():
        try:
            snapshot = snapshot_from_cli()
            runtime = write_runtime(
                snapshot,
                source="plugin_event",
                last_event={"name": event_name_value, "envelope": event},
            )
        except SystemExit as exc:
            previous = load_json(RUNTIME, {})
            snapshot = {
                "agents": previous.get("agents", []),
                "version": previous.get("herdr_version"),
                "protocol": previous.get("protocol"),
            }
            runtime = write_runtime(
                snapshot,
                connected=False,
                error=str(exc),
                source="plugin_event",
                last_event={"name": event_name_value, "envelope": event},
            )
    refresh_attention()
    print(
        f"plugin event reconciled: {event_name_value or 'unknown'} "
        f"({len(runtime['agents'])} agents)"
    )


# ── Claims, verification, acceptance ───────────────────────────────────────

def parse_evidence(value: str) -> dict[str, Any]:
    if ":" in value:
        kind, detail = value.split(":", 1)
    else:
        kind, detail = "note", value
    kind, detail = kind.strip() or "note", detail.strip()
    if not detail:
        raise SystemExit("--evidence 不能为空")
    return {
        "id": f"evidence-{uuid.uuid4().hex[:10]}",
        "kind": kind,
        "value": detail,
        "source": "commander",
        "verified": False,
        "created_at": now(),
    }


def parse_report(args: list[str]) -> tuple[str, str, str, float | None, list[dict[str, Any]]]:
    if len(args) < 3:
        raise SystemExit(__doc__)
    project, status = args[0], args[1]
    summary_parts: list[str] = []
    confidence: float | None = None
    evidence: list[dict[str, Any]] = []
    index = 2
    while index < len(args):
        token = args[index]
        if token == "--confidence":
            if index + 1 >= len(args):
                raise SystemExit("--confidence 后必须提供 0 到 1 的数字")
            try:
                confidence = float(args[index + 1])
            except ValueError as exc:
                raise SystemExit("--confidence 必须是 0 到 1 的数字") from exc
            if not 0 <= confidence <= 1:
                raise SystemExit("--confidence 必须在 0 到 1 之间")
            index += 2
        elif token == "--evidence":
            if index + 1 >= len(args):
                raise SystemExit("--evidence 后必须提供 类型:内容")
            evidence.append(parse_evidence(args[index + 1]))
            index += 2
        else:
            summary_parts.append(token)
            index += 1
    summary = " ".join(summary_parts).strip()
    if not summary:
        raise SystemExit("必须提供一行摘要")
    return project, status, summary, confidence, evidence


def create_claim(
    claims: list[dict[str, Any]],
    project: str,
    status: str,
    summary: str,
    confidence: float | None,
    evidence: list[dict[str, Any]],
) -> dict[str, Any]:
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    claim = {
        "id": f"{project}-claim-{stamp}-{uuid.uuid4().hex[:6]}",
        "project": project,
        "status": status,
        "summary": summary,
        "confidence": confidence,
        "state": "reported",
        "evidence": evidence,
        "created_at": now(),
        "verified_at": None,
        "accepted_at": None,
        "accepted_by": None,
        "acceptance_note": None,
    }
    claims.append(claim)
    return claim


def cmd_report(args: list[str]) -> None:
    name, status, summary, confidence, evidence = parse_report(args)
    if status not in STATUSES:
        raise SystemExit(f"状态必须是: {'/'.join(sorted(STATUSES))}")
    if status == "need_decision":
        return cmd_ask([name, summary])
    with state_lock():
        reg, decisions, claims = load_reg(), load_decisions(), load_claims()
        entry = require_project(reg, name)
        claim = create_claim(claims, name, status, summary, confidence, evidence)
        entry["reported_status"] = status
        entry["status"] = "need_decision" if open_decisions(decisions, name) else status
        entry["note"] = summary
        entry["claim_id"] = claim["id"]
        entry["claim_state"] = claim["state"]
        entry["updated_at"] = now()
        atomic_json(REG, reg)
        atomic_json(CLAIMS, claims)
        append_inbox(
            {
                "ts": now(),
                "type": "status_claimed",
                "project": name,
                "status": status,
                "summary": summary,
                "claim_id": claim["id"],
                "claim_state": claim["state"],
                "confidence": confidence,
                "evidence_count": len(evidence),
            }
        )
    refresh_attention()
    print(f"claimed: {claim['id']}")
    if status == "done":
        print(
            f"next: fleet verify {name} --claim {claim['id']} --label <标签> -- <验证命令>"
        )


def parse_claim_query(args: list[str]) -> tuple[str | None, bool, bool]:
    project: str | None = None
    show_all = False
    json_mode = False
    for token in args:
        if token == "--all":
            show_all = True
        elif token == "--json":
            json_mode = True
        elif token.startswith("--") or project is not None:
            raise SystemExit(__doc__)
        else:
            project = token
    return project, show_all, json_mode


def cmd_claims(args: list[str]) -> None:
    project, show_all, json_mode = parse_claim_query(args)
    values = [c for c in load_claims() if project is None or c.get("project") == project]
    if not show_all:
        latest: dict[str, dict[str, Any]] = {}
        for claim in values:
            latest[str(claim.get("project", "?"))] = claim
        values = list(latest.values())
    if json_mode:
        print(json.dumps(values, ensure_ascii=False, indent=2))
        return
    if not values:
        print("(没有状态声明)")
        return
    for claim in values:
        confidence = claim.get("confidence")
        conf_text = f"  confidence={confidence:.2f}" if isinstance(confidence, (int, float)) else ""
        print(
            f"{claim.get('id','?')}  {claim.get('project','?')}  "
            f"{claim.get('status','?')}  {claim.get('state','reported')}  "
            f"{claim.get('summary','')}{conf_text}  evidence={len(claim.get('evidence', []))}"
        )


def parse_verify(args: list[str]) -> tuple[str, str | None, str, list[str]]:
    if not args or "--" not in args:
        raise SystemExit(__doc__)
    separator = args.index("--")
    before, command = args[:separator], args[separator + 1 :]
    if not before or not command:
        raise SystemExit(__doc__)
    project = before[0]
    claim_id: str | None = None
    label = "verification"
    index = 1
    while index < len(before):
        token = before[index]
        if token == "--claim" and index + 1 < len(before):
            claim_id = before[index + 1]
            index += 2
        elif token == "--label" and index + 1 < len(before):
            label = before[index + 1]
            index += 2
        else:
            raise SystemExit(__doc__)
    return project, claim_id, label, command


def tail_text(value: str, limit: int = 4000) -> str:
    return value if len(value) <= limit else value[-limit:]


def cmd_verify(args: list[str]) -> None:
    project, claim_id, label, command = parse_verify(args)
    with state_lock():
        reg, claims = load_reg(), load_claims()
        entry = require_project(reg, project)
        claim = latest_claim(claims, project, claim_id)
        if claim is None:
            raise SystemExit(f"项目 {project} 没有可验证的 claim")
        selected_claim_id = str(claim["id"])
        cwd = entry.get("cwd") or os.getcwd()
    started_at = now()
    started_ms = utc_ms()
    timeout = int(os.environ.get("HERDR_VERIFY_TIMEOUT", "900"))
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        returncode = result.returncode
        stdout, stderr = result.stdout, result.stderr
        error = None
    except subprocess.TimeoutExpired as exc:
        returncode = 124
        stdout = exc.stdout or ""
        stderr = exc.stderr or ""
        error = f"timeout after {timeout}s"
    except OSError as exc:
        returncode = 127
        stdout, stderr = "", ""
        error = str(exc)
    finished_at = now()
    evidence = {
        "id": f"evidence-{uuid.uuid4().hex[:10]}",
        "kind": "command",
        "label": label,
        "command": command,
        "cwd": cwd,
        "source": "coordinator",
        "verified": returncode == 0,
        "exit_code": returncode,
        "stdout_tail": tail_text(str(stdout)),
        "stderr_tail": tail_text(str(stderr)),
        "error": error,
        "started_at": started_at,
        "finished_at": finished_at,
        "duration_ms": max(0, utc_ms() - started_ms),
    }
    with state_lock():
        reg, claims = load_reg(), load_claims()
        entry = require_project(reg, project)
        claim = latest_claim(claims, project, selected_claim_id)
        if claim is None:
            raise SystemExit(f"claim 在验证期间被删除: {selected_claim_id}")
        claim.setdefault("evidence", []).append(evidence)
        if claim.get("state") != "accepted":
            claim["state"] = "verified" if returncode == 0 else "reported"
        if returncode == 0:
            claim["verified_at"] = finished_at
        else:
            claim["verification_failed_at"] = finished_at
        if entry.get("claim_id") == selected_claim_id:
            entry["claim_state"] = claim["state"]
            entry["updated_at"] = finished_at
        atomic_json(REG, reg)
        atomic_json(CLAIMS, claims)
        append_inbox(
            {
                "ts": finished_at,
                "type": "verification_passed" if returncode == 0 else "verification_failed",
                "project": project,
                "status": claim.get("status", "unknown"),
                "summary": f"{label}: {'通过' if returncode == 0 else '失败'}",
                "claim_id": selected_claim_id,
                "evidence_id": evidence["id"],
                "exit_code": returncode,
            }
        )
    refresh_attention()
    print(
        f"{'verified' if returncode == 0 else 'verification failed'}: "
        f"{selected_claim_id} ({label}, exit={returncode})"
    )
    if returncode != 0:
        raise SystemExit(returncode)


def parse_accept(args: list[str]) -> tuple[str, str | None, str, bool]:
    if not args:
        raise SystemExit(__doc__)
    project = args[0]
    claim_id: str | None = None
    note = ""
    force = False
    index = 1
    while index < len(args):
        token = args[index]
        if token == "--claim" and index + 1 < len(args):
            claim_id = args[index + 1]
            index += 2
        elif token == "--note" and index + 1 < len(args):
            note = args[index + 1]
            index += 2
        elif token == "--force":
            force = True
            index += 1
        else:
            raise SystemExit(__doc__)
    return project, claim_id, note, force


def cmd_accept(args: list[str]) -> None:
    project, claim_id, note, force = parse_accept(args)
    with state_lock():
        reg, claims = load_reg(), load_claims()
        entry = require_project(reg, project)
        claim = latest_claim(claims, project, claim_id)
        if claim is None:
            raise SystemExit(f"项目 {project} 没有可验收的 claim")
        if claim.get("status") != "done" and not force:
            raise SystemExit("只有 done claim 可以验收；确需覆盖请使用 --force")
        if claim.get("state") != "verified" and not force:
            raise SystemExit("claim 尚未通过机器验证；先运行 fleet verify，或明确使用 --force")
        claim["state"] = "accepted"
        claim["accepted_at"] = now()
        claim["accepted_by"] = os.environ.get("USER") or os.environ.get("USERNAME") or "operator"
        claim["acceptance_note"] = note or None
        if entry.get("claim_id") == claim.get("id"):
            entry["claim_state"] = "accepted"
            entry["updated_at"] = claim["accepted_at"]
        atomic_json(REG, reg)
        atomic_json(CLAIMS, claims)
        append_inbox(
            {
                "ts": claim["accepted_at"],
                "type": "claim_accepted",
                "project": project,
                "status": claim.get("status", "done"),
                "summary": note or "用户已验收",
                "claim_id": claim.get("id"),
            }
        )
    refresh_attention()
    print(f"accepted: {claim['id']}")


# ── Decisions ───────────────────────────────────────────────────────────────

def create_decision(
    reg: dict[str, dict[str, Any]],
    decisions: list[dict[str, Any]],
    project: str,
    question: str,
    options: list[str],
    source: str,
) -> dict[str, Any]:
    entry = require_project(reg, project)
    decision_id = (
        f"{project}-{dt.datetime.now().strftime('%Y%m%d-%H%M%S')}-"
        f"{uuid.uuid4().hex[:6]}"
    )
    created = now()
    decision = {
        "id": decision_id,
        "project": project,
        "state": "open",
        "question": question,
        "options": options,
        "source": source,
        "created_at": created,
        "resolved_at": None,
        "resolution": None,
    }
    decisions.append(decision)
    entry["status"] = "need_decision"
    entry["note"] = question
    entry["updated_at"] = created
    append_inbox(
        {
            "ts": created,
            "type": "decision_requested",
            "project": project,
            "status": "need_decision",
            "summary": question,
            "decision_id": decision_id,
            "options": options,
        }
    )
    return decision


def parse_ask(args: list[str]) -> tuple[str, str, list[str]]:
    if len(args) < 2:
        raise SystemExit(__doc__)
    project = args[0]
    parts: list[str] = []
    options: list[str] = []
    index = 1
    while index < len(args):
        if args[index] == "--option":
            if index + 1 >= len(args):
                raise SystemExit("--option 后必须提供内容")
            options.append(args[index + 1])
            index += 2
        else:
            parts.append(args[index])
            index += 1
    question = " ".join(parts).strip()
    if not question:
        raise SystemExit("必须提供需要拍板的问题")
    return project, question, options


def cmd_ask(args: list[str]) -> None:
    project, question, options = parse_ask(args)
    with state_lock():
        reg, decisions = load_reg(), load_decisions()
        decision = create_decision(
            reg, decisions, project, question, options, "commander_report"
        )
        atomic_json(REG, reg)
        atomic_json(DECISIONS, decisions)
    refresh_attention()
    print(f"待拍板: {decision['id']}")


def cmd_decisions(args: list[str]) -> None:
    if args not in ([], ["--all"]):
        raise SystemExit(__doc__)
    decisions = load_decisions()
    values = decisions if args == ["--all"] else open_decisions(decisions)
    if not values:
        print("(没有待拍板事项)" if not args else "(没有决策记录)")
        return
    for item in values:
        state = "待拍板" if item.get("state") == "open" else "已解决"
        options = item.get("options") or []
        suffix = f"  选项: {' / '.join(options)}" if options else ""
        if item.get("resolution"):
            suffix += f"  结论: {item['resolution']}"
        print(
            f"{item.get('id','?')}  {item.get('project','?')}  {state}  "
            f"{item.get('question','')}{suffix}"
        )


def cmd_resolve(args: list[str]) -> None:
    if len(args) < 2:
        raise SystemExit(__doc__)
    decision_id, answer = args[0], " ".join(args[1:]).strip()
    if not answer:
        raise SystemExit("必须提供用户的拍板答案")
    with state_lock():
        reg, decisions = load_reg(), load_decisions()
        target = next((d for d in decisions if d.get("id") == decision_id), None)
        if target is None:
            raise SystemExit(f"找不到决策: {decision_id}")
        if target.get("state") != "open":
            raise SystemExit(f"决策已经解决: {decision_id}")
        target["state"] = "resolved"
        target["resolution"] = answer
        target["resolved_at"] = now()
        project = str(target.get("project", ""))
        entry = require_project(reg, project)
        remaining = [
            d
            for d in decisions
            if d is not target and d.get("project") == project and d.get("state") == "open"
        ]
        entry["status"] = (
            "need_decision" if remaining else entry.get("reported_status", "idle")
        )
        entry["note"] = remaining[0]["question"] if remaining else f"已拍板：{answer}"
        entry["updated_at"] = now()
        atomic_json(REG, reg)
        atomic_json(DECISIONS, decisions)
        append_inbox(
            {
                "ts": now(),
                "type": "decision_resolved",
                "project": project,
                "status": entry["status"],
                "summary": f"{target.get('question','')} → {answer}",
                "decision_id": decision_id,
            }
        )
    refresh_attention()
    print(f"已拍板: {decision_id} -> {answer}")


# ── Attention projection ───────────────────────────────────────────────────

def attention_item(
    *,
    project: str,
    kind: str,
    priority: int,
    reason: str,
    required_actor: str,
    created_at: str | None,
    action: str,
    reference_id: str | None = None,
) -> dict[str, Any]:
    created = created_at or now()
    suffix = reference_id or project
    return {
        "id": f"{kind}:{project}:{suffix}",
        "project": project,
        "kind": kind,
        "priority": priority,
        "reason": reason,
        "required_actor": required_actor,
        "created_at": created,
        "age_seconds": age_seconds(created),
        "action": action,
        "reference_id": reference_id,
    }


def compute_attention(
    reg: dict[str, dict[str, Any]],
    decisions: list[dict[str, Any]],
    claims: list[dict[str, Any]],
    runtime: dict[str, Any],
) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    open_by_project: dict[str, list[dict[str, Any]]] = {}
    for decision in open_decisions(decisions):
        project = str(decision.get("project", "?"))
        open_by_project.setdefault(project, []).append(decision)
        items.append(
            attention_item(
                project=project,
                kind="decision_required",
                priority=100,
                reason=str(decision.get("question", "需要用户拍板")),
                required_actor="user",
                created_at=decision.get("created_at"),
                action=f"fleet resolve {decision.get('id','<decision_id>')} <答案>",
                reference_id=str(decision.get("id", "")),
            )
        )

    if runtime and not runtime.get("connected") and reg:
        items.append(
            attention_item(
                project="*",
                kind="runtime_disconnected",
                priority=95,
                reason=str(runtime.get("error") or "Herdr 实时状态连接中断"),
                required_actor="coordinator",
                created_at=runtime.get("updated_at"),
                action="fleet plugin-reconcile",
            )
        )

    runtime_projects = runtime.get("projects", {}) if isinstance(runtime, dict) else {}
    for project, entry in reg.items():
        status = effective_status(entry, decisions, project)
        live = runtime_projects.get(project, {}) if isinstance(runtime_projects, dict) else {}
        live_status = live.get("live_status")
        live_since = live.get("live_status_since") or live.get("observed_at")
        has_decision = bool(open_by_project.get(project))

        if runtime.get("connected") and status in {"working", "blocked"} and live:
            if not live.get("online"):
                items.append(
                    attention_item(
                        project=project,
                        kind="commander_offline",
                        priority=88,
                        reason=f"项目仍标记为 {status}，但机长 {entry.get('commander','?')} 已离线",
                        required_actor="coordinator",
                        created_at=live_since,
                        action=f"fleet route {project} <恢复或重新指派指令>",
                    )
                )
            elif live_status == "blocked" and not has_decision:
                stale = age_seconds(live_since) >= 600
                items.append(
                    attention_item(
                        project=project,
                        kind="stale_blocked" if stale else "observed_blocked",
                        priority=90 if stale else 82,
                        reason=(
                            "机长持续阻塞超过 10 分钟，且尚未创建正式决策"
                            if stale
                            else "机长实时状态为 blocked，但尚未创建正式决策"
                        ),
                        required_actor="coordinator",
                        created_at=live_since,
                        action=f"fleet route {project} 请说明阻塞原因；需要用户拍板时运行 fleet ask",
                    )
                )
            elif status == "working" and live_status == "idle" and age_seconds(live_since) >= 300:
                items.append(
                    attention_item(
                        project=project,
                        kind="project_idle_mismatch",
                        priority=52,
                        reason="项目仍标记 working，但机长已空闲超过 5 分钟",
                        required_actor="coordinator",
                        created_at=live_since,
                        action=f"fleet route {project} 请汇报当前项目状态",
                    )
                )

        claim = latest_claim(claims, project, entry.get("claim_id"))
        if status == "done":
            if claim is None or claim.get("state") == "reported":
                claim_id = str(claim.get("id")) if claim else None
                items.append(
                    attention_item(
                        project=project,
                        kind="reported_done_unverified",
                        priority=72,
                        reason="机长声称完成，但尚无通过的机器验证证据",
                        required_actor="coordinator",
                        created_at=(claim or {}).get("created_at") or entry.get("updated_at"),
                        action=(
                            f"fleet verify {project} --claim {claim_id} --label <标签> -- <验证命令>"
                            if claim_id
                            else f"fleet report {project} done <摘要> 后运行 fleet verify"
                        ),
                        reference_id=claim_id,
                    )
                )
            elif claim.get("state") == "verified":
                items.append(
                    attention_item(
                        project=project,
                        kind="ready_for_acceptance",
                        priority=62,
                        reason="完成声明已通过机器验证，等待用户验收",
                        required_actor="user",
                        created_at=claim.get("verified_at") or claim.get("created_at"),
                        action=f"fleet accept {project} --claim {claim.get('id')}",
                        reference_id=str(claim.get("id", "")),
                    )
                )

    items.sort(
        key=lambda item: (
            -int(item.get("priority", 0)),
            -int(item.get("age_seconds", 0)),
            str(item.get("project", "")),
            str(item.get("kind", "")),
        )
    )
    return items


def refresh_attention() -> list[dict[str, Any]]:
    with state_lock():
        values = compute_attention(
            load_reg(), load_decisions(), load_claims(), load_json(RUNTIME, {})
        )
        atomic_json(ATTENTION, values)
    return values


def cmd_attention(args: list[str]) -> None:
    if args not in ([], ["--json"]):
        raise SystemExit(__doc__)
    values = refresh_attention()
    if args == ["--json"]:
        print(json.dumps(values, ensure_ascii=False, indent=2))
        return
    if not values:
        print("(当前无需人工介入)")
        return
    for index, item in enumerate(values, start=1):
        print(
            f"{index:>2}. P{item['priority']}  {item['project']}  {item['kind']}  "
            f"{item['reason']}  → {item['required_actor']}"
        )
        print(f"    {item['action']}")


# ── Operator-facing commands ───────────────────────────────────────────────

def cmd_list(_args: list[str]) -> None:
    reg, decisions = load_reg(), load_decisions()
    runtime = load_json(RUNTIME, {})
    projects = runtime.get("projects", {}) if isinstance(runtime, dict) else {}
    plugin_mode = bool(os.environ.get("HERDR_PLUGIN_ID"))
    watching = watcher_pid() is not None
    if not reg:
        print("(注册表为空)")
        return
    for name, entry in reg.items():
        live = projects.get(name, {}) if isinstance(projects, dict) else {}
        if runtime.get("connected"):
            live_text = live.get("live_status", "offline")
        elif plugin_mode:
            live_text = "未对账"
        elif not watching:
            live_text = "未监听"
        else:
            live_text = "连接中断"
        status = effective_status(entry, decisions, name)
        pending = open_decisions(decisions, name)
        decision = f"  待拍板={pending[0]['question']}" if pending else ""
        claim_text = (
            f"  claim={entry.get('claim_state')}"
            if entry.get("claim_state")
            else ""
        )
        print(
            f"{name:<16} 机长={entry.get('commander','?'):<14} "
            f"项目={status:<14} 实时={live_text:<10} "
            f"更新={entry.get('updated_at','?')}  {entry.get('note','')}"
            f"{decision}{claim_text}"
        )
    registered = {entry.get("commander") for entry in reg.values()}
    ghosts = {
        agent.get("name")
        for agent in runtime.get("agents", [])
        if agent.get("name", "").endswith("-cmd")
        and agent.get("name") not in registered
    }
    if ghosts:
        print(f"⚠ 存活但未注册的机长: {', '.join(sorted(ghosts))}")
    attention = refresh_attention()
    if attention:
        top = attention[0]
        print(
            f"⚠ attention={len(attention)}  top={top['project']}/{top['kind']}: {top['reason']}"
        )
    if not plugin_mode and not watching:
        print("⚠ 事件监听器未运行；执行 ./fleet watch-start，或安装为 Herdr plugin")


def cmd_route(args: list[str]) -> None:
    if len(args) < 2:
        raise SystemExit(__doc__)
    project, text = args[0], " ".join(args[1:]).strip()
    if not text:
        raise SystemExit("必须提供要转发给机长的指令")
    entry = require_project(load_reg(), project)
    commander = entry.get("commander")
    if not commander:
        raise SystemExit(f"项目 {project} 没有登记机长")
    result = subprocess.run(
        [HERDR_BIN, "agent", "prompt", commander, text],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(result.stderr.strip() or f"无法联系 {project} 的机长")
    print(f"已转发给 {project} 的机长")


def cmd_inbox(args: list[str]) -> None:
    if args not in ([], ["--all"]):
        raise SystemExit(__doc__)
    show_all = args == ["--all"]
    try:
        with open(INBOX, encoding="utf-8") as handle:
            lines = handle.readlines()
    except FileNotFoundError:
        print("(收件箱为空)")
        return
    cursor = 0
    if not show_all:
        try:
            with open(CURSOR, encoding="utf-8") as handle:
                cursor = int(handle.read().strip() or 0)
        except (FileNotFoundError, ValueError):
            pass
    values = lines if show_all else lines[cursor:]
    if not values:
        print("(没有新消息)")
    for line in values:
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            continue
        refs: list[str] = []
        if entry.get("decision_id"):
            refs.append(f"决策 {entry['decision_id']}")
        if entry.get("claim_id"):
            refs.append(f"claim {entry['claim_id']}")
        reference = f" · {' · '.join(refs)}" if refs else ""
        print(
            f"[{entry.get('ts','?')}] {entry.get('project','?')} · "
            f"{entry.get('status','?')} · {entry.get('summary','')}{reference}"
        )
    if not show_all:
        with open(CURSOR, "w", encoding="utf-8") as handle:
            handle.write(str(len(lines)))


# ── Compatibility socket watcher ───────────────────────────────────────────

def notify_watcher() -> None:
    pid = watcher_pid()
    if pid is not None:
        try:
            os.kill(pid, signal.SIGHUP)
        except ProcessLookupError:
            pass


def watcher_pid() -> int | None:
    try:
        with open(WATCH_PID, encoding="utf-8") as handle:
            pid = int(handle.read().strip())
        os.kill(pid, 0)
        command = subprocess.run(
            ["ps", "-p", str(pid), "-o", "command="],
            capture_output=True,
            text=True,
        ).stdout
        if os.path.abspath(__file__) not in command or " watch" not in command:
            return None
        return pid
    except (FileNotFoundError, ValueError, ProcessLookupError, PermissionError):
        return None


def socket_path() -> str:
    override = os.environ.get("HERDR_SOCKET_PATH")
    if override:
        return os.path.expanduser(override)
    config = os.environ.get(
        "HERDR_CONFIG_PATH", os.path.expanduser("~/.config/herdr/config.toml")
    )
    config_dir = os.path.dirname(config)
    session = os.environ.get("HERDR_SESSION")
    if session and session != "default":
        return os.path.join(config_dir, "sessions", session, "herdr.sock")
    return os.path.join(config_dir, "herdr.sock")


def send_request(writer: Any, request_id: str, method: str, params: dict[str, Any]) -> None:
    payload = {"id": request_id, "method": method, "params": params}
    writer.write((json.dumps(payload, separators=(",", ":")) + "\n").encode())
    writer.flush()


def read_response(reader: Any, request_id: str) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    pending: list[dict[str, Any]] = []
    while True:
        line = reader.readline()
        if not line:
            raise ConnectionError("Herdr socket 已关闭")
        message = json.loads(line)
        if message.get("id") == request_id:
            if "error" in message:
                raise ConnectionError(str(message["error"]))
            return message, pending
        pending.append(message)


def subscriptions_for(snapshot: dict[str, Any]) -> list[dict[str, Any]]:
    subscriptions = [{"type": event} for event in sorted(TOPOLOGY_EVENTS)]
    pane_ids = {
        agent.get("pane_id")
        for agent in runtime_agents(snapshot)
        if agent.get("pane_id")
    }
    subscriptions.extend(
        {"type": "pane.agent_status_changed", "pane_id": pane_id}
        for pane_id in sorted(pane_ids)
    )
    return subscriptions


def event_name(message: dict[str, Any]) -> str | None:
    raw = message.get("event") or message.get("data", {}).get("type")
    return EVENT_ALIASES.get(raw, raw)


def apply_status_event(snapshot: dict[str, Any], message: dict[str, Any]) -> bool:
    if event_name(message) != "pane.agent_status_changed":
        return False
    data = message.get("data", {})
    pane_id = data.get("pane_id")
    for collection in (snapshot.get("agents", []), snapshot.get("panes", [])):
        for item in collection:
            if item.get("pane_id") == pane_id:
                item["agent_status"] = data.get("agent_status", "unknown")
                for field in ("agent", "display_agent", "title", "state_labels"):
                    if field in data:
                        item[field] = data[field]
    return True


def fetch_socket_snapshot() -> dict[str, Any]:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(socket_path())
    with client:
        with client.makefile("rb") as reader, client.makefile("wb") as writer:
            send_request(writer, "tower_snapshot", "session.snapshot", {})
            response, _pending = read_response(reader, "tower_snapshot")
    snapshot = response.get("result", {}).get("snapshot", {})
    if not snapshot:
        raise ConnectionError("Herdr 未返回 session snapshot")
    return snapshot


def watch_connection() -> None:
    snapshot = fetch_socket_snapshot()
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(socket_path())
    with client:
        with client.makefile("rb") as reader, client.makefile("wb") as writer:
            send_request(
                writer,
                "tower_subscribe",
                "events.subscribe",
                {"subscriptions": subscriptions_for(snapshot)},
            )
            _ack, early_events = read_response(reader, "tower_subscribe")
            write_runtime(snapshot, source="watch")
            refresh_attention()
            for message in early_events:
                if apply_status_event(snapshot, message):
                    write_runtime(snapshot, source="watch", last_event=message)
                    refresh_attention()
            while True:
                line = reader.readline()
                if not line:
                    raise ConnectionError("Herdr socket 已关闭")
                message = json.loads(line)
                event = event_name(message)
                if event in TOPOLOGY_EVENTS:
                    raise ReloadWatch()
                if apply_status_event(snapshot, message):
                    write_runtime(snapshot, source="watch", last_event=message)
                    refresh_attention()


def cmd_watch(_args: list[str]) -> None:
    ensure_data_dir()
    existing = watcher_pid()
    if existing is not None and existing != os.getpid():
        raise SystemExit(f"事件监听器已在运行（pid {existing}）")
    with open(WATCH_PID, "w", encoding="utf-8") as handle:
        handle.write(str(os.getpid()))
    previous = load_json(RUNTIME, {})
    write_runtime(
        {
            "agents": previous.get("agents", []),
            "version": previous.get("herdr_version"),
            "protocol": previous.get("protocol"),
        },
        connected=False,
        error="正在建立 Herdr 事件订阅",
        source="watch",
    )

    def reload_handler(_signum: int, _frame: Any) -> None:
        raise ReloadWatch()

    def stop_handler(_signum: int, _frame: Any) -> None:
        raise StopWatch()

    signal.signal(signal.SIGHUP, reload_handler)
    signal.signal(signal.SIGTERM, stop_handler)
    delay = 0.25
    try:
        while True:
            try:
                watch_connection()
                delay = 0.25
            except ReloadWatch:
                delay = 0.25
                continue
            except StopWatch:
                break
            except (ConnectionError, OSError, json.JSONDecodeError) as exc:
                current = load_json(RUNTIME, {})
                snapshot = {
                    "agents": current.get("agents", []),
                    "version": current.get("herdr_version"),
                    "protocol": current.get("protocol"),
                }
                write_runtime(
                    snapshot,
                    connected=False,
                    error=str(exc),
                    source="watch",
                )
                refresh_attention()
                try:
                    time.sleep(delay)
                except ReloadWatch:
                    delay = 0.25
                    continue
                except StopWatch:
                    break
                delay = min(delay * 2, 5.0)
    finally:
        try:
            with open(WATCH_PID, encoding="utf-8") as handle:
                owned = int(handle.read().strip()) == os.getpid()
            if owned:
                os.unlink(WATCH_PID)
        except (FileNotFoundError, ValueError):
            pass


def cmd_watch_start(_args: list[str]) -> None:
    pid = watcher_pid()
    if pid is not None:
        print(f"事件监听器已在运行（pid {pid}）")
        return
    ensure_data_dir()
    log = open(WATCH_LOG, "a", encoding="utf-8")
    subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "watch"],
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        close_fds=True,
    )
    log.close()
    for _ in range(40):
        time.sleep(0.05)
        pid = watcher_pid()
        if pid is not None:
            print(f"事件监听器已启动（pid {pid}）")
            return
    raise SystemExit(f"事件监听器启动失败；查看 {WATCH_LOG}")


def cmd_watch_status(_args: list[str]) -> None:
    pid = watcher_pid()
    runtime = load_json(RUNTIME, {})
    if pid is None:
        print("事件监听器未运行")
        return
    state = "已连接 Herdr" if runtime.get("connected") else "正在重连 Herdr"
    print(f"事件监听器运行中（pid {pid}，{state}）")


def cmd_watch_stop(_args: list[str]) -> None:
    pid = watcher_pid()
    if pid is None:
        print("事件监听器未运行")
        return
    os.kill(pid, signal.SIGTERM)
    for _ in range(40):
        time.sleep(0.05)
        if watcher_pid() is None:
            print("事件监听器已停止")
            return
    raise SystemExit("事件监听器没有及时停止")


COMMANDS = {
    "init": cmd_init,
    "register": cmd_register,
    "set-status": cmd_set_status,
    "unregister": cmd_unregister,
    "list": cmd_list,
    "route": cmd_route,
    "sync": cmd_sync,
    "report": cmd_report,
    "claims": cmd_claims,
    "verify": cmd_verify,
    "accept": cmd_accept,
    "ask": cmd_ask,
    "decisions": cmd_decisions,
    "resolve": cmd_resolve,
    "attention": cmd_attention,
    "inbox": cmd_inbox,
    "plugin-reconcile": cmd_plugin_reconcile,
    "plugin-event": cmd_plugin_event,
    "watch": cmd_watch,
    "watch-start": cmd_watch_start,
    "watch-status": cmd_watch_status,
    "watch-stop": cmd_watch_stop,
}


if __name__ == "__main__":
    if len(sys.argv) < 2 or sys.argv[1] not in COMMANDS:
        print(__doc__)
        raise SystemExit(1)
    try:
        migrate_legacy_state()
        COMMANDS[sys.argv[1]](sys.argv[2:])
    except IndexError:
        print(__doc__)
        raise SystemExit(1)

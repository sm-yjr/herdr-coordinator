"""Runtime guards installed by the public ``fleet`` entrypoint.

The large control-plane implementation stays importable for tests and tooling, while
this module owns launcher-level guarantees that depend on the execution environment.
Callers should always use the repository-root ``./fleet`` command.
"""

from __future__ import annotations

from types import ModuleType
from typing import Any


def install(core: ModuleType) -> None:
    """Install timeout and governance guards on a loaded fleet core module."""

    original_watch_start = core.cmd_watch_start
    original_watch_status = core.cmd_watch_status

    def plugin_mode() -> bool:
        return bool(
            core.os.environ.get("HERDR_PLUGIN_STATE_DIR")
            or core.os.environ.get("HERDR_PLUGIN_ID")
        )

    def snapshot_from_cli() -> dict[str, Any]:
        try:
            timeout = float(core.os.environ.get("HERDR_SNAPSHOT_TIMEOUT", "15"))
        except ValueError as exc:
            raise SystemExit("HERDR_SNAPSHOT_TIMEOUT 必须是秒数") from exc
        if timeout <= 0:
            raise SystemExit("HERDR_SNAPSHOT_TIMEOUT 必须大于 0")
        try:
            result = core.subprocess.run(
                [core.HERDR_BIN, "api", "snapshot"],
                capture_output=True,
                text=True,
                timeout=timeout,
            )
        except core.subprocess.TimeoutExpired as exc:
            raise SystemExit(f"herdr api snapshot 超时（{timeout:g}s）") from exc
        except OSError as exc:
            raise SystemExit(f"无法执行 Herdr CLI: {exc}") from exc
        if result.returncode != 0:
            raise SystemExit(result.stderr.strip() or "herdr api snapshot 失败")
        return core.parse_snapshot_payload(result.stdout)

    def cmd_accept(args: list[str]) -> None:
        project, claim_id, note, force = core.parse_accept(args)
        with core.state_lock():
            reg, claims = core.load_reg(), core.load_claims()
            entry = core.require_project(reg, project)
            claim = core.latest_claim(claims, project, claim_id)
            if claim is None:
                raise SystemExit(f"项目 {project} 没有可验收的 claim")
            if claim.get("status") != "done":
                raise SystemExit("只有 done claim 可以验收")
            if force and not note:
                raise SystemExit(
                    "--force 必须同时提供 --note，记录接受未验证风险的原因"
                )
            if claim.get("state") != "verified" and not force:
                raise SystemExit(
                    "claim 尚未通过机器验证；先运行 fleet verify，或由用户明确授权 --force"
                )
            claim["state"] = "accepted"
            claim["accepted_at"] = core.now()
            claim["accepted_by"] = (
                core.os.environ.get("USER")
                or core.os.environ.get("USERNAME")
                or "operator"
            )
            claim["acceptance_note"] = note or None
            if entry.get("claim_id") == claim.get("id"):
                entry["claim_state"] = "accepted"
                entry["updated_at"] = claim["accepted_at"]
            core.atomic_json(core.REG, reg)
            core.atomic_json(core.CLAIMS, claims)
            core.append_inbox(
                {
                    "ts": claim["accepted_at"],
                    "type": "claim_accepted",
                    "project": project,
                    "status": "done",
                    "summary": note or "用户已验收",
                    "claim_id": claim.get("id"),
                }
            )
        core.refresh_attention()
        print(f"accepted: {claim['id']}")

    def cmd_watch_start(args: list[str]) -> None:
        if plugin_mode():
            print("Plugin 模式由 Herdr startup/event hooks 维护实时状态，无独立 watcher")
            return
        original_watch_start(args)

    def cmd_watch_status(args: list[str]) -> None:
        if plugin_mode():
            runtime = core.load_json(core.RUNTIME, {})
            state = "已连接 Herdr" if runtime.get("connected") else "等待 snapshot 对账"
            print(f"Plugin 事件模式（{state}，无独立 watcher）")
            return
        original_watch_status(args)

    core.snapshot_from_cli = snapshot_from_cli
    core.cmd_accept = cmd_accept
    core.cmd_watch_start = cmd_watch_start
    core.cmd_watch_status = cmd_watch_status
    core.COMMANDS["accept"] = cmd_accept
    core.COMMANDS["watch-start"] = cmd_watch_start
    core.COMMANDS["watch-status"] = cmd_watch_status

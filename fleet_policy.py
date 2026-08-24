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

    core.snapshot_from_cli = snapshot_from_cli
    core.cmd_accept = cmd_accept
    core.COMMANDS["accept"] = cmd_accept

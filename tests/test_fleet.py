import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
FLEET = ROOT / "fleet"
PLUGIN_RUNTIME = ROOT / "scripts" / "plugin_runtime.py"


class FleetTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.data = self.root / "state"
        self.env = os.environ.copy()
        self.env.pop("HERDR_PLUGIN_STATE_DIR", None)
        self.env.pop("HERDR_PLUGIN_ID", None)
        self.env.pop("HERDR_BIN_PATH", None)
        self.env["HERDR_COORDINATOR_HOME"] = str(self.data)
        self.fleet("init")
        self.fleet(
            "register",
            "demo",
            "--tab",
            "w1:t1",
            "--commander",
            "demo-cmd",
            "--cwd",
            str(self.root),
        )

    def tearDown(self):
        self.temp.cleanup()

    def fleet(self, *args, check=True, env=None):
        return subprocess.run(
            [str(FLEET), *args],
            env=env or self.env,
            text=True,
            capture_output=True,
            check=check,
        )

    def load(self, name):
        return json.loads((self.data / name).read_text())

    def install_fake_herdr(self, snapshot=None):
        fake_bin = self.root / "bin"
        fake_bin.mkdir(exist_ok=True)
        route_log = self.root / "route.json"
        snapshot_path = self.root / "snapshot.json"
        if snapshot is not None:
            snapshot_path.write_text(json.dumps(snapshot))
        fake_herdr = fake_bin / "herdr"
        fake_herdr.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "args = sys.argv[1:]\n"
            "if args[:2] == ['api', 'snapshot']:\n"
            "    snap = json.load(open(os.environ['SNAPSHOT_PATH']))\n"
            "    print(json.dumps({'result': {'type': 'session_snapshot', 'snapshot': snap}}))\n"
            "elif args[:2] == ['agent', 'prompt']:\n"
            "    open(os.environ['ROUTE_LOG'], 'w').write(json.dumps(args))\n"
            "else:\n"
            "    print(json.dumps({'result': {}}))\n"
        )
        fake_herdr.chmod(0o755)
        self.env["PATH"] = f"{fake_bin}:{self.env['PATH']}"
        self.env["HERDR_BIN_PATH"] = str(fake_herdr)
        self.env["ROUTE_LOG"] = str(route_log)
        self.env["SNAPSHOT_PATH"] = str(snapshot_path)
        return fake_herdr, route_log, snapshot_path

    def test_decision_is_first_class_and_preserves_reported_status(self):
        self.fleet("report", "demo", "working", "开始实现")
        created = self.fleet(
            "ask",
            "demo",
            "采用哪种兼容策略？",
            "--option",
            "保持旧格式",
            "--option",
            "升级格式",
        )
        decision_id = created.stdout.strip().split(": ", 1)[1]

        decisions = self.load("decisions.json")
        self.assertEqual(decisions[0]["id"], decision_id)
        self.assertEqual(decisions[0]["state"], "open")
        self.assertEqual(decisions[0]["options"], ["保持旧格式", "升级格式"])

        self.fleet("report", "demo", "done", "实现已完成，等待选择")
        entry = self.load("fleets.json")["demo"]
        self.assertEqual(entry["reported_status"], "done")
        self.assertEqual(entry["status"], "need_decision")

        self.fleet("resolve", decision_id, "保持旧格式")
        entry = self.load("fleets.json")["demo"]
        self.assertEqual(entry["status"], "done")
        self.assertEqual(self.load("decisions.json")[0]["state"], "resolved")

    def test_blocked_does_not_implicitly_create_decision(self):
        self.fleet("set-status", "demo", "blocked", "等待外部服务")
        self.assertEqual(self.load("decisions.json"), [])
        self.assertEqual(self.load("fleets.json")["demo"]["status"], "blocked")

    def test_route_can_only_target_registered_commander(self):
        _, route_log, _ = self.install_fake_herdr({"agents": []})
        self.fleet("route", "demo", "检查当前发布状态")
        self.assertEqual(
            json.loads(route_log.read_text()),
            ["agent", "prompt", "demo-cmd", "检查当前发布状态"],
        )

    def test_legacy_need_decision_is_migrated(self):
        registry = self.load("fleets.json")
        registry["demo"]["status"] = "need_decision"
        registry["demo"]["note"] = "旧版遗留问题"
        registry["demo"].pop("reported_status", None)
        (self.data / "fleets.json").write_text(json.dumps(registry))

        self.fleet("decisions")
        decision = self.load("decisions.json")[0]
        self.assertEqual(decision["question"], "旧版遗留问题")
        self.assertEqual(decision["source"], "legacy_registry_migration")

    def test_wrapper_discovers_installed_plugin_state(self):
        xdg_state = self.root / "xdg-state"
        plugin_state = xdg_state / "herdr" / "plugins" / "sm-yjr.herdr-coordinator"
        plugin_state.mkdir(parents=True)
        env = os.environ.copy()
        env.pop("HERDR_PLUGIN_STATE_DIR", None)
        env.pop("HERDR_COORDINATOR_HOME", None)
        env.pop("HERDR_PLUGIN_ID", None)
        env["XDG_STATE_HOME"] = str(xdg_state)
        result = subprocess.run(
            [str(FLEET), "init"],
            env=env,
            text=True,
            capture_output=True,
            check=True,
        )
        self.assertIn(str(plugin_state), result.stdout)
        self.assertTrue((plugin_state / "fleets.json").exists())

    def test_plugin_state_dir_takes_precedence(self):
        plugin_state = self.root / "plugin-state"
        env = self.env.copy()
        env["HERDR_PLUGIN_STATE_DIR"] = str(plugin_state)
        result = self.fleet("init", env=env)
        self.assertIn(str(plugin_state), result.stdout)
        self.assertTrue((plugin_state / "claims.json").exists())
        self.assertTrue((plugin_state / "attention.json").exists())

    def test_plugin_runtime_imports_legacy_state_before_reconcile(self):
        plugin_state = self.root / "plugin-state"
        snapshot = {"version": "0.8.2", "protocol": 20, "agents": []}
        self.install_fake_herdr(snapshot)
        env = self.env.copy()
        env["HERDR_PLUGIN_STATE_DIR"] = str(plugin_state)
        env["HERDR_PLUGIN_ROOT"] = str(ROOT)
        result = subprocess.run(
            [sys.executable, str(PLUGIN_RUNTIME), "reconcile"],
            env=env,
            text=True,
            capture_output=True,
            check=True,
        )
        self.assertIn("plugin reconciled", result.stdout)
        registry = json.loads((plugin_state / "fleets.json").read_text())
        self.assertIn("demo", registry)
        marker = json.loads((plugin_state / "legacy-import.json").read_text())
        self.assertIn("fleets.json", marker["imported"])
        self.assertNotIn("runtime.json", marker["imported"])
        runtime = json.loads((plugin_state / "runtime.json").read_text())
        self.assertTrue(runtime["connected"])

    def test_plugin_snapshot_has_bounded_timeout(self):
        fake_bin = self.root / "timeout-bin"
        fake_bin.mkdir()
        fake_herdr = fake_bin / "herdr"
        fake_herdr.write_text(
            "#!/usr/bin/env python3\n"
            "import time\n"
            "time.sleep(1)\n"
        )
        fake_herdr.chmod(0o755)
        env = self.env.copy()
        env["HERDR_BIN_PATH"] = str(fake_herdr)
        env["HERDR_SNAPSHOT_TIMEOUT"] = "0.02"
        result = self.fleet("plugin-reconcile", check=False, env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("snapshot 超时", result.stderr)

    def test_plugin_reconcile_uses_snapshot_and_preserves_project_state(self):
        snapshot = {
            "version": "0.8.2",
            "protocol": 20,
            "agents": [
                {
                    "name": "demo-cmd",
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "agent_status": "working",
                    "state_labels": {"mode": "coding"},
                }
            ],
        }
        self.install_fake_herdr(snapshot)
        self.fleet("plugin-reconcile")
        runtime = self.load("runtime.json")
        self.assertTrue(runtime["connected"])
        self.assertEqual(runtime["source"], "plugin_startup")
        self.assertEqual(runtime["projects"]["demo"]["live_status"], "working")
        entry = self.load("fleets.json")["demo"]
        self.assertEqual(entry["reported_status"], "idle")

    def test_plugin_event_reconciles_fresh_snapshot_not_payload(self):
        snapshot = {
            "version": "0.8.2",
            "protocol": 20,
            "agents": [
                {
                    "name": "demo-cmd",
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "agent_status": "idle",
                }
            ],
        }
        _, _, snapshot_path = self.install_fake_herdr(snapshot)
        self.fleet("plugin-reconcile")
        snapshot["agents"][0]["agent_status"] = "working"
        snapshot_path.write_text(json.dumps(snapshot))
        env = self.env.copy()
        env["HERDR_PLUGIN_EVENT"] = "pane.agent_status_changed"
        env["HERDR_PLUGIN_EVENT_JSON"] = json.dumps(
            {
                "event": "pane_agent_status_changed",
                "data": {"pane_id": "w1:p1", "agent_status": "blocked"},
            }
        )
        self.fleet("plugin-event", env=env)
        runtime = self.load("runtime.json")
        self.assertEqual(runtime["projects"]["demo"]["live_status"], "working")
        self.assertEqual(runtime["source"], "plugin_event")
        self.assertEqual(runtime["last_event"]["name"], "pane.agent_status_changed")

    def test_report_verify_accept_protocol(self):
        report = self.fleet(
            "report",
            "demo",
            "done",
            "实现和测试完成",
            "--confidence",
            "0.91",
            "--evidence",
            "commit:abc123",
        )
        claim_id = report.stdout.splitlines()[0].split(": ", 1)[1]
        claim = self.load("claims.json")[0]
        self.assertEqual(claim["state"], "reported")
        self.assertAlmostEqual(claim["confidence"], 0.91)
        self.assertFalse(claim["evidence"][0]["verified"])

        kinds = [item["kind"] for item in self.load("attention.json")]
        self.assertIn("reported_done_unverified", kinds)

        self.fleet(
            "verify",
            "demo",
            "--claim",
            claim_id,
            "--label",
            "unit-tests",
            "--",
            sys.executable,
            "-c",
            "print('ok')",
        )
        claim = self.load("claims.json")[0]
        self.assertEqual(claim["state"], "verified")
        self.assertTrue(claim["evidence"][-1]["verified"])
        kinds = [item["kind"] for item in self.load("attention.json")]
        self.assertIn("ready_for_acceptance", kinds)
        self.assertNotIn("reported_done_unverified", kinds)

        self.fleet("accept", "demo", "--claim", claim_id, "--note", "验收通过")
        claim = self.load("claims.json")[0]
        self.assertEqual(claim["state"], "accepted")
        self.assertEqual(self.load("fleets.json")["demo"]["claim_state"], "accepted")
        kinds = [item["kind"] for item in self.load("attention.json")]
        self.assertNotIn("ready_for_acceptance", kinds)

    def test_failed_verification_stays_unverified(self):
        report = self.fleet("report", "demo", "done", "声称完成")
        claim_id = report.stdout.splitlines()[0].split(": ", 1)[1]
        result = self.fleet(
            "verify",
            "demo",
            "--claim",
            claim_id,
            "--",
            sys.executable,
            "-c",
            "raise SystemExit(3)",
            check=False,
        )
        self.assertEqual(result.returncode, 3)
        claim = self.load("claims.json")[0]
        self.assertEqual(claim["state"], "reported")
        self.assertEqual(claim["evidence"][-1]["exit_code"], 3)
        self.assertIn(
            "reported_done_unverified",
            [item["kind"] for item in self.load("attention.json")],
        )

    def test_accept_requires_verification_unless_forced_with_note(self):
        report = self.fleet("report", "demo", "done", "声称完成")
        claim_id = report.stdout.splitlines()[0].split(": ", 1)[1]
        denied = self.fleet("accept", "demo", "--claim", claim_id, check=False)
        self.assertNotEqual(denied.returncode, 0)
        missing_note = self.fleet(
            "accept", "demo", "--claim", claim_id, "--force", check=False
        )
        self.assertNotEqual(missing_note.returncode, 0)
        self.fleet(
            "accept",
            "demo",
            "--claim",
            claim_id,
            "--force",
            "--note",
            "用户接受未验证风险",
        )
        self.assertEqual(self.load("claims.json")[0]["state"], "accepted")

    def test_force_cannot_accept_non_done_claim(self):
        report = self.fleet("report", "demo", "working", "仍在开发")
        claim_id = report.stdout.splitlines()[0].split(": ", 1)[1]
        result = self.fleet(
            "accept",
            "demo",
            "--claim",
            claim_id,
            "--force",
            "--note",
            "不应允许",
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.load("claims.json")[0]["state"], "reported")

    def test_attention_prioritizes_decision_then_stale_block(self):
        self.fleet(
            "register",
            "blocked",
            "--tab",
            "w1:t2",
            "--commander",
            "blocked-cmd",
            "--cwd",
            str(self.root),
        )
        self.fleet("set-status", "blocked", "working", "正在执行")
        old = "2026-01-01 00:00:00"
        (self.data / "runtime.json").write_text(
            json.dumps(
                {
                    "connected": True,
                    "updated_at": old,
                    "agents": [],
                    "projects": {
                        "demo": {
                            "online": True,
                            "live_status": "idle",
                            "live_status_since": old,
                        },
                        "blocked": {
                            "online": True,
                            "live_status": "blocked",
                            "live_status_since": old,
                        },
                    },
                }
            )
        )
        self.fleet("ask", "demo", "是否发布？", "--option", "发布", "--option", "暂缓")
        values = json.loads(self.fleet("attention", "--json").stdout)
        self.assertEqual(values[0]["kind"], "decision_required")
        self.assertIn("stale_blocked", [item["kind"] for item in values])

    def test_socket_event_updates_runtime_but_not_project_state(self):
        socket_path = self.root / "herdr.sock"
        self.env["HERDR_SOCKET_PATH"] = str(socket_path)
        ready = threading.Event()

        def recv_json(connection):
            data = b""
            while not data.endswith(b"\n"):
                part = connection.recv(4096)
                if not part:
                    break
                data += part
            return json.loads(data)

        def server():
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(str(socket_path))
                listener.listen(2)
                ready.set()
                first, _ = listener.accept()
                with first:
                    request = recv_json(first)
                    self.assertEqual(request["method"], "session.snapshot")
                    snapshot = {
                        "id": request["id"],
                        "result": {
                            "type": "session_snapshot",
                            "snapshot": {
                                "version": "test",
                                "protocol": 20,
                                "workspaces": [],
                                "tabs": [],
                                "panes": [],
                                "layouts": [],
                                "agents": [
                                    {
                                        "name": "demo-cmd",
                                        "pane_id": "w1:p1",
                                        "agent_status": "idle",
                                    }
                                ],
                            },
                        },
                    }
                    first.sendall((json.dumps(snapshot) + "\n").encode())

                second, _ = listener.accept()
                with second:
                    request = recv_json(second)
                    self.assertEqual(request["method"], "events.subscribe")
                    self.assertIn(
                        {
                            "type": "pane.agent_status_changed",
                            "pane_id": "w1:p1",
                        },
                        request["params"]["subscriptions"],
                    )
                    second.sendall(
                        (
                            json.dumps(
                                {
                                    "id": request["id"],
                                    "result": {"type": "events_subscribed"},
                                }
                            )
                            + "\n"
                        ).encode()
                    )
                    second.sendall(
                        (
                            json.dumps(
                                {
                                    "event": "pane.agent_status_changed",
                                    "data": {
                                        "pane_id": "w1:p1",
                                        "workspace_id": "w1",
                                        "agent_status": "working",
                                    },
                                }
                            )
                            + "\n"
                        ).encode()
                    )
                    time.sleep(2)

        thread = threading.Thread(target=server, daemon=True)
        thread.start()
        self.assertTrue(ready.wait(2))
        watcher = subprocess.Popen(
            [str(FLEET), "watch"],
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            deadline = time.time() + 3
            while time.time() < deadline:
                runtime_path = self.data / "runtime.json"
                if runtime_path.exists():
                    runtime = json.loads(runtime_path.read_text())
                    if runtime.get("projects", {}).get("demo", {}).get(
                        "live_status"
                    ) == "working":
                        break
                time.sleep(0.05)
            else:
                self.fail("事件未在期限内写入 runtime.json")

            entry = self.load("fleets.json")["demo"]
            self.assertEqual(entry["status"], "idle")
            self.assertEqual(entry["reported_status"], "idle")
        finally:
            watcher.terminate()
            watcher.wait(timeout=3)
            thread.join(timeout=3)


if __name__ == "__main__":
    unittest.main()

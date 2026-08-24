import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
FLEET = ROOT / "fleet"


class FleetTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.data = Path(self.temp.name) / "state"
        self.env = os.environ.copy()
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
            str(self.temp.name),
        )

    def tearDown(self):
        self.temp.cleanup()

    def fleet(self, *args, check=True):
        return subprocess.run(
            [str(FLEET), *args],
            env=self.env,
            text=True,
            capture_output=True,
            check=check,
        )

    def load(self, name):
        return json.loads((self.data / name).read_text())

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

        # 机长可以继续汇报执行状态，但未解决的决策仍是项目的一等状态。
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
        fake_bin = Path(self.temp.name) / "bin"
        fake_bin.mkdir()
        route_log = Path(self.temp.name) / "route.json"
        fake_herdr = fake_bin / "herdr"
        fake_herdr.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "open(os.environ['ROUTE_LOG'], 'w').write(json.dumps(sys.argv[1:]))\n"
        )
        fake_herdr.chmod(0o755)
        self.env["PATH"] = f"{fake_bin}:{self.env['PATH']}"
        self.env["ROUTE_LOG"] = str(route_log)

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

    def test_socket_event_updates_runtime_but_not_project_state(self):
        socket_path = Path(self.temp.name) / "herdr.sock"
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
                                "protocol": 19,
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

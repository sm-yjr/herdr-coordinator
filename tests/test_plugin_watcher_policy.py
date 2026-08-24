import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
FLEET = ROOT / "fleet"


class PluginWatcherPolicyTest(unittest.TestCase):
    def test_plugin_mode_does_not_start_compatibility_watcher(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary) / "plugin-state"
            env = os.environ.copy()
            env["HERDR_PLUGIN_STATE_DIR"] = str(state)
            env["HERDR_PLUGIN_ID"] = "sm-yjr.herdr-coordinator"
            subprocess.run(
                [str(FLEET), "init"],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )
            started = subprocess.run(
                [str(FLEET), "watch-start"],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )
            self.assertIn("无独立 watcher", started.stdout)
            self.assertFalse((state / "watch.pid").exists())
            status = subprocess.run(
                [str(FLEET), "watch-status"],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )
            self.assertIn("Plugin 事件模式", status.stdout)

    def test_plugin_mode_rejects_foreground_watch(self):
        with tempfile.TemporaryDirectory() as temporary:
            env = os.environ.copy()
            env["HERDR_PLUGIN_STATE_DIR"] = str(Path(temporary) / "plugin-state")
            result = subprocess.run(
                [str(FLEET), "watch"],
                env=env,
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("无需 fleet watch", result.stderr)


if __name__ == "__main__":
    unittest.main()

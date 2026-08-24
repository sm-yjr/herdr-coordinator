import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
FLEET = ROOT / "fleet"


class WatcherEntrypointTest(unittest.TestCase):
    def test_foreground_watch_is_visible_to_watch_status(self):
        with tempfile.TemporaryDirectory() as temporary:
            data = Path(temporary) / "state"
            env = os.environ.copy()
            env["HERDR_COORDINATOR_HOME"] = str(data)
            env["HERDR_SOCKET_PATH"] = str(Path(temporary) / "missing.sock")
            subprocess.run(
                [str(FLEET), "init"],
                env=env,
                text=True,
                capture_output=True,
                check=True,
            )
            watcher = subprocess.Popen(
                [str(FLEET), "watch"],
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                deadline = time.time() + 3
                while time.time() < deadline and not (data / "watch.pid").exists():
                    time.sleep(0.05)
                self.assertTrue((data / "watch.pid").exists())
                status = subprocess.run(
                    [str(FLEET), "watch-status"],
                    env=env,
                    text=True,
                    capture_output=True,
                    check=True,
                )
                self.assertIn("事件监听器运行中", status.stdout)
            finally:
                watcher.terminate()
                watcher.wait(timeout=3)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Check launcher failure boundaries without booting or accessing a device."""
import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("start_switchvisor", ROOT / "scripts/start-switchvisor.py")
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)
WAIT_CONSOLE = launcher.wait_console


class LauncherTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.payload = self.root / "hekate payload.bin"
        self.payload.write_bytes(b"payload")
        self.bundle = self.root / "guest bundle.json"
        self.bundle.write_text("{}")
        self.log = self.root / "logs/console.log"
        self.argv = [str(self.payload), "--bundle", str(self.bundle), "--log", str(self.log)]
        for owner, name, kwargs in (
            (launcher.platform, "system", {"return_value": "Darwin"}),
            (launcher.shutil, "which", {"side_effect": lambda name: "/tools/" + name}),
            (launcher.subprocess, "run", {}),
            (launcher.os, "execv", {}),
            (launcher, "wait_console", {}),
        ):
            self.enterContext(patch.object(owner, name, **kwargs))

    def invoke(self):
        with contextlib.redirect_stdout(io.StringIO()):
            launcher.main(self.argv)

    def test_success_opens_guest_console_and_captures_log(self):
        self.argv += ["--console", "/dev/cu.custom-guest"]
        self.invoke()
        self.assertEqual(launcher.subprocess.run.call_args_list[0].args[0],
                         ["/tools/nxboot", "--hekate", "id", "SCR-SWV", str(self.payload)])
        self.assertEqual(launcher.subprocess.run.call_args_list[1].args[0],
                         ["/tools/switchvisorctl", "deploy", str(self.bundle)])
        self.assertEqual(launcher.os.execv.call_args.args,
                         ("/tools/minicom", ["/tools/minicom", "-D", "/dev/cu.custom-guest", "-b", "115200", "-8", "-C", str(self.log)]))
        self.assertTrue(self.log.is_file())

    def test_missing_bundle_does_not_inject(self):
        self.bundle.unlink()
        with self.assertRaises(FileNotFoundError):
            self.invoke()
        launcher.subprocess.run.assert_not_called()

    def test_missing_tool_does_not_inject(self):
        launcher.shutil.which.side_effect = lambda name: None if name == "minicom" else "/tools/" + name
        with self.assertRaisesRegex(ValueError, "missing minicom"):
            self.invoke()
        launcher.subprocess.run.assert_not_called()

    def test_console_timeout_does_not_deploy(self):
        launcher.wait_console.side_effect = ValueError("console timeout")
        with self.assertRaisesRegex(ValueError, "console timeout"):
            self.invoke()
        self.assertEqual(launcher.subprocess.run.call_count, 1)
        launcher.os.execv.assert_not_called()

    def test_deploy_failure_does_not_open_console_or_retry(self):
        launcher.subprocess.run.side_effect = [None, subprocess.CalledProcessError(1, "deploy")]
        with self.assertRaises(subprocess.CalledProcessError):
            self.invoke()
        self.assertEqual(launcher.subprocess.run.call_count, 2)
        launcher.os.execv.assert_not_called()

    def test_port_wait_is_bounded_and_rejects_regular_files(self):
        with patch.object(launcher.time, "monotonic", side_effect=[0, 0, 60]), \
                patch.object(launcher.time, "sleep") as sleep:
            with self.assertRaisesRegex(ValueError, "console did not appear"):
                WAIT_CONSOLE(self.payload, 60)
            sleep.assert_called_once_with(0.2)

    def test_port_wait_handles_delayed_enumeration(self):
        with patch.object(Path, "is_char_device", side_effect=[False, True]), \
                patch.object(launcher.time, "sleep") as sleep:
            WAIT_CONSOLE(launcher.CONSOLE, 60)
            sleep.assert_called_once()

    def test_injection_failure_does_not_deploy(self):
        launcher.subprocess.run.side_effect = subprocess.CalledProcessError(1, "nxboot")
        with self.assertRaises(subprocess.CalledProcessError):
            self.invoke()
        self.assertEqual(launcher.subprocess.run.call_count, 1)
        launcher.wait_console.assert_not_called()
        launcher.os.execv.assert_not_called()


if __name__ == "__main__":
    unittest.main()

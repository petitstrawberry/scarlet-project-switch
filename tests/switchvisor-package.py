#!/usr/bin/env python3
"""Check USB package profiles and custom distributions without hardware."""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("package_switchvisor", ROOT / "scripts/package-switchvisor.py")
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class SwitchvisorPackageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.project = Path(temporary.name) / "project"
        self.console = self.project / ".scarlet/l4t"
        self.output = self.project / ".scarlet/switchvisor"
        self.dist = Path(temporary.name) / "custom-dist"
        self.dist.mkdir()
        pins = {}
        hashes = {}
        for name in ("bl31.bin", "bl33.bin", "nx-plat.dtimg", "uImage", "initramfs"):
            relative = f"{package.SOURCE_DIRECTORY}/{name}"
            source = self.console / relative
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_bytes(name.encode())
            hashes[relative] = package.digest(source)
            if name in ("bl31.bin", "bl33.bin", "nx-plat.dtimg"):
                pins[name] = hashes[relative]
                bootstack = self.project / ".scarlet/bootstack" / name
                bootstack.parent.mkdir(parents=True, exist_ok=True)
                bootstack.write_bytes(source.read_bytes())
        (self.project / "bootstack.json").write_text(json.dumps({"files": pins}))
        (self.console / "manifest.json").write_text(json.dumps({
            "sha256": hashes, "kernel_elf_sha256": "test-kernel",
        }))
        for name in ("boot.cmd", "L4T-scarlet-switchvisor.ini"):
            path = self.project / "bootloader" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes((package.PROJECT / "bootloader" / name).read_bytes())
        (self.dist / "bl33.bin").write_bytes(b"test monitor")
        for name in ("usb-uart.dtbo", "usb-net.dtbo"):
            (self.dist / name).write_bytes(name.encode())
        self.manifest = {
            "stage2_enabled": True,
            "usb_uart": {"enabled": True},
            "usb_control": {"enabled": True, "packaged_payload_fallback": False},
            "usb_gdb": {"enabled": False},
            "usb_net": {"enabled": False},
            "payload": {"sha256": pins["bl33.bin"]},
            "bootstack": {"files": {name: {"sha256": digest} for name, digest in pins.items()}},
            "sha256": package.digest(self.dist / "bl33.bin"),
        }
        for name, value in {"PROJECT": self.project, "CONSOLE": self.console, "OUTPUT": self.output}.items():
            replacement = patch.object(package, name, value)
            replacement.start()
            self.addCleanup(replacement.stop)

    def invoke(self):
        (self.dist / "manifest.json").write_text(json.dumps(self.manifest))
        with contextlib.redirect_stdout(io.StringIO()):
            package.package_distribution(self.dist)
        return json.loads((self.output / "manifest.json").read_text())

    def snapshot(self):
        return {str(path.relative_to(self.output)): package.digest(path)
                for path in self.output.rglob("*") if path.is_file()}

    def test_custom_distribution_needs_no_git_checkout(self):
        result = self.invoke()
        self.assertIsNone(result["switchvisor_checkout_revision"])
        self.assertEqual(result["switchvisor_source"], {})
        for path, digest in result["sha256"].items():
            self.assertEqual(package.digest(self.output / path), digest)
        bundle = json.loads((self.output / "bundle.json").read_text())
        for image in bundle["images"]:
            self.assertTrue((self.output / image["path"]).is_file())

    def test_network_to_uart_removes_stale_overlay_and_records_source(self):
        self.manifest["source"] = {"git": "https://example.invalid/switchvisor", "rev": "a" * 40}
        self.manifest["usb_net"]["enabled"] = True
        self.invoke()
        boot = self.output / package.BOOT_DIRECTORY
        self.assertTrue((boot / "usb-net.dtbo").is_file())
        self.assertIn(b"setenv scarlet_switchvisor_net 1\n", (boot / "boot.scr").read_bytes())
        self.manifest["usb_net"]["enabled"] = False
        result = self.invoke()
        self.assertFalse((boot / "usb-net.dtbo").exists())
        self.assertIn(b"setenv scarlet_switchvisor_net 0\n", (boot / "boot.scr").read_bytes())
        self.assertEqual(result["switchvisor_source"], self.manifest["source"])
        self.assertEqual(result["switchvisor_checkout_revision"], "a" * 40)

    def test_missing_network_overlay_preserves_previous_package(self):
        self.invoke()
        before = self.snapshot()
        self.manifest["usb_net"]["enabled"] = True
        (self.dist / "usb-net.dtbo").unlink()
        with self.assertRaisesRegex(ValueError, "missing Switchvisor overlay"):
            self.invoke()
        self.assertEqual(self.snapshot(), before)

    def test_invalid_profile_or_inputs_preserve_previous_package(self):
        self.invoke()
        before = self.snapshot()
        original = copy.deepcopy(self.manifest)
        changes = [
            ("usb_control", "packaged_payload_fallback", True),
            ("usb_gdb", "enabled", True),
            ("payload", "sha256", "wrong hash"),
            ("bootstack", "files", {"bl31.bin": {"sha256": "wrong hash"}}),
        ]
        for section, field, value in changes:
            with self.subTest(section=section, field=field):
                self.manifest = copy.deepcopy(original)
                self.manifest[section][field] = value
                with self.assertRaises(ValueError):
                    self.invoke()
                self.assertEqual(self.snapshot(), before)
        self.manifest = original
        (self.dist / "bl33.bin").write_bytes(b"corrupted monitor")
        with self.assertRaisesRegex(ValueError, "mismatched input"):
            self.invoke()
        self.assertEqual(self.snapshot(), before)


if __name__ == "__main__":
    unittest.main()

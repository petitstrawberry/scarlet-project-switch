#!/usr/bin/env python3
"""Reject incomplete, corrupted, and native-ABI packages before SD bundling."""
import hashlib
import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from linux_vulkan_package import FILES, validate_build


class PackageTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.output = Path(self.temporary.name).resolve()
        self.rootfs = self.output / "rootfs"
        self.report = {"result": "PASS", "target": "aarch64-unknown-linux-gnu",
                       "backends": ["virtio-gpu", "nvidia-gm20b"], "artifacts": []}
        for name in sorted(FILES):
            path = self.rootfs / name
            path.parent.mkdir(parents=True, exist_ok=True)
            data = bytearray(64) if name.endswith(".so") else bytearray(b"manifest")
            if name.endswith(".so"):
                data[:8] = b"\x7fELF\x02\x01\x01\x00"
                struct.pack_into("<HH", data, 16, 3, 183)
            path.write_bytes(data)
            self.report["artifacts"].append({"path": name, "sha256": hashlib.sha256(data).hexdigest()})
        self.save_report()

    def save_report(self):
        (self.output / "build.json").write_text(json.dumps(self.report))

    def test_complete_linux_package(self):
        self.assertEqual(validate_build(self.output), self.rootfs)

    def test_corrupted_driver_is_rejected(self):
        (self.rootfs / "usr/lib/sgfx/libsgfx_scarlet_maxwell.so").write_bytes(b"corrupted")
        with self.assertRaisesRegex(ValueError, "differs from build report"):
            validate_build(self.output)

    def test_native_plugin_cannot_be_substituted_even_with_updated_checksum(self):
        item = next(item for item in self.report["artifacts"] if item["path"].endswith("maxwell.so"))
        path = self.rootfs / item["path"]
        data = bytearray(path.read_bytes())
        data[7] = 83
        path.write_bytes(data)
        item["sha256"] = hashlib.sha256(data).hexdigest()
        self.save_report()
        with self.assertRaisesRegex(ValueError, "not a Linux ELF64"):
            validate_build(self.output)

    def test_missing_maxwell_is_rejected(self):
        self.report["backends"] = ["virtio-gpu"]
        self.save_report()
        with self.assertRaisesRegex(ValueError, "including the Maxwell plugin"):
            validate_build(self.output)

    def test_incomplete_and_escaping_artifact_lists_are_rejected(self):
        self.report["artifacts"].pop()
        self.save_report()
        with self.assertRaisesRegex(ValueError, "artifact list"):
            validate_build(self.output)
        self.report["artifacts"].append({"path": "../../outside.so", "sha256": "0" * 64})
        self.save_report()
        with self.assertRaisesRegex(ValueError, "artifact list"):
            validate_build(self.output)


if __name__ == "__main__":
    unittest.main()

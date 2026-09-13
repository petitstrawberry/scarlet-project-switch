#!/usr/bin/env python3
"""Verify SD file-install boundaries without a physical SD or raw device."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("install_sd", ROOT / "scripts/install-sd.py")
sd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sd)


class SdInstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.volume = self.root / "volume"
        self.volume.mkdir()
        self.package = self.root / "generated/l4t"
        self.package.mkdir(parents=True)
        for relative in sd.PROTECTED:
            path = self.volume / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"existing recovery configuration")
        self.hashes = {}
        for relative in ["switchroot/scarlet/uImage", "bootloader/ini/L4T-scarlet.ini"]:
            path = self.package / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"new Scarlet boot artifact")
            self.hashes[relative] = sd.digest(path)
        self.manifest()

    def manifest(self):
        (self.package / "manifest.json").write_text(json.dumps({"sha256": self.hashes}))

    def invoke(self, write=False):
        argv = ["install-sd.py", "--mount", str(self.volume)] + (["--write"] if write else [])
        with patch.object(sd, "PACKAGE", self.package), \
                patch.object(sd, "validate_mount", return_value="disk99s1"), \
                patch("sys.argv", argv), contextlib.redirect_stdout(io.StringIO()):
            sd.main()

    def test_dry_run_and_verified_write_preserve_recovery_files(self):
        before = sd.protected_hashes(self.volume)
        self.invoke()
        self.assertFalse((self.volume / "switchroot/scarlet").exists())
        self.invoke(write=True)
        for relative, expected in self.hashes.items():
            self.assertEqual(sd.digest(self.volume / relative), expected)
        self.assertEqual(sd.protected_hashes(self.volume), before)
        receipt = json.loads((self.package.parent / "sd-installation.json").read_text())
        self.assertFalse(receipt["raw_device_accessed"])
        self.assertFalse(receipt["hardware_boot_validated"])

    def test_checksum_failure_precedes_any_write(self):
        (self.package / "switchroot/scarlet/uImage").write_bytes(b"corrupted")
        with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
            self.invoke(write=True)
        self.assertFalse((self.volume / "switchroot/scarlet").exists())

    def test_rejects_recovery_destination_and_path_traversal(self):
        for relative in ["bootloader/hekate_ipl.ini", "switchroot/scarlet/../../escape"]:
            with self.subTest(relative=relative):
                self.hashes = {relative: hashlib.sha256(b"payload").hexdigest()}
                self.manifest()
                with self.assertRaises(ValueError):
                    self.invoke(write=True)
        self.assertFalse((self.volume / "switchroot/scarlet").exists())

    def disk_metadata(self):
        info = {"MountPoint": str(self.volume), "FilesystemType": "msdos",
                "FilesystemName": "MS-DOS FAT32", "Content": "Windows_FAT_32",
                "DeviceIdentifier": "disk99s1", "ParentWholeDisk": "disk99"}
        disk = {"DeviceIdentifier": "disk99", "Content": "FDisk_partition_scheme",
                "Size": sd.EXPECTED_BYTES, "Partitions": [
                    {"DeviceIdentifier": f"disk99s{n}", "Size": size,
                     "Content": "Windows_FAT_32" if n == 1 else "Linux"}
                    for n, size in sd.EXPECTED_PARTITIONS.items()]}
        return info, disk

    def test_layout_validation_rejects_wrong_disk_or_partition(self):
        info, disk = self.disk_metadata()
        def diskutil(*args):
            return info if args[0] == "info" else {"AllDisksAndPartitions": [disk]}
        with patch.object(sd.os, "uname", return_value=SimpleNamespace(sysname="Darwin")), \
                patch.object(sd, "diskutil", side_effect=diskutil):
            self.assertEqual(sd.validate_mount(self.volume), "disk99s1")
            disk["Partitions"][3]["Size"] -= 512
            with self.assertRaisesRegex(ValueError, "partition #4"):
                sd.validate_mount(self.volume)
            disk["Partitions"][3]["Size"] += 512
            disk["Size"] -= 512
            with self.assertRaisesRegex(ValueError, "capacity"):
                sd.validate_mount(self.volume)
            disk["Size"] += 512
            info["DeviceIdentifier"] = "disk99s2"
            with self.assertRaisesRegex(ValueError, "partition #1"):
                sd.validate_mount(self.volume)

    def test_fat32_validation_uses_filesystem_not_partition_tag(self):
        info, disk = self.disk_metadata()
        def diskutil(*args):
            return info if args[0] == "info" else {"AllDisksAndPartitions": [disk]}
        with patch.object(sd.os, "uname", return_value=SimpleNamespace(sysname="Darwin")), \
                patch.object(sd, "diskutil", side_effect=diskutil):
            for content in ["Windows_FAT_32", "DOS_FAT_32"]:
                with self.subTest(content=content):
                    info["Content"] = content
                    disk["Partitions"][0]["Content"] = content
                    self.assertEqual(sd.validate_mount(self.volume), "disk99s1")
            for filesystem in ["MS-DOS FAT12", "MS-DOS FAT16", "ExFAT", "APFS", None]:
                with self.subTest(filesystem=filesystem):
                    info["FilesystemName"] = filesystem
                    with self.assertRaisesRegex(ValueError, "filesystem is not FAT32"):
                        sd.validate_mount(self.volume)


if __name__ == "__main__":
    unittest.main()

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

rootfs_spec = importlib.util.spec_from_file_location("install_rootfs", ROOT / "scripts/install-rootfs.py")
rootfs = importlib.util.module_from_spec(rootfs_spec)
rootfs_spec.loader.exec_module(rootfs)


class RootfsDiskTests(unittest.TestCase):
    def test_removable_sd_in_internal_reader_keeps_layout_checks(self):
        whole = {"WholeDisk": True, "Internal": True, "RemovableMedia": True,
                 "VirtualOrPhysical": "Physical", "Content": "FDisk_partition_scheme",
                 "Size": rootfs.DISK_BYTES, "DeviceBlockSize": 512,
                 "DeviceIdentifier": "disk99"}
        parts = {
            f"/dev/disk99s{i}": {"ParentWholeDisk": "disk99",
                                 "PartitionMapPartitionOffset": first * 512,
                                 "Size": count * 512}
            for i, (_, first, count) in enumerate(rootfs.PARTITIONS, 1)
        }
        with patch.object(rootfs.os, "uname", return_value=SimpleNamespace(sysname="Darwin")), \
                patch.object(rootfs, "disk_info", side_effect=lambda device: whole if device == "/dev/disk99" else parts[device]):
            self.assertEqual(len(rootfs.validate_disk("/dev/disk99")), 4)
            parts["/dev/disk99s4"]["PartitionMapPartitionOffset"] += 512
            with self.assertRaisesRegex(ValueError, "partition 4"):
                rootfs.validate_disk("/dev/disk99")
            whole["RemovableMedia"] = False
            with self.assertRaisesRegex(ValueError, "capacity, sector size, or MBR"):
                rootfs.validate_disk("/dev/disk99")


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
        for relative in ["switchroot/scarlet-console/uImage", "bootloader/ini/L4T-scarlet-console.ini"]:
            path = self.package / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"new Scarlet boot artifact")
            self.hashes[relative] = sd.digest(path)
        self.manifest()

    def manifest(self):
        (self.package / "manifest.json").write_text(json.dumps({"sha256": self.hashes}))

    def invoke(self, write=False, console=False, switchvisor=False, l4t=False):
        argv = ["install-sd.py", "--mount", str(self.volume)]
        if write:
            argv.append("--write")
        if console:
            argv.append("--console")
        if switchvisor:
            argv.append("--switchvisor")
        if l4t:
            argv.append("--l4t")
        with patch.object(sd, "CONSOLE_PACKAGE", self.package), \
                patch.object(sd, "SWITCHVISOR_PACKAGE", getattr(self, "switchvisor_package", self.package)), \
                patch.object(sd, "validate_mount", return_value="disk99s1"), \
                patch("sys.argv", argv), contextlib.redirect_stdout(io.StringIO()):
            sd.main()

    def test_dry_run_and_verified_write_preserve_recovery_files(self):
        before = sd.protected_hashes(self.volume)
        self.invoke()
        self.assertFalse((self.volume / "switchroot/scarlet-console").exists())
        self.invoke(write=True)
        for relative, expected in self.hashes.items():
            self.assertEqual(sd.digest(self.volume / relative), expected)
        self.assertEqual(sd.protected_hashes(self.volume), before)
        receipt = json.loads((self.package.parent / "sd-installation.json").read_text())
        self.assertFalse(receipt["raw_device_accessed"])
        self.assertFalse(receipt["hardware_boot_validated"])

    def test_checksum_failure_precedes_any_write(self):
        (self.package / "switchroot/scarlet-console/uImage").write_bytes(b"corrupted")
        with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
            self.invoke(write=True)
        self.assertFalse((self.volume / "switchroot/scarlet-console").exists())

    def test_console_install_preserves_the_working_diagnostic_entry(self):
        diagnostic = ["switchroot/scarlet/uImage", "bootloader/ini/L4T-scarlet.ini"]
        self.hashes = {}
        for relative in diagnostic:
            old = self.volume / relative
            old.parent.mkdir(parents=True, exist_ok=True)
            old.write_bytes(b"hardware-tested diagnostic")
            console = relative.replace("switchroot/scarlet/", "switchroot/scarlet-console/").replace("L4T-scarlet.ini", "L4T-scarlet-console.ini")
            source = self.package / console
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_bytes(b"SWS console artifact")
            self.hashes[console] = sd.digest(source)
        self.manifest()
        self.invoke(write=True, console=True)
        for relative in diagnostic:
            self.assertEqual((self.volume / relative).read_bytes(), b"hardware-tested diagnostic")
        for relative, expected in self.hashes.items():
            self.assertEqual(sd.digest(self.volume / relative), expected)
        # A console package cannot smuggle in a write to the known-good entry.
        (self.package / diagnostic[0]).parent.mkdir(parents=True, exist_ok=True)
        (self.package / diagnostic[0]).write_bytes(b"unexpected diagnostic artifact")
        self.hashes[diagnostic[0]] = sd.digest(self.package / diagnostic[0])
        self.manifest()
        with self.assertRaisesRegex(ValueError, "unexpected package destination"):
            self.invoke(write=True, console=True)

    def test_rejects_recovery_destination_and_path_traversal(self):
        for relative in ["bootloader/hekate_ipl.ini", "switchroot/scarlet-console/../../escape"]:
            with self.subTest(relative=relative):
                self.hashes = {relative: hashlib.sha256(b"payload").hexdigest()}
                self.manifest()
                with self.assertRaises(ValueError):
                    self.invoke(write=True)
        self.assertFalse((self.volume / "switchroot/scarlet-console").exists())

    def test_default_console_preserves_switchvisor_without_legacy_probe(self):
        existing = ["bootloader/ini/L4T-scarlet-switchvisor.ini",
                    "switchroot/scarlet-switchvisor/bl33.bin",
                    "bootloader/ini/L4T-scarlet-switchvisor-dev.ini",
                    "switchroot/scarlet-switchvisor-dev/bl33.bin"]
        for relative in existing:
            path = self.volume / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"existing Switchvisor artifact")
        self.invoke(write=True)
        self.assertFalse((self.volume / "switchroot/scarlet").exists())
        for relative in existing:
            self.assertEqual((self.volume / relative).read_bytes(), b"existing Switchvisor artifact")
        for relative, expected in self.hashes.items():
            self.assertEqual(sd.digest(self.volume / relative), expected)

    def test_switchvisor_install_preserves_existing_scarlet_entries(self):
        existing = [
            "bootloader/ini/L4T-scarlet.ini", "switchroot/scarlet/uImage",
            "bootloader/ini/L4T-scarlet-console.ini",
            "switchroot/scarlet-console/uImage",
            "switchroot/scarlet-console-logs/boot.scr",
        ]
        for relative in existing:
            path = self.volume / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"existing Scarlet entry")
        self.hashes = {}
        for relative in ["bootloader/ini/L4T-scarlet-switchvisor.ini",
                         "switchroot/scarlet-switchvisor/bl33.bin",
                         "bootloader/ini/L4T-scarlet-switchvisor-dev.ini",
                         "switchroot/scarlet-switchvisor-dev/bl33.bin"]:
            path = self.package / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"Switchvisor USB entry")
            self.hashes[relative] = sd.digest(path)
        self.manifest()
        self.invoke(write=True, switchvisor=True)
        for relative in existing:
            self.assertEqual((self.volume / relative).read_bytes(), b"existing Scarlet entry")
        for relative, expected in self.hashes.items():
            self.assertEqual(sd.digest(self.volume / relative), expected)

    def prepare_switchvisor_pair(self):
        self.switchvisor_package = self.root / "generated/switchvisor"
        hashes = {}
        for suffix in ("", "-dev"):
            for relative in (f"bootloader/ini/L4T-scarlet-switchvisor{suffix}.ini",
                             f"switchroot/scarlet-switchvisor{suffix}/bl33.bin"):
                path = self.switchvisor_package / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(relative.encode())
                hashes[relative] = sd.digest(path)
        dependencies = {}
        for name in ("uImage", "initramfs"):
            relative = f"switchroot/scarlet-console/{name}"
            path = self.package / relative
            path.write_bytes(name.encode())
            self.hashes[relative] = sd.digest(path)
            dependencies[relative] = self.hashes[relative]
        self.manifest()
        manifest = {"sha256": hashes, "sd_boot_dependencies": dependencies}
        (self.switchvisor_package / "manifest.json").write_text(json.dumps(manifest))
        return manifest

    def test_switchvisor_pair_requires_matching_installed_console_before_writes(self):
        manifest = self.prepare_switchvisor_pair()
        before = sd.protected_hashes(self.volume)
        with self.assertRaisesRegex(ValueError, "SD boot dependency"):
            self.invoke(write=True, switchvisor=True)
        self.assertFalse((self.volume / "switchroot/scarlet-switchvisor").exists())
        self.assertFalse((self.volume / "switchroot/scarlet-switchvisor-dev").exists())
        self.invoke(write=True, console=True)
        installed = self.volume / "switchroot/scarlet-console/initramfs"
        installed.write_bytes(b"different installed version")
        with self.assertRaisesRegex(ValueError, "SD boot dependency"):
            self.invoke(write=True, switchvisor=True)
        self.assertFalse((self.volume / "switchroot/scarlet-switchvisor").exists())
        installed.write_bytes(b"initramfs")
        self.invoke(write=True, switchvisor=True)
        for relative, expected in manifest["sha256"].items():
            self.assertEqual(sd.digest(self.volume / relative), expected)
        self.assertEqual(sd.protected_hashes(self.volume), before)

    def test_combined_install_supplies_console_dependencies_and_both_entries(self):
        manifest = self.prepare_switchvisor_pair()
        self.invoke(write=True, l4t=True)
        for relative, expected in {**self.hashes, **manifest["sha256"]}.items():
            self.assertEqual(sd.digest(self.volume / relative), expected)
        receipt = json.loads((self.switchvisor_package.parent / "sd-installation.json").read_text())
        self.assertEqual(len(receipt["sha256"]), len(self.hashes) + len(manifest["sha256"]))

    def test_corrupt_development_image_prevents_installing_either_entry(self):
        self.prepare_switchvisor_pair()
        before = {str(p.relative_to(self.volume)): sd.digest(p)
                  for p in self.volume.rglob("*") if p.is_file()}
        (self.switchvisor_package / "switchroot/scarlet-switchvisor-dev/bl33.bin").write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
            self.invoke(write=True, l4t=True)
        self.assertEqual(before, {str(p.relative_to(self.volume)): sd.digest(p)
                                  for p in self.volume.rglob("*") if p.is_file()})

    def test_development_destination_allowlist_is_exact(self):
        manifest = self.prepare_switchvisor_pair()
        before = {str(p.relative_to(self.volume)): sd.digest(p)
                  for p in self.volume.rglob("*") if p.is_file()}
        for relative in ("switchroot/scarlet-switchvisor-dev-other/bl33.bin",
                         "switchroot/scarlet-switchvisor-dev/../../escape",
                         "bootloader/ini/L4T-scarlet-switchvisor-other.ini"):
            with self.subTest(relative=relative):
                bad = {**manifest, "sha256": {relative: "untrusted"}}
                (self.switchvisor_package / "manifest.json").write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    self.invoke(write=True, switchvisor=True)
        self.assertEqual(before, {str(p.relative_to(self.volume)): sd.digest(p)
                                  for p in self.volume.rglob("*") if p.is_file()})

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

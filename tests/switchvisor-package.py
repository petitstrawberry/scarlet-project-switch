#!/usr/bin/env python3
"""Check USB package profiles and custom distributions without hardware."""
import contextlib
import copy
import importlib.util
import io
import json
import re
from pathlib import Path
import struct
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import zlib

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
        self.dev_dist = Path(temporary.name) / "custom-dev-dist"
        self.dev_dist.mkdir()
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
                bootstack = self.project / "firmware/bootstack" / name
                bootstack.parent.mkdir(parents=True, exist_ok=True)
                bootstack.write_bytes(source.read_bytes())
        (self.project / "bootstack.json").write_text(json.dumps({"files": pins}))
        (self.console / "manifest.json").write_text(json.dumps({
            "sha256": hashes, "kernel_elf_sha256": "test-kernel",
        }))
        for name in ("boot.cmd", "L4T-scarlet-switchvisor.ini", "L4T-scarlet-switchvisor-dev.ini"):
            path = self.project / "bootloader" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes((package.PROJECT / "bootloader" / name).read_bytes())
        (self.dist / "bl33.bin").write_bytes(b"test monitor")
        (self.dev_dist / "bl33.bin").write_bytes(b"test development monitor")
        for directory in (self.dist, self.dev_dist):
            for name in ("usb-uart.dtbo", "usb-net.dtbo"):
                (directory / name).write_bytes(name.encode())
        self.manifest = {
            "stage2_enabled": True,
            "usb_uart": {"enabled": True},
            "usb_control": {"enabled": True, "packaged_payload_fallback": True},
            "usb_gdb": {"enabled": False},
            "usb_net": {"enabled": False},
            "payload": {"sha256": pins["bl33.bin"]},
            "bootstack": {"files": {name: {"sha256": digest} for name, digest in pins.items()}},
            "sha256": package.digest(self.dist / "bl33.bin"),
        }
        self.dev_manifest = copy.deepcopy(self.manifest)
        self.dev_manifest["usb_control"]["packaged_payload_fallback"] = False
        self.dev_manifest["sha256"] = package.digest(self.dev_dist / "bl33.bin")
        for name, value in {"PROJECT": self.project, "CONSOLE": self.console, "OUTPUT": self.output}.items():
            replacement = patch.object(package, name, value)
            replacement.start()
            self.addCleanup(replacement.stop)

    def invoke(self):
        (self.dist / "manifest.json").write_text(json.dumps(self.manifest))
        (self.dev_dist / "manifest.json").write_text(json.dumps(self.dev_manifest))
        with contextlib.redirect_stdout(io.StringIO()):
            package.package_distribution(self.dist, self.dev_dist)
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
        for relative, digest in result["sd_boot_dependencies"].items():
            self.assertEqual(package.digest(self.console / relative), digest)

    def test_pack_commands_select_fallback_only_for_normal_entry(self):
        data = self.dist / "data"
        data.mkdir()
        (data / "source.json").write_text("{}")
        for name in ("usb-uart.dtbo", "usb-net.dtbo"):
            (data / name).write_bytes(b"overlay")
        with patch.dict(package.os.environ, {"SWITCHVISOR_DATA": str(data)}), \
                patch.object(package.subprocess, "run", return_value=SimpleNamespace(stdout="{}")) as run:
            for development in (False, True):
                package.build_distribution(self.dist / str(development), True, development=development)
                command = run.call_args.args[0]
                self.assertEqual("--no-fallback" in command, development)
                self.assertIn("--usb-net", command)
                self.assertIn("--usb-uart", command)
                self.assertIn("--usb-control", command)

    def script(self, development=False):
        directory = package.DEV_BOOT_DIRECTORY if development else package.BOOT_DIRECTORY
        data = (self.output / directory / "boot.scr").read_bytes()
        header = bytearray(data[:64])
        magic, crc, _, size, _, _, data_crc = struct.unpack(">7I", header[:28])
        header[4:8] = bytes(4)
        self.assertEqual(magic, 0x27051956)
        self.assertEqual(zlib.crc32(header), crc)
        self.assertEqual(len(data) - 64, size)
        self.assertEqual(zlib.crc32(data[64:]), data_crc)
        script_size, terminator = struct.unpack(">II", data[64:72])
        self.assertEqual(terminator, 0)
        self.assertEqual(script_size, len(data) - 72)
        return data[72:].decode()

    def execute_script(self, script, prefix, fail_load="", fail_overlay=False):
        # Execute the script's shell control flow with firmware commands stubbed.
        # FDT <cells> are U-Boot syntax; quote them for the host shell. This does
        # not emulate U-Boot, storage hardware, or the actual DTB transformations.
        script = re.sub(r"<([^>\n]+)>", r"'<\1>'", script)
        harness = r'''
setenv() { name=$1; shift; export "$name=$*"; }
load() { printf 'LOAD %s\n' "$*"; test "$4" != "$FAIL_LOAD"; }
fdt() { printf 'FDT %s\n' "$*"; test "$1:$FAIL_OVERLAY" != apply:1; }
dtimg() { setenv fdtrsize 65536; }
sleep() { :; }
echoe() { echo "$@"; }
reset() { echo RESET; exit 1; }
bootm() { printf 'BOOT %s\nARGS %s\n' "$*" "$bootargs"; exit 0; }
'''
        env = {"PATH": "/usr/bin:/bin", "prefix": prefix, "t210b01": "0", "sku": "0",
               "devnum": "0", "distro_bootpart": "1", "FAIL_LOAD": fail_load,
               "FAIL_OVERLAY": str(int(fail_overlay))}
        return subprocess.run(["/bin/sh"], input=harness + script, env=env,
                              text=True, capture_output=True)

    def test_entries_boot_sd_or_uploaded_images_with_the_same_device_ownership(self):
        self.manifest["usb_net"]["enabled"] = True
        self.dev_manifest["usb_net"]["enabled"] = True
        self.invoke()
        for development, directory, entry, ident in (
            (False, package.BOOT_DIRECTORY, package.ENTRY_FILE, "SCR-SWV"),
            (True, package.DEV_BOOT_DIRECTORY, package.DEV_ENTRY_FILE, "SCR-SWD"),
        ):
            with self.subTest(development=development):
                ini = (self.output / entry).read_text()
                self.assertIn(f"id={ident}\n", ini)
                self.assertIn(f"boot_prefixes=/{directory}/\n", ini)
                result = self.execute_script(self.script(development), "/" + directory)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                for name in ("uImage", "initramfs"):
                    self.assertEqual(f"/{package.SOURCE_DIRECTORY}/{name}" in result.stdout, not development)
                for name in ("nx-plat.dtimg", "usb-net.dtbo", "usb-uart.dtbo"):
                    self.assertIn(f"/{directory}/{name}", result.stdout)
                self.assertEqual(result.stdout.count("FDT apply "), 2)
                self.assertNotIn("scarlet,usb-host", result.stdout)
                self.assertIn("BOOT 0xA0000000 0x92000000 0x8d000000", result.stdout)
                self.assertIn("root=/dev/mmcblk0p4", result.stdout)
                self.assertNotIn("init.console=/dev/null", result.stdout)

    def test_direct_console_keeps_sd_reads_and_physical_usb(self):
        result = self.execute_script((self.project / "bootloader/boot.cmd").read_text(),
                                     "/" + package.SOURCE_DIRECTORY)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"/{package.SOURCE_DIRECTORY}/uImage", result.stdout)
        self.assertIn("scarlet,usb-host", result.stdout)
        self.assertNotIn("FDT apply ", result.stdout)
        self.assertIn("init.console=/dev/null", result.stdout)

    def test_missing_sd_image_or_overlay_never_enters_kernel(self):
        self.invoke()
        for path in (f"/{package.SOURCE_DIRECTORY}/uImage",
                     f"/{package.SOURCE_DIRECTORY}/initramfs",
                     f"/{package.BOOT_DIRECTORY}/usb-uart.dtbo"):
            with self.subTest(path=path):
                result = self.execute_script(self.script(), "/" + package.BOOT_DIRECTORY, fail_load=path)
                self.assertEqual(result.returncode, 1)
                self.assertNotIn("BOOT ", result.stdout)
        for development in (False, True):
            result = self.execute_script(self.script(development), "/boot", fail_overlay=True)
            self.assertEqual(result.returncode, 1)
            self.assertNotIn("BOOT ", result.stdout)

    def test_network_to_uart_removes_stale_overlay_and_records_source(self):
        self.manifest["source"] = {"git": "https://example.invalid/switchvisor", "rev": "a" * 40}
        self.manifest["usb_net"]["enabled"] = True
        self.dev_manifest["usb_net"]["enabled"] = True
        self.invoke()
        boot = self.output / package.BOOT_DIRECTORY
        self.assertTrue((boot / "usb-net.dtbo").is_file())
        self.assertIn(b"setenv scarlet_switchvisor_net 1\n", (boot / "boot.scr").read_bytes())
        self.manifest["usb_net"]["enabled"] = False
        self.dev_manifest["usb_net"]["enabled"] = False
        result = self.invoke()
        self.assertFalse((boot / "usb-net.dtbo").exists())
        self.assertFalse((self.output / package.DEV_BOOT_DIRECTORY / "usb-net.dtbo").exists())
        self.assertIn(b"setenv scarlet_switchvisor_net 0\n", (boot / "boot.scr").read_bytes())
        self.assertEqual(result["switchvisor_source"], self.manifest["source"])
        self.assertEqual(result["switchvisor_checkout_revision"], "a" * 40)

    def test_development_boot_profile_is_explicit_and_defaults_to_sd(self):
        console_manifest = self.console / "manifest.json"
        console = json.loads(console_manifest.read_text())
        console["initramfs_profile"] = "initramfs-dev"
        console_manifest.write_text(json.dumps(console))
        result = self.invoke()
        boot_script = self.output / package.BOOT_DIRECTORY / "boot.scr"
        self.assertEqual(result["initramfs_profile"], "initramfs-dev")
        self.assertIn(b"setenv scarlet_initramfs_dev 1\n", boot_script.read_bytes())
        del console["initramfs_profile"]
        console_manifest.write_text(json.dumps(console))
        result = self.invoke()
        self.assertEqual(result["initramfs_profile"], "sd-root")
        self.assertIn(b"setenv scarlet_initramfs_dev 0\n", boot_script.read_bytes())

    def test_missing_network_overlay_preserves_previous_package(self):
        self.invoke()
        before = self.snapshot()
        self.manifest["usb_net"]["enabled"] = True
        self.dev_manifest["usb_net"]["enabled"] = True
        (self.dev_dist / "usb-net.dtbo").unlink()
        with self.assertRaisesRegex(ValueError, "missing Switchvisor overlay"):
            self.invoke()
        self.assertEqual(self.snapshot(), before)

    def test_invalid_profile_or_inputs_preserve_previous_package(self):
        self.invoke()
        before = self.snapshot()
        original = copy.deepcopy(self.manifest)
        changes = [
            ("usb_control", "packaged_payload_fallback", False),
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

    def test_development_cannot_fall_back_or_use_a_different_network_profile(self):
        self.invoke()
        before = self.snapshot()
        self.dev_manifest["usb_control"]["packaged_payload_fallback"] = True
        with self.assertRaisesRegex(ValueError, "no-fallback"):
            self.invoke()
        self.assertEqual(self.snapshot(), before)
        self.dev_manifest["usb_control"]["packaged_payload_fallback"] = False
        self.dev_manifest["usb_net"]["enabled"] = True
        with self.assertRaisesRegex(ValueError, "same USB network profile"):
            self.invoke()
        self.assertEqual(self.snapshot(), before)


if __name__ == "__main__":
    unittest.main()

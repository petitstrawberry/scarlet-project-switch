#!/usr/bin/env python3
"""Exercise bundle replacement on disposable ext2 files, without an SD device."""
import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("system_update", ROOT / "scripts/update-system-sd.py")
update = importlib.util.module_from_spec(spec)
spec.loader.exec_module(update)


class ReplacementTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="scarlet-bundle-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.image = self.root / "root.ext2"
        with self.image.open("wb") as stream:
            stream.truncate(16 * 1024 * 1024)
        subprocess.run(["mke2fs", "-q", "-t", "ext2", "-F", str(self.image)], check=True)
        self.updater = update.Updater(self.image, "debugfs", self.root, io.StringIO())

    def check_filesystem(self):
        result = subprocess.run(["e2fsck", "-f", "-n", str(self.image)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def source(self, name, data):
        path = self.root / name
        path.write_bytes(data)
        return path

    def test_staged_regular_replacement_keeps_contents_modes_and_link_counts(self):
        before = self.source("before", b"installed bytes")
        after = self.source("after", bytes(range(256)) * 1024)
        self.updater.parents("/bin/terminal")
        self.updater.run(f"write {update.quote(before)} /bin/terminal", write=True)
        expected = {"sha256": update.digest(after), "mode": 0o755}
        self.updater.replace("/bin/terminal", after, expected)
        self.assertEqual(self.updater.identity("/bin/terminal", self.updater.stat("/bin/terminal")), expected)
        self.assertIsNone(self.updater.stat("/bin/.scarlet-bundle-update"))
        self.assertIn("Links: 1", self.updater.stat("/bin/terminal")[2])
        self.check_filesystem()

    def test_short_and_block_backed_symlinks_can_be_replaced_and_verified(self):
        for target in ("libvulkan.so.1", "a/" * 60 + "library.so"):
            source = self.root / "symlink"
            source.unlink(missing_ok=True)
            source.symlink_to(target)
            self.updater.replace("/lib/library.so", source, {"link": target})
            self.assertEqual(self.updater.identity("/lib/library.so", self.updater.stat("/lib/library.so")), {"link": target})
            self.check_filesystem()

    def test_failed_staging_verification_keeps_the_installed_inode(self):
        before = self.source("before", b"old executable")
        after = self.source("after", b"replacement")
        self.updater.parents("/bin/terminal")
        self.updater.run(f"write {update.quote(before)} /bin/terminal", write=True)
        original = self.updater.identity
        self.updater.identity = lambda name, info: {"sha256": "corrupt", "mode": 0o755} if name.endswith(".scarlet-bundle-update") else original(name, info)
        with self.assertRaisesRegex(ValueError, "staged readback mismatch"):
            self.updater.replace("/bin/terminal", after, {"sha256": update.digest(after), "mode": 0o755})
        self.assertEqual(original("/bin/terminal", self.updater.stat("/bin/terminal"))["sha256"], update.digest(before))
        self.assertIsNotNone(self.updater.stat("/bin/.scarlet-bundle-update"))
        self.check_filesystem()

    def test_unfinished_staging_and_source_changes_do_not_remove_installed_files(self):
        source = self.source("source", b"input")
        self.updater.parents("/bin/terminal")
        self.updater.run(f"write {update.quote(source)} /bin/terminal", write=True)
        self.updater.run(f"write {update.quote(source)} /bin/.scarlet-bundle-update", write=True)
        with self.assertRaisesRegex(ValueError, "unfinished staged"):
            self.updater.replace("/bin/terminal", source, {"sha256": update.digest(source), "mode": 0o755})
        self.updater.run("rm /bin/.scarlet-bundle-update", write=True)
        with self.assertRaisesRegex(ValueError, "source changed"):
            self.updater.replace("/bin/terminal", source, {"sha256": "wrong", "mode": 0o755})
        self.assertEqual(self.updater.identity("/bin/terminal", self.updater.stat("/bin/terminal"))["sha256"], update.digest(source))
        self.check_filesystem()

    def test_selection_and_configuration_preserve_user_owned_locations(self):
        overlay = self.root / "overlay"
        names = ("bin/terminal", "etc/wireless.conf", "root/private", "home/user/file", "tmp/file", "systems/linux-aarch64/root/private", "systems/linux-aarch64/usr/bin/app", "share/.DS_Store")
        for name in names:
            path = overlay / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"example")
        self.assertEqual({name for name, _ in update.candidates(overlay)}, {"bin/terminal", "etc/wireless.conf", "systems/linux-aarch64/usr/bin/app"})
        self.assertTrue(update.configuration("etc/wireless.conf"))
        self.assertTrue(update.configuration("systems/linux-aarch64/etc/resolv.conf"))
        self.assertTrue(update.configuration("systems/linux-aarch64/var/lib/dpkg/status"))
        self.assertFalse(update.configuration("bin/terminal"))
        self.assertFalse(update.configuration("share/app-art/terminal-cover.png"))

    def test_published_defaults_advance_but_local_configuration_is_retained(self):
        before = {"sha256": "old-default", "mode": 0o644}
        after = {"sha256": "new-default", "mode": 0o644}
        custom = {"sha256": "user-edited", "mode": 0o644}
        defaults = {"etc/sws/start-desktop.sh": {"old-default"}}
        self.assertEqual(update.action("etc/sws/start-desktop.sh", before, after, defaults), "update")
        self.assertEqual(update.action("etc/sws/start-desktop.sh", custom, after, defaults), "retain")
        self.assertEqual(update.action("etc/sws/start-desktop.sh", after, after, defaults), "verify")
        self.assertEqual(update.action("etc/sws/start-desktop.sh", None, after, defaults), "update")
        self.assertEqual(update.action("etc/wireless.conf", before, after, defaults), "retain")
        self.assertEqual(update.action("systems/linux-aarch64/etc/resolv.conf", before, after, defaults), "retain")


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Exercise candidate board isolation and receipt source hashes without a target build."""
import hashlib
from pathlib import Path
import runpy
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch as mock_patch

ROOT = Path(__file__).resolve().parents[1]
BUILDER = runpy.run_path(str(ROOT / "scripts/build-patched-usb.py"))


def write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


def patch(path, relative, old="original", new="candidate"):
    write(path, f"diff --git a/{relative} b/{relative}\n"
          f"--- a/{relative}\n+++ b/{relative}\n@@ -1 +1 @@\n-{old}\n+{new}\n")
    return [{"patch": str(path), "sha256": BUILDER["digest"](path)}]


class BoardIsolationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="usb-builder-test-")
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name).resolve() / "repository"
        self.project = self.repo / "projects/console"
        self.core = self.repo / "core"
        self.cargo_home = self.repo / "cargo-home"
        self.cargo_home.mkdir(parents=True)
        write(self.project / "bsp/Cargo.toml", '[package]\nname = "fixture"\nversion = "0.1.0"\n')
        write(self.project / "bsp/Cargo.lock", "fixture lock\n")
        write(self.repo / "drivers/usb/test/Cargo.toml", '[dependencies]\nsoc = { path = "../../soc/test" }\nshared = { path = "../../../shared/test" }\n')
        write(self.repo / "drivers/usb/test/src/lib.rs", "original\n")
        write(self.repo / "drivers/soc/test/Cargo.toml", "[package]\n")
        write(self.repo / "shared/test/Cargo.toml", "[package]\n")
        write(self.repo / "projects/external/Cargo.toml", "[package]\n")
        self.config = {
            "bsp": {"kernel": {"features": {"network": True}}},
            "modules": {
                "usb": {"path": "../../drivers/usb/test", "enabled": True},
                "soc": {"path": "../../drivers/soc/test", "enabled": True},
                "external": {"path": "../external", "enabled": False},
            },
        }

    def copied_board(self):
        board = self.repo / "isolated-board"
        BUILDER["copy_board_sources"](board, self.repo)
        return board

    def test_no_patch_project_uses_original_module_paths(self):
        work = self.repo / "plain-work"
        before = BUILDER["tree_hashes"](self.repo / "drivers")
        paths = BUILDER["create_project"](
            work, self.project, self.core, self.config, self.cargo_home,
            repository=self.repo)
        config = tomllib.loads((work / "scarlet.toml").read_text())
        for name, module in self.config["modules"].items():
            original = (self.project / module["path"]).resolve()
            self.assertEqual(paths[name], original)
            self.assertEqual(config["modules"][name]["path"], str(original))
        self.assertFalse((self.repo / "isolated-board").exists())
        self.assertEqual(before, BUILDER["tree_hashes"](self.repo / "drivers"))

    def test_actual_project_generation_preserves_dependencies_and_isolated_patch(self):
        ignored = {"target", ".git", ".scarlet", ".cache", "cache", "__pycache__"}
        for name in ignored:
            write(self.repo / f"drivers/usb/test/{name}/sentinel", "do not copy\n")
        original = BUILDER["tree_hashes"](self.repo / "drivers")
        shared = BUILDER["tree_hashes"](self.repo / "shared")
        board = self.copied_board()
        policy = self.repo / "candidate.patch"
        metadata = patch(policy, "drivers/usb/test/src/lib.rs")
        changed = BUILDER["apply_board_patches"](board, [policy], metadata)
        self.assertEqual(changed, {"drivers/usb/test/src/lib.rs":
                                  hashlib.sha256(b"candidate\n").hexdigest()})
        work = self.repo / "patched-work"
        paths = BUILDER["create_project"](
            work, self.project, self.core, self.config, self.cargo_home, board, self.repo)
        generated = tomllib.loads((work / "scarlet.toml").read_text())
        self.assertEqual(paths["usb"], board / "drivers/usb/test")
        self.assertEqual(paths["soc"], board / "drivers/soc/test")
        self.assertEqual(paths["external"], self.repo / "projects/external")
        self.assertEqual(generated["modules"]["usb"]["path"], str(paths["usb"]))
        dependencies = tomllib.loads((paths["usb"] / "Cargo.toml").read_text())["dependencies"]
        self.assertEqual((paths["usb"] / dependencies["soc"]["path"]).resolve(), paths["soc"])
        self.assertEqual((paths["usb"] / dependencies["shared"]["path"]).resolve(),
                         self.repo / "shared/test")
        for name in ignored:
            self.assertFalse((paths["usb"] / name).exists())
        self.assertEqual(original, BUILDER["tree_hashes"](self.repo / "drivers"))
        self.assertEqual(shared, BUILDER["tree_hashes"](self.repo / "shared"))

    def test_shared_patch_is_rejected_without_following_source_symlink(self):
        board = self.copied_board()
        before = BUILDER["tree_hashes"](self.repo / "shared")
        policy = self.repo / "shared.patch"
        metadata = patch(policy, "shared/test/Cargo.toml", "[package]", "changed")
        with self.assertRaisesRegex(ValueError, "under copied drivers"):
            BUILDER["apply_board_patches"](board, [policy], metadata)
        self.assertEqual(before, BUILDER["tree_hashes"](self.repo / "shared"))

    def test_rename_and_copy_from_shared_are_rejected_before_git_apply(self):
        board = self.copied_board()
        before = BUILDER["tree_hashes"](self.repo / "shared")
        for operation in ("rename", "copy"):
            with self.subTest(operation=operation):
                policy = self.repo / f"{operation}.patch"
                write(policy, "diff --git a/shared/test/Cargo.toml b/drivers/usb/test/src/moved.rs\n"
                      "similarity index 100%\n"
                      f"{operation} from shared/test/Cargo.toml\n"
                      f"{operation} to drivers/usb/test/src/moved.rs\n")
                metadata = [{"patch": str(policy), "sha256": BUILDER["digest"](policy)}]
                with mock_patch.object(subprocess, "run", side_effect=AssertionError("must not apply patch")), \
                        mock_patch.object(subprocess, "check_output", side_effect=AssertionError("must not invoke Git")):
                    with self.assertRaisesRegex(ValueError, "rename/copy"):
                        BUILDER["apply_board_patches"](board, [policy], metadata)
                self.assertFalse((board / "drivers/usb/test/src/moved.rs").exists())
                self.assertEqual(before, BUILDER["tree_hashes"](self.repo / "shared"))

    def test_symlink_target_is_rejected_without_mutating_original(self):
        board = self.copied_board()
        target = board / "drivers/usb/test/src/lib.rs"
        target.unlink()
        original = self.repo / "drivers/usb/test/src/lib.rs"
        target.symlink_to(original)
        policy = self.repo / "link.patch"
        metadata = patch(policy, "drivers/usb/test/src/lib.rs")
        with self.assertRaisesRegex(ValueError, "symlink"):
            BUILDER["apply_board_patches"](board, [policy], metadata)
        self.assertEqual(original.read_text(), "original\n")

    def test_symlink_parent_is_rejected(self):
        board = self.copied_board()
        (board / "drivers/escape").symlink_to(self.repo / "drivers/usb/test", target_is_directory=True)
        policy = self.repo / "parent.patch"
        metadata = patch(policy, "drivers/escape/src/lib.rs")
        with self.assertRaisesRegex(ValueError, "symlink"):
            BUILDER["apply_board_patches"](board, [policy], metadata)
        self.assertEqual((self.repo / "drivers/usb/test/src/lib.rs").read_text(), "original\n")

    def test_missing_covered_module_is_rejected(self):
        board = self.copied_board()
        self.config["modules"]["usb"]["path"] = "../../drivers/usb/missing"
        with self.assertRaisesRegex(ValueError, "missing from isolated drivers"):
            BUILDER["module_paths"](self.project, self.config, board, self.repo)

    def test_changed_patch_hash_is_rejected_before_application(self):
        board = self.copied_board()
        policy = self.repo / "changed.patch"
        metadata = patch(policy, "drivers/usb/test/src/lib.rs")
        policy.write_text(policy.read_text() + "\n")
        with self.assertRaisesRegex(ValueError, "changed before application"):
            BUILDER["apply_board_patches"](board, [policy], metadata)
        self.assertEqual((board / "drivers/usb/test/src/lib.rs").read_text(), "original\n")

    def test_patch_cannot_create_a_source_symlink(self):
        board = self.copied_board()
        policy = self.repo / "new-symlink.patch"
        write(policy, "diff --git a/drivers/usb/test/src/link.rs b/drivers/usb/test/src/link.rs\n"
              "new file mode 120000\n--- /dev/null\n+++ b/drivers/usb/test/src/link.rs\n"
              "@@ -0,0 +1 @@\n+../../../../shared/test/Cargo.toml\n"
              "\\ No newline at end of file\n")
        metadata = [{"patch": str(policy), "sha256": BUILDER["digest"](policy)}]
        with self.assertRaisesRegex(ValueError, "created a symlink"):
            BUILDER["apply_board_patches"](board, [policy], metadata)
        self.assertEqual((self.repo / "shared/test/Cargo.toml").read_text(), "[package]\n")

    def test_new_file_under_existing_copied_directory_is_recorded(self):
        board = self.copied_board()
        policy = self.repo / "new.patch"
        write(policy, "diff --git a/drivers/usb/test/src/new.rs b/drivers/usb/test/src/new.rs\n"
              "new file mode 100644\n--- /dev/null\n+++ b/drivers/usb/test/src/new.rs\n"
              "@@ -0,0 +1 @@\n+new source\n")
        metadata = [{"patch": str(policy), "sha256": BUILDER["digest"](policy)}]
        changed = BUILDER["apply_board_patches"](board, [policy], metadata)
        self.assertEqual(changed, {"drivers/usb/test/src/new.rs":
                                  hashlib.sha256(b"new source\n").hexdigest()})
        self.assertFalse((self.repo / "drivers/usb/test/src/new.rs").exists())

    def test_core_receipt_includes_git_apply_untracked_files(self):
        core = self.repo / "git-core"
        core.mkdir()
        subprocess.run(["git", "init", "--quiet"], cwd=core, check=True)
        write(core / "existing.rs", "old\n")
        subprocess.run(["git", "add", "existing.rs"], cwd=core, check=True)
        subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                        "commit", "--quiet", "-m", "fixture"], cwd=core, check=True)
        write(core / "existing.rs", "changed\n")
        new = self.repo / "new-core.patch"
        write(new, "diff --git a/profile.rs b/profile.rs\nnew file mode 100644\n"
              "--- /dev/null\n+++ b/profile.rs\n@@ -0,0 +1 @@\n+profile source\n")
        subprocess.run(["git", "apply", str(new)], cwd=core, check=True)
        hashes = BUILDER["patched_source_hashes"](core)
        self.assertEqual(set(hashes), {"existing.rs", "profile.rs"})
        self.assertEqual(hashes["profile.rs"], hashlib.sha256(b"profile source\n").hexdigest())

    def test_real_switch_policy_changes_only_isolated_runtime(self):
        original_drivers = BUILDER["tree_hashes"](ROOT / "drivers")
        original_shared = BUILDER["tree_hashes"](ROOT / "shared")
        board = self.repo / "real-board"
        BUILDER["copy_board_sources"](board, ROOT)
        policy = ROOT / "patches/tegra210-xusb/linux-imod-policy.patch"
        metadata = [{"patch": str(policy), "sha256": BUILDER["digest"](policy)}]
        changed = BUILDER["apply_board_patches"](board, [policy], metadata)
        relative = "drivers/usb/tegra210-xusb/src/runtime.rs"
        self.assertEqual(set(changed), {relative})
        self.assertIn("context, Some(40_000)", (board / relative).read_text())
        config = tomllib.loads((ROOT / "projects/aarch64-switch-l4t-console/scarlet.toml").read_text())
        paths = BUILDER["module_paths"](ROOT / "projects/aarch64-switch-l4t-console", config, board)
        self.assertEqual(len(paths), 14)
        self.assertTrue(all(path.is_relative_to(board / "drivers") for path in paths.values()))
        self.assertEqual(original_drivers, BUILDER["tree_hashes"](ROOT / "drivers"))
        self.assertEqual(original_shared, BUILDER["tree_hashes"](ROOT / "shared"))


if __name__ == "__main__":
    unittest.main()

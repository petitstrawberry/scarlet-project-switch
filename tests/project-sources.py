#!/usr/bin/env python3
"""Check published source resolution without relying on sibling checkouts."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("project_sources", ROOT / "scripts/project_sources.py")
sources = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sources)


class SourceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "project"
        self.root.mkdir()
        self.upstream = Path(self.temp.name) / "upstream"
        subprocess.run(["git", "init", "--quiet", str(self.upstream)], check=True)
        (self.upstream / "source.txt").write_text("published source")
        (self.upstream / "Cargo.lock").write_text("# pinned lock fixture\n")
        subprocess.run(["git", "-C", str(self.upstream), "add", "."], check=True)
        subprocess.run(["git", "-C", str(self.upstream),
                        "-c", "user.name=Source test", "-c", "user.email=test@example.invalid",
                        "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture"], check=True)
        self.rev = subprocess.check_output(
            ["git", "-C", str(self.upstream), "rev-parse", "HEAD"], text=True).strip()
        (self.root / "source-pins.toml").write_text(
            f'[scarlet]\ngit = "{self.upstream.as_posix()}"\nrev = "{self.rev}"\n')
        self.override = Path(self.temp.name) / "local-source"
        self.override.mkdir()
        (self.root / "source-paths.local.toml").write_text(
            '[paths]\nscarlet = "../local-source"\n')
        self.patch_root = patch.object(sources, "ROOT", self.root)
        self.patch_root.start()
        self.addCleanup(self.patch_root.stop)

    def test_published_resolution_ignores_explicit_local_override(self):
        self.assertEqual(sources.source("scarlet"), self.override.resolve())
        public = sources.source("scarlet", published=True)
        self.assertTrue(public.is_relative_to(self.root))
        self.assertEqual((public / "source.txt").read_text(), "published source")
        self.assertNotEqual(public.resolve(), self.override.resolve())

    def test_cached_pin_is_reusable_offline_but_rejects_tracked_edits(self):
        public = sources.source("scarlet", published=True)
        self.upstream.rename(self.upstream.with_name("offline"))
        self.assertEqual(sources.source("scarlet", published=True), public)
        (public / "source.txt").write_text("unexpected edit")
        with self.assertRaisesRegex(ValueError, "cached source differs"):
            sources.source("scarlet", published=True)

    def test_existing_user_cargo_config_is_preserved(self):
        config = self.root / ".cargo/config.toml"
        config.parent.mkdir()
        config.write_text("[build]\njobs = 2\n")
        with self.assertRaisesRegex(ValueError, "non-generated"):
            sources.write_generated(config, sources.GENERATED + "[env]\n")
        self.assertEqual(config.read_text(), "[build]\njobs = 2\n")

    def test_cargo_can_update_generated_path_patch_lock_entries(self):
        public = sources.source("scarlet", published=True)
        (public / "Cargo.lock").write_text("# resolved local path patches\n")
        self.assertEqual(sources.source("scarlet", published=True), public)
        (public / "source.txt").write_text("unexpected source edit")
        with self.assertRaisesRegex(ValueError, "cached source differs"):
            sources.source("scarlet", published=True)

    def test_dependency_refresh_only_unlocks_patched_packages(self):
        (self.root / "Cargo.toml").write_text('[package]\nname = "app"\nversion = "0.1.0"\n')
        (self.root / "Cargo.lock").write_text('''[[package]]
name = "scarlet-ui"
version = "0.1.0"
source = "git+https://example.invalid/ui.git?branch=main#old"
[[package]]
name = "unrelated"
version = "0.2.0"
source = "git+https://example.invalid/other#unchanged"
[[package]]
name = "local-library"
version = "1.0.0"
''')
        config = self.root / "userspace.toml"
        config.write_text('[patch."https://example.invalid/ui"]\nscarlet-ui = { path = "new-ui" }\n')
        with patch.object(sources.subprocess, "run") as run:
            sources.refresh_pinned_dependencies(self.root, config)
        run.assert_called_once_with([
            "cargo", "--config", str(config), "update", "-p",
            "https://example.invalid/ui.git#scarlet-ui@0.1.0",
        ], cwd=self.root, check=True)

    def test_recorded_port_patch_is_applied_and_unexpected_edits_are_rejected(self):
        (self.upstream / "source.txt").write_text("published source with compatibility fix")
        port_patch = subprocess.check_output(
            ["git", "-C", str(self.upstream), "diff", "--binary", "HEAD"], text=True)
        (self.root / "compat.patch").write_text(port_patch)
        with (self.root / "source-pins.toml").open("a") as manifest:
            manifest.write('patch = "compat.patch"\n')
        public = sources.source("scarlet", published=True)
        self.assertEqual((public / "source.txt").read_text(),
                         "published source with compatibility fix")
        self.assertEqual(sources.source("scarlet", published=True), public)
        (public / "source.txt").write_text("unrecorded edit")
        with self.assertRaisesRegex(ValueError, "cached source differs"):
            sources.source("scarlet", published=True)

    def test_port_patch_additions_are_reusable_and_verified(self):
        (self.upstream / "panel.rs").write_text("pub const PANEL: bool = true;\n")
        subprocess.run(["git", "-C", str(self.upstream), "add", "--intent-to-add",
                        "panel.rs"], check=True)
        port_patch = subprocess.check_output(
            ["git", "-C", str(self.upstream), "diff", "--binary", "HEAD"], text=True)
        (self.root / "panel.patch").write_text(port_patch)
        with (self.root / "source-pins.toml").open("a") as manifest:
            manifest.write('patch = "panel.patch"\n')
        public = sources.source("scarlet", published=True)
        self.assertEqual(sources.source("scarlet", published=True), public)
        self.assertEqual((public / "panel.rs").read_text(), "pub const PANEL: bool = true;\n")
        (public / "panel.rs").write_text("unexpected source edit\n")
        with self.assertRaisesRegex(ValueError, "cached source differs"):
            sources.source("scarlet", published=True)


class PublishedManifestTests(unittest.TestCase):
    def test_cargo_and_kernel_sources_match_the_public_pins(self):
        pins = tomllib.loads((ROOT / "source-pins.toml").read_text())
        revisions = {item["git"].removesuffix(".git"): item["rev"] for item in pins.values()}
        manifests = []
        for directory in ("drivers", "userspace", "tests", "shared", "projects"):
            manifests.extend(p for p in (ROOT / directory).rglob("Cargo.toml")
                             if not {".scarlet", "target"} & set(p.relative_to(ROOT).parts))
        manifests += [ROOT / "projects/aarch64-switch-l4t-console/scarlet.toml",
                      ROOT / "tests/boot-probe/scarlet.toml"]

        def check(value, manifest):
            if isinstance(value, dict):
                if "git" in value and value["git"].removesuffix(".git") in revisions:
                    self.assertEqual(value.get("rev"), revisions[value["git"].removesuffix(".git")],
                                     str(manifest))
                if "path" in value:
                    # Inspect the tracked path, without following ignored
                    # source links that may intentionally select local work.
                    path = Path(os.path.abspath(manifest.parent / value["path"]))
                    self.assertTrue(path.is_relative_to(ROOT),
                                    f"{manifest}: external path {value['path']}")
                for child in value.values():
                    check(child, manifest)
            elif isinstance(value, list):
                for child in value:
                    check(child, manifest)

        for manifest in manifests:
            check(tomllib.loads(manifest.read_text()), manifest)

    def test_full_bundle_keeps_layers_and_applies_pinned_application_source(self):
        sys.path.insert(0, str(ROOT / "scripts"))
        spec = importlib.util.spec_from_file_location("prepare_console", ROOT / "scripts/prepare-console.py")
        console = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(console)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root / "full.toml").write_text('[[layers]]\nkind = "bundle"\npath = "desktop.toml"\n')
            (root / "desktop.toml").write_text('''[[layers]]
kind = "copy"
source = "fs"
to = "/"
[[layers]]
kind = "script"
source = "tools/dictionary.sh"
output = ".cache/dictionary"
to = "/share/dictionary"
[[layers]]
kind = "cargo"
source = { git = "https://example.invalid/application.git", rev = "old" }
subdir = "app"
bin = "application"
to = "/bin/application"
''')
            checkout = root / "pinned-app"
            layers = list(console.full_layers(root / "full.toml", {
                "https://example.invalid/application": checkout,
            }))
            self.assertEqual(len(layers), 3)
            self.assertEqual(layers[0]["source"], str(root / "fs"))
            self.assertEqual(layers[1]["source"], str(root / "tools/dictionary.sh"))
            self.assertEqual(layers[1]["output"], ".cache/dictionary")
            self.assertEqual(layers[2]["source"], str(checkout))
            self.assertEqual(layers[2]["subdir"], "app")
            encoded = "\n\n".join("[[layers]]\n" + "\n".join(
                f"{key} = {console.value(value)}" for key, value in layer.items()
            ) for layer in layers)
            self.assertEqual(tomllib.loads(encoded)["layers"], layers)


if __name__ == "__main__":
    unittest.main()

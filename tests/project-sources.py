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
        self.patch_root = patch.object(sources, "ROOT", self.root)
        self.patch_root.start()
        self.addCleanup(self.patch_root.stop)

    def test_pinned_resolution_uses_the_recorded_upstream_commit(self):
        public = sources.source("scarlet")
        self.assertTrue(public.is_relative_to(self.root))
        self.assertEqual((public / "source.txt").read_text(), "published source")

    def test_local_source_override_is_rejected(self):
        (self.root / "source-paths.local.toml").write_text(
            '[paths]\nscarlet = "../local-source"\n')
        with self.assertRaisesRegex(ValueError, "no longer supported"):
            sources.source("scarlet")
        self.assertFalse((self.root / ".cache").exists())

    def test_cached_pin_is_reusable_offline_but_rejects_tracked_edits(self):
        public = sources.source("scarlet")
        self.upstream.rename(self.upstream.with_name("offline"))
        self.assertEqual(sources.source("scarlet"), public)
        (public / "source.txt").write_text("unexpected edit")
        with self.assertRaisesRegex(ValueError, "cached source differs"):
            sources.source("scarlet")

    def test_untracked_source_additions_are_rejected(self):
        public = sources.source("scarlet")
        (public / "panel.rs").write_text("pub const PANEL: bool = true;\n")
        with self.assertRaisesRegex(ValueError, "cached source differs"):
            sources.source("scarlet")

    def test_existing_user_cargo_config_is_preserved(self):
        config = self.root / ".cargo/config.toml"
        config.parent.mkdir()
        config.write_text("[build]\njobs = 2\n")
        with self.assertRaisesRegex(ValueError, "non-generated"):
            sources.write_generated(config, sources.GENERATED + "[env]\n")
        self.assertEqual(config.read_text(), "[build]\njobs = 2\n")

    def test_target_lockfile_resolution_does_not_allow_source_changes(self):
        public = sources.source("scarlet")
        (public / "Cargo.lock").write_text("# target-specific dependency resolution\n")
        self.assertEqual(sources.source("scarlet"), public)
        (public / "source.txt").write_text("unexpected source edit")
        with self.assertRaisesRegex(ValueError, "cached source differs"):
            sources.source("scarlet")

    def test_local_patch_declarations_are_rejected_before_fetch(self):
        with (self.root / "source-pins.toml").open("a") as manifest:
            manifest.write('patch = "compat.patch"\n')
        with self.assertRaisesRegex(ValueError, "local source patches are not supported"):
            sources.source("scarlet")
        self.assertFalse((self.root / ".cache").exists())

    def test_preparation_removes_old_generated_dependency_overrides(self):
        with (self.root / "source-pins.toml").open("a") as manifest:
            manifest.write(f'[scarlet-ui]\ngit = "{self.upstream.as_posix()}"\nrev = "{self.rev}"\n')
        config = self.root / ".cargo/config.toml"
        sources.write_generated(config, sources.GENERATED +
                                '[patch."https://example.invalid/ui"]\nui = { path = "old" }\n')
        project = self.root / "project"
        with patch.object(sources, "PROJECT", project):
            checkouts = sources.prepare()
        generated = tomllib.loads(config.read_text())
        self.assertEqual(set(generated), {"env", "target"})
        self.assertEqual(generated["env"]["SCARLET_UI_SOURCE"]["value"], str(checkouts["scarlet-ui"]))
        self.assertEqual(config.read_text(), (project / ".scarlet/userspace.toml").read_text())


class PublishedManifestTests(unittest.TestCase):
    def test_no_local_patch_files_or_declarations_remain(self):
        self.assertFalse(list((ROOT / "patches").rglob("*")))
        for pin in sources.pins().values():
            self.assertEqual(set(pin), {"git", "rev"})
            self.assertTrue(pin["git"].startswith("https://github.com/"))

    def test_cargo_and_kernel_sources_match_the_public_pins(self):
        pins = tomllib.loads((ROOT / "source-pins.toml").read_text())
        revisions = {item["git"].removesuffix(".git"): item["rev"] for name, item in pins.items()
                     if name not in ("scarlet-distribution", "sgfx-core")}
        manifests = []
        for directory in ("drivers", "userspace", "tests", "shared", "projects"):
            manifests.extend(p for p in (ROOT / directory).rglob("Cargo.toml")
                             if not {".scarlet", "target"} & set(p.relative_to(ROOT).parts))
        manifests += [ROOT / "projects/aarch64-switch-l4t-console/scarlet.toml",
                      ROOT / "tests/boot-probe/scarlet.toml"]

        def check(value, manifest, key=None):
            if isinstance(value, dict):
                if "git" in value and value["git"].removesuffix(".git") in revisions:
                    expected = (pins["sgfx-core"]["rev"] if value.get("package", key) == "sgfx-core"
                                else revisions[value["git"].removesuffix(".git")])
                    self.assertEqual(value.get("rev"), expected, str(manifest))
                if "path" in value:
                    # Inspect the manifest path without following generated
                    # source links into the upstream cache.
                    path = Path(os.path.abspath(manifest.parent / value["path"]))
                    self.assertTrue(path.is_relative_to(ROOT),
                                    f"{manifest}: external path {value['path']}")
                for name, child in value.items():
                    check(child, manifest, name)
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

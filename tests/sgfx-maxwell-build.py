#!/usr/bin/env python3
"""Check native Maxwell packaging contracts without a GPU or cross compiler."""

import copy
import importlib.util
import os
from pathlib import Path
import subprocess
import struct
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import tomllib

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location("build_sgfx_maxwell", ROOT / "scripts/build-sgfx-maxwell.py")
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)

VALID_REPORT = {
    "elf_type": "DYN", "osabi": 83, "machine": "aarch64", "entry": 0,
    "interpreter": None, "needed": [], "undefined_relocated_symbols": [],
    "tls": [], "relr": False, "textrel": False, "symbol_versioning": False,
    "relocations": {"RELATIVE": 10, "ABS64": 1}, "hash_tables": ["sysv", "gnu"],
    "soname": "libsgfx_scarlet_maxwell.so",
}


class ElfFixture:
    """The pinned ELF parser interface, with independently selected exports."""

    def __init__(self, report=None, symbols=None, symbol_size=24):
        self.inventory = copy.deepcopy(VALID_REPORT if report is None else report)
        self.symbols = symbols if symbols is not None else [
            ("sgfx_backend_get_api_v2", 1, 1), ("sgfx_backend_get_driver_api_v2", 1, 1),
        ]
        self.symbol_size = symbol_size

    def report(self):
        return self.inventory

    def tag(self, tag, default=None):
        return {4: 0x200, 6: 0x400, 11: self.symbol_size}.get(tag, default)

    def at_vaddr(self, address, size):
        return address

    def unpack(self, fmt, offset):
        if fmt == "II":
            return (1, len(self.symbols) + 1)
        index = (offset - 0x400) // 24
        _, binding, section, *details = self.symbols[index - 1]
        kind, visibility = details if details else (2, 0)
        return (index, binding << 4 | kind, visibility, section, 0x1000, 4)

    def dynstring(self, index):
        return self.symbols[index - 1][0]


class AuditTests(unittest.TestCase):
    def audit(self, elf):
        with patch.object(builder, "audit_module", return_value=SimpleNamespace(Elf=lambda path: elf)):
            return builder.audit_driver(Path("driver.so"))

    def test_the_two_required_v2_entries_cannot_be_missing(self):
        self.assertEqual(self.audit(ElfFixture())["exports"], sorted(builder.EXPORTS))
        for symbols in ([], [("sgfx_backend_get_api_v2", 1, 1)],
                        [("sgfx_backend_get_driver_api_v2", 1, 1)],
                        [("sgfx_backend_get_api_v2", 1, 1),
                         ("sgfx_backend_get_driver_api_v2", 1, 1), ("unexpected", 2, 1)],
                        [("sgfx_backend_get_api_v2", 1, 0)]):
            with self.subTest(symbols=symbols), self.assertRaisesRegex(RuntimeError, "unexpected driver exports"):
                self.audit(ElfFixture(symbols=symbols))

    def test_local_symbols_do_not_extend_the_public_abi(self):
        report = self.audit(ElfFixture(symbols=[("sgfx_backend_get_api_v2", 1, 1),
                                               ("sgfx_backend_get_driver_api_v2", 1, 1),
                                               ("private_helper", 0, 1)]))
        self.assertEqual(report["exports"], sorted(builder.EXPORTS))

    def test_the_optional_ycbcr_entry_is_a_known_function_export(self):
        required = [("sgfx_backend_get_api_v2", 1, 1),
                    ("sgfx_backend_get_driver_api_v2", 1, 1)]
        optional = "sgfx_backend_get_ycbcr_api_v2"
        report = self.audit(ElfFixture(symbols=required + [(optional, 1, 1)]))
        self.assertEqual(report["exports"], sorted(builder.EXPORTS | builder.OPTIONAL_EXPORTS))
        with self.assertRaisesRegex(RuntimeError, "API entry must be a function"):
            self.audit(ElfFixture(symbols=required + [(optional, 1, 1, 1, 0)]))
        with self.assertRaisesRegex(RuntimeError, "unexpected driver exports"):
            self.audit(ElfFixture(symbols=required + [(optional, 1, 1), ("unknown_api", 1, 1)]))

    def test_unsupported_loader_requirements_fail_before_installation(self):
        cases = {
            "elf_type": "EXEC", "osabi": 0, "machine": "riscv64", "entry": 0x1000,
            "interpreter": "/lib/ld.so", "needed": ["libc.so"],
            "undefined_relocated_symbols": [{"name": "malloc"}],
            "tls": [{"memsz": 8}], "relr": True, "textrel": True,
            "symbol_versioning": True, "relocations": {"TLS_DTPMOD64": 1},
            "hash_tables": ["gnu"], "soname": "wrong.so",
        }
        for field, value in cases.items():
            with self.subTest(field=field), self.assertRaisesRegex(RuntimeError, "unsupported native linkage"):
                report = copy.deepcopy(VALID_REPORT)
                report[field] = value
                self.audit(ElfFixture(report=report))

    def test_the_upstream_loader_relocation_set_is_accepted(self):
        report = copy.deepcopy(VALID_REPORT)
        report["relocations"] = {name: 1 for name in ("NONE", "RELATIVE", "JUMP_SLOT", "GLOB_DAT", "ABS64")}
        self.audit(ElfFixture(report=report))

    def test_non_elf64_dynamic_symbols_are_rejected(self):
        for size in (16, None):
            with self.subTest(size=size), self.assertRaisesRegex(RuntimeError, "dynamic symbol table"):
                self.audit(ElfFixture(symbol_size=size))

    def test_the_api_entry_must_be_a_visible_function(self):
        with self.assertRaisesRegex(RuntimeError, "API entry must be a function"):
            self.audit(ElfFixture(symbols=[("sgfx_backend_get_api_v2", 1, 1, 1, 0)]))
        with self.assertRaisesRegex(RuntimeError, "unexpected driver exports"):
            self.audit(ElfFixture(symbols=[("sgfx_backend_get_api_v2", 1, 1, 2, 2)]))

    def test_parser_is_loaded_from_the_pinned_distribution(self):
        with tempfile.TemporaryDirectory() as temporary:
            checkout = Path(temporary)
            (checkout / "tools").mkdir()
            (checkout / "tools/elf_audit.py").write_text("PINNED_PARSER = True\n")
            with patch.object(builder, "source", return_value=checkout) as source:
                self.assertTrue(builder.audit_module().PINNED_PARSER)
            source.assert_called_once_with("scarlet-distribution")


def elf_fixture(kind=2, visibility=0, symbol_size=24):
    """A sectionless DSO with both v2 entries and real dynamic hash tables."""
    names = [builder.LIBRARY, "sgfx_backend_get_api_v2", "sgfx_backend_get_driver_api_v2"]
    strings, offsets = bytearray(b"\0"), []
    for name in names:
        offsets.append(len(strings))
        strings.extend(name.encode() + b"\0")
    data = bytearray(2048)
    data[:16] = b"\x7fELF\x02\x01\x01\x53" + bytes(8)
    struct.pack_into("<HHIQQQIHHHHHH", data, 16, 3, 183, 1, 0, 64, 0, 0, 64, 56, 2, 0, 0, 0)
    tags = [(5, 1024), (10, len(strings)), (6, 1280), (4, 1536), (0x6ffffef5, 1600), (14, offsets[0])]
    if symbol_size is not None:
        tags.append((11, symbol_size))
    tags.append((0, 0))
    struct.pack_into("<IIQQQQQQ", data, 64, 1, 5, 0, 0, 0, len(data), len(data), 4096)
    struct.pack_into("<IIQQQQQQ", data, 120, 2, 4, 512, 512, 0, len(tags) * 16, len(tags) * 16, 8)
    for index, tag in enumerate(tags):
        struct.pack_into("<qQ", data, 512 + index * 16, *tag)
    data[1024:1024 + len(strings)] = strings
    for index, name in enumerate(offsets[1:], 1):
        struct.pack_into("<IBBHQQ", data, 1280 + index * 24, name, 0x10 | kind, visibility, 1, 1800 + index * 4, 4)
    struct.pack_into("<IIIIII", data, 1536, 1, 3, 1, 0, 2, 0)
    hashes, bloom = [], 0
    for name in names[1:]:
        value = 5381
        for byte in name.encode():
            value = (value * 33 + byte) & 0xffffffff
        hashes.append(value)
        bloom |= 1 << (value % 64) | 1 << ((value >> 5) % 64)
    struct.pack_into("<IIIIQIII", data, 1600, 1, 1, 1, 5, bloom, 1, hashes[0] & ~1, hashes[1] | 1)
    return data


class PinnedParserTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Unit tests stay offline; exercise the real parser if source preparation
        # has already fetched its immutable pin, as it does before image builds.
        pin = tomllib.loads((ROOT / "source-pins.toml").read_text())["scarlet-distribution"]["rev"]
        path = ROOT / ".cache/sources/scarlet-distribution" / pin / "tools/elf_audit.py"
        if not path.is_file():
            raise unittest.SkipTest("pinned distribution parser has not been fetched")
        spec = importlib.util.spec_from_file_location("pinned_sgfx_elf_audit", path)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def audit(self, data):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / builder.LIBRARY
            path.write_bytes(data)
            with patch.object(builder, "audit_module", return_value=self.module):
                return builder.audit_driver(path)

    def test_real_sectionless_elf_exports_both_api_functions(self):
        self.assertEqual(self.audit(elf_fixture())["exports"], sorted(builder.EXPORTS))

    def test_hidden_object_or_missing_symbol_size_cannot_pass_real_parser(self):
        for arguments in ({"kind": 1}, {"visibility": 2}, {"symbol_size": None}):
            with self.subTest(arguments=arguments), self.assertRaises(RuntimeError):
                self.audit(elf_fixture(**arguments))

    def test_truncated_elf_is_rejected(self):
        with self.assertRaises(ValueError):
            self.audit(elf_fixture()[:80])


class BuildTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.project = self.root / "project with spaces"
        self.plugin = self.root / "plugin"
        self.plugin.mkdir()
        (self.plugin / builder.MANIFEST).write_text(
            "abi=2\nname=scarlet-maxwell\ngpu_backend=nvidia-gm20b\nlibrary=libsgfx_scarlet_maxwell.so\n")
        self.plugin_patch = patch.object(builder, "PLUGIN", self.plugin)
        self.plugin_patch.start()
        self.addCleanup(self.plugin_patch.stop)

    def compile_fixture(self, command, *, check, cwd, env):
        self.command, self.cwd, self.env = command, cwd, env
        self.assertTrue(check)
        artifact = Path(env["CARGO_TARGET_DIR"]) / builder.TARGET / "release" / builder.LIBRARY
        artifact.parent.mkdir(parents=True, exist_ok=True)
        artifact.write_bytes(b"audited driver fixture")

    def test_build_is_locked_release_and_audits_before_installing_exactly_two_files(self):
        install = self.root / "staging/system/lib/sgfx"
        inherited = {"CARGO_ENCODED_RUSTFLAGS": "old-flags", "CARGO_UNSTABLE_BUILD_STD": "std,panic_abort",
                     "CARGO_UNSTABLE_BUILD_STD_FEATURES": "compiler-builtins-mem", "RUSTFLAGS": "old-flags"}
        with patch.dict(os.environ, inherited), patch.object(builder.subprocess, "run", side_effect=self.compile_fixture), \
                patch.object(builder, "audit_driver", return_value=VALID_REPORT) as audit:
            output = builder.build(self.project, install)
            self.assertEqual(os.environ["CARGO_ENCODED_RUSTFLAGS"], "old-flags")
        self.assertEqual(self.command[:6], ["cargo", "rustc", "--locked", "--release", "--target", builder.TARGET])
        self.assertEqual(self.cwd, self.plugin)
        self.assertIn(str(self.plugin / "Cargo.toml"), self.command)
        for flag in ("--hash-style=both", "-z", "now", "defs", "--exclude-libs=ALL", "--entry=0", "-soname", builder.LIBRARY):
            self.assertIn(f"link-arg={flag}", self.command)
        for name in inherited.keys() - {"RUSTFLAGS"}:
            self.assertNotIn(name, self.env)
        self.assertIn("-Zdefault-visibility=hidden", self.env["RUSTFLAGS"])
        self.assertIn('getrandom_backend="custom"', self.env["RUSTFLAGS"])
        self.assertEqual(self.env["CARGO_PROFILE_RELEASE_LTO"], "thin")
        self.assertEqual(self.env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"], "1")
        self.assertEqual(output, self.project / ".scarlet/sgfx-maxwell")
        audit.assert_called_once_with(output / "build" / builder.TARGET / "release" / builder.LIBRARY)
        self.assertTrue((output / "driver-elf.json").is_file())
        self.assertEqual({path.name for path in install.iterdir()}, {builder.LIBRARY, builder.MANIFEST})
        self.assertEqual((install / builder.LIBRARY).read_bytes(), b"audited driver fixture")
        self.assertEqual((install / builder.MANIFEST).read_bytes(), (self.plugin / builder.MANIFEST).read_bytes())

    def test_failed_audit_never_creates_an_install_destination(self):
        install = self.root / "staging/system/lib/sgfx"
        with patch.object(builder.subprocess, "run", side_effect=self.compile_fixture), \
                patch.object(builder, "audit_driver", side_effect=RuntimeError("audit failed")):
            with self.assertRaisesRegex(RuntimeError, "audit failed"):
                builder.build(self.project, install)
        self.assertFalse(install.exists())
        self.assertFalse((self.project / ".scarlet/sgfx-maxwell" / builder.LIBRARY).exists())

    def test_bundle_passes_the_staging_directory_from_project_cwd(self):
        bundle = ROOT / "projects/aarch64-switch-l4t-console/bundles/sgfx-maxwell.toml"
        layer, = tomllib.loads(bundle.read_text())["layers"]
        self.assertEqual(layer["kind"], "script")
        self.assertEqual(layer["to"], "/system/lib/sgfx")
        self.assertNotIn("output", layer)
        wrapper = (bundle.parent / layer["source"]).resolve()
        fake_bin = self.root / "fake-bin"
        fake_bin.mkdir()
        record = self.root / "arguments"
        python = fake_bin / "python3"
        python.write_text('#!/bin/sh\nprintf "%s\\n" "$PWD" "$@" > "$SGFX_TEST_RECORD"\n')
        python.chmod(0o755)
        self.project.mkdir()
        install = self.root / "initramfs staging/system/lib/sgfx"
        env = os.environ | {"PATH": f"{fake_bin}:{os.environ['PATH']}", "SGFX_TEST_RECORD": str(record)}
        subprocess.run(["sh", str(wrapper), str(install)], cwd=self.project, env=env, check=True)
        self.assertEqual(record.read_text().splitlines(), [
            str(self.project), str(ROOT / "scripts/build-sgfx-maxwell.py"),
            "--project", str(self.project), "--install-dir", str(install),
        ])


if __name__ == "__main__":
    unittest.main()

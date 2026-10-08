#!/usr/bin/env python3
"""Build, audit, and optionally install the native Maxwell SGFX driver."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
PLUGIN = ROOT / "userspace/sgfx-backend-scarlet-maxwell-plugin"
TARGET = "aarch64-unknown-scarlet"
LIBRARY = "libsgfx_scarlet_maxwell.so"
MANIFEST = "scarlet-maxwell.sgfx-driver"
EXPORTS = {"sgfx_backend_get_api_v2", "sgfx_backend_get_driver_api_v2"}
OPTIONAL_EXPORTS = {"sgfx_backend_get_ycbcr_api_v2"}
RELOCATIONS = {"NONE", "RELATIVE", "JUMP_SLOT", "GLOB_DAT", "ABS64"}


def audit_module():
    """Use the vendored Scarlet ELF parser without resolving source checkouts."""
    path = ROOT / "scripts/elf_audit.py"
    spec = importlib.util.spec_from_file_location("scarlet_elf_audit", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def audit_driver(path):
    elf = audit_module().Elf(path)
    report = elf.report()
    if (report["elf_type"] != "DYN" or report["osabi"] != 83
            or report["machine"] != "aarch64" or report["entry"] != 0
            or report["interpreter"] or report["needed"]
            or report["undefined_relocated_symbols"] or report["tls"]
            or report["relr"] or report["textrel"] or report["symbol_versioning"]
            or set(report["relocations"]) - RELOCATIONS
            or set(report["hash_tables"]) != {"sysv", "gnu"}
            or report["soname"] != LIBRARY):
        raise RuntimeError(f"driver requires unsupported native linkage: {report}")
    # DT_HASH records the dynamic symbol count even when section headers have
    # been stripped. Scarlet exposes defined global/weak symbols with default
    # or protected visibility through dlsym.
    _, count = elf.unpack("II", elf.at_vaddr(elf.tag(4), 8))
    if elf.tag(11) != 24 or elf.tag(6) is None:
        raise RuntimeError("missing or unsupported dynamic symbol table")
    exports = set()
    for index in range(1, count):
        name, info, visibility, section, _, _ = elf.unpack(
            "IBBHQQ", elf.at_vaddr(elf.tag(6) + index * 24, 24))
        if section and info >> 4 in (1, 2) and visibility & 3 in (0, 3):
            symbol = elf.dynstring(name)
            if symbol in EXPORTS | OPTIONAL_EXPORTS and info & 15 != 2:
                raise RuntimeError("driver API entry must be a function")
            exports.add(symbol)
    if not EXPORTS <= exports or exports - EXPORTS - OPTIONAL_EXPORTS:
        raise RuntimeError(f"unexpected driver exports: {sorted(exports)}")
    report["exports"] = sorted(exports)
    return report


def build_environment(target_dir):
    env = os.environ.copy()
    # Encoded flags take precedence over RUSTFLAGS. Kernel build-std settings
    # must not leak into a userspace build with a prebuilt Scarlet standard lib.
    for variable in ("CARGO_ENCODED_RUSTFLAGS", "CARGO_UNSTABLE_BUILD_STD",
                     "CARGO_UNSTABLE_BUILD_STD_FEATURES"):
        env.pop(variable, None)
    env.update(CARGO_TARGET_DIR=str(target_dir),
               RUSTFLAGS='-Zdefault-visibility=hidden --cfg getrandom_backend="custom"',
               CARGO_PROFILE_RELEASE_LTO="thin", CARGO_PROFILE_RELEASE_CODEGEN_UNITS="1")
    return env


def build(project, install_dir=None):
    output = project.resolve() / ".scarlet/sgfx-maxwell"
    target_dir = output / "build"
    output.mkdir(parents=True, exist_ok=True)
    subprocess.run([
        "cargo", "rustc", "--locked", "--release", "--target", TARGET,
        "--manifest-path", str(PLUGIN / "Cargo.toml"), "--lib", "--",
        "-C", "link-arg=--hash-style=both", "-C", "link-arg=-z", "-C", "link-arg=now",
        "-C", "link-arg=-z", "-C", "link-arg=defs", "-C", "link-arg=--exclude-libs=ALL",
        "-C", "link-arg=--entry=0", "-C", "link-arg=-soname", "-C", f"link-arg={LIBRARY}",
    ], check=True, cwd=PLUGIN, env=build_environment(target_dir))
    artifact = target_dir / TARGET / "release" / LIBRARY
    report = audit_driver(artifact)
    library = output / LIBRARY
    manifest = output / MANIFEST
    shutil.copy2(artifact, library)
    shutil.copy2(PLUGIN / MANIFEST, manifest)
    (output / "driver-elf.json").write_text(json.dumps(report, indent=2) + "\n")
    # As in Scarlet's native driver bundle, prepare linkage before subsequent
    # Cargo layers. Application dependency selection stays with cargo-scarlet.
    flags = ["--cfg", 'getrandom_backend="custom"', "-Zdefault-visibility=hidden",
             "-C", "link-arg=--dynamic-linker=/bin/scarlet-ld",
             "-C", "link-arg=--as-needed", "-C", f"link-arg={library}",
             "-C", "link-arg=--unresolved-symbols=ignore-all"]
    (project.resolve() / ".scarlet/userspace.toml").write_text(
        '[target.aarch64-unknown-scarlet]\nrustflags = ' + json.dumps(flags) + '\n')
    if install_dir is not None:
        install_dir.mkdir(parents=True, exist_ok=True)
        for path in (library, manifest):
            shutil.copy2(path, install_dir / path.name)
    print(f"Native Maxwell SGFX driver: {output}")
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, default=PROJECT,
                        help="project directory that receives the audited build artifacts")
    parser.add_argument("--install-dir", type=Path,
                        help="also install the library and driver manifest into this directory")
    args = parser.parse_args()
    build(args.project, args.install_dir)


if __name__ == "__main__":
    main()

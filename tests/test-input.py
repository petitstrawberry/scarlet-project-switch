#!/usr/bin/env python3
"""Build an isolated QA kernel and exercise gamepad and touch scrolling in QEMU.

No fixture is linked into or copied to the production image/SD package. This
tests Scarlet's input path, not Tegra, the physical Joy-Con rail, or touch I2C.
"""
import argparse
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
QA = ROOT / ".cache/input-qa-project"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rootfs", type=Path, help="test a prepared SD image instead of the factory rootfs")
    parser.add_argument("--panel", action="store_true", help="exercise the installed software keyboard")
    args = parser.parse_args()
    production = tomllib.loads((PROJECT / "scarlet.toml").read_text())
    QA.mkdir(parents=True, exist_ok=True)
    shutil.copytree(PROJECT / "bsp", QA / "bsp", dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns("target", "Cargo.lock"))
    manifest = '''schema_version = 2
[project]
name = "switch-input-qa"
[bsp]
path = "bsp"
package = "scarlet"
[bsp.kernel]
'''
    kernel = production["bsp"]["kernel"]["source"]
    manifest += f'source = {{ git = "{kernel["git"]}", rev = "{kernel["rev"]}" }}\n'
    features = production["bsp"]["kernel"]["features"]
    manifest += "features = { " + ", ".join(f"{key} = {str(value).lower()}" for key, value in features.items()) + " }\n[modules]\n"
    for name, module in production["modules"].items():
        if module.get("enabled"):
            path = (PROJECT / module["path"]).resolve()
            manifest += f'"{name}" = {{ path = "{path}", enabled = true }}\n'
    manifest += f'"scarlet-input-qa-fixture" = {{ path = "{ROOT / "tests/input-fixture"}", enabled = true }}\n'
    (QA / "scarlet.toml").write_text(manifest)
    kernel_env = os.environ.copy()
    kernel_env.pop("CARGO_UNSTABLE_BUILD_STD", None)
    kernel_env.pop("CARGO_UNSTABLE_BUILD_STD_FEATURES", None)
    subprocess.run(["cargo", "scarlet", "build", "--project", str(QA), "--release"],
                   cwd=ROOT, env=kernel_env, check=True)
    elf = QA / "bsp/target/aarch64-switch-none-elf/release/scarlet"
    # Match production packaging: SDK's allocated .ksym sidecar is at address
    # zero outside PT_LOAD, and must not create a 2 GiB gap in the flat Image.
    subprocess.run(["llvm-objcopy", "--remove-section=.ksym", "-O", "binary",
                    str(elf), str(QA / "Image")], check=True)
    package_spec = importlib.util.spec_from_file_location("package", PROJECT / "tools/package_l4t.py")
    package = importlib.util.module_from_spec(package_spec)
    package_spec.loader.exec_module(package)
    package.validate_image((QA / "Image").read_bytes(), elf.read_bytes())
    user_env = os.environ.copy()
    user_env.update(CARGO_HOME=str(PROJECT / ".scarlet/cache/cargo-home"),
                    CARGO_TARGET_DIR=str(ROOT / ".cache/input-qa-target"),
                    CARGO_UNSTABLE_BUILD_STD="std,panic_abort",
                    CARGO_UNSTABLE_BUILD_STD_FEATURES="compiler-builtins-mem",
                    CARGO_UNSTABLE_UNSTABLE_OPTIONS="true")
    subprocess.run(["cargo", "build", "--manifest-path", "tests/input-panel-qa/Cargo.toml" if args.panel else "tests/input-qa/Cargo.toml",
                    "--target", "aarch64-unknown-scarlet", "--release"],
                   cwd=ROOT, env=user_env, check=True)
    spec = importlib.util.spec_from_file_location("console", ROOT / "tests/qemu-console.py")
    console = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(console)
    console.run(input_qa=not args.panel, input_panel_qa=args.panel, timeout=240, rootfs=args.rootfs)


if __name__ == "__main__":
    main()

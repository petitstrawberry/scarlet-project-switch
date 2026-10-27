#!/usr/bin/env python3
"""Validate physical-driver protocol cores and their local Scarlet PCI adapter.

Run inside nix develop. This does not emulate Tegra or claim a physical radio
connection. No production pins are changed; the temporary build uses the given
local Scarlet checkout and is removed after its logs are preserved.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(scarlet, output, target_dir=None):
    output.mkdir(parents=True, exist_ok=True)
    (output / "result.json").unlink(missing_ok=True)
    env = os.environ.copy()
    env["CARGO_HOME"] = str(ROOT / "projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home")
    env.pop("CARGO_TARGET_DIR", None)
    if target_dir:
        env["CARGO_TARGET_DIR"] = str(target_dir)
    with tempfile.TemporaryDirectory(prefix="wireless-cores-", dir=ROOT / ".cache") as temporary:
        work = Path(temporary)
        suites = {
            "tegra-pcie": ROOT / "drivers/pcie/tegra210/src/lib.rs",
            "brcmfmac": ROOT / "drivers/network/brcmfmac/src/lib.rs",
            "watchdog": ROOT / "tests/wireless-probe/src/recovery.rs",
        }
        for name, source in suites.items():
            with (output / (name + ".log")).open("w") as log:
                subprocess.run(["rustc", "--edition=2024", "--test", str(source), "-o", str(work / name)],
                               stdout=log, stderr=subprocess.STDOUT, check=True)
                subprocess.run([str(work / name)], stdout=log, stderr=subprocess.STDOUT, check=True)
        (work / "src").mkdir()
        (work / "src/lib.rs").write_text("#![no_std]\npub use pcie::native::Host;\npub use brcmfmac::force_link;\n")
        (work / "Cargo.toml").write_text(f'''[package]
name = "wireless-native-adapter-qa"
version = "0.1.0"
edition = "2024"
[dependencies]
pcie = {{ package = "scarlet-driver-tegra210-pcie", path = {json.dumps(str(ROOT / 'drivers/pcie/tegra210'))} }}
brcmfmac = {{ package = "scarlet-driver-brcmfmac", path = {json.dumps(str(ROOT / "drivers/network/brcmfmac"))} }}
[patch."https://github.com/petitstrawberry/Scarlet"]
scarlet = {{ path = {json.dumps(str(scarlet / 'kernel'))} }}
''')
        target = ROOT / "projects/aarch64-switch-l4t-console/bsp/targets/aarch64-switch-none-elf.json"
        with (output / "native-adapter.log").open("w") as log:
            subprocess.run(["cargo", "check", "--manifest-path", str(work / "Cargo.toml"),
                            "--target", str(target), "-Z", "unstable-options", "-Z", "build-std=core,compiler_builtins,alloc",
                            "-Z", "build-std-features=compiler-builtins-mem"],
                           env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    files = sorted((ROOT / "drivers/pcie/tegra210/src").glob("*.rs"))
    files += sorted((ROOT / "drivers/network/brcmfmac/src").glob("*.rs"))
    files += [ROOT / "tests/wireless-probe/src/recovery.rs", scarlet / "kernel/src/device/pci/config.rs"]
    (output / "result.json").write_text(json.dumps({
        "pass": True, "hardware_test": False, "native_adapter_compiled": True,
        "scarlet_checkout": str(scarlet),
        "sha256": {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in files},
    }, indent=2) + "\n")
    print("WIRELESS_CORES_PASS tegra-config/controller brcm-chip/pcie watchdog native-adapter")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scarlet", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=ROOT / ".cache/wireless-cores-qa")
    parser.add_argument("--target-dir", type=Path, help="Reuse a build directory during driver iteration")
    args = parser.parse_args()
    run(args.scarlet.resolve(), args.output.resolve(), args.target_dir.resolve() if args.target_dir else None)

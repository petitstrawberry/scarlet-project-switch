#!/usr/bin/env python3
"""Package the existing console image behind Switchvisor's USB loader."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
CONSOLE = PROJECT / ".scarlet/l4t"
OUTPUT = PROJECT / ".scarlet/switchvisor"
BOOT_DIRECTORY = "switchroot/scarlet-switchvisor"
SOURCE_DIRECTORY = "switchroot/scarlet-console"
ENTRY_FILE = "bootloader/ini/L4T-scarlet-switchvisor.ini"

sys.path.insert(0, str(PROJECT / "tools"))
from package_l4t import legacy_image  # noqa: E402


def digest(path):
    sha = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            sha.update(block)
    return sha.hexdigest()


def verified(path, expected):
    if not path.is_file() or digest(path) != expected:
        raise ValueError(f"missing or mismatched input: {path}")
    return path


def build_distribution(destination, usb_net):
    """Combine the Nix-built monitor with the locally imported bootstack."""
    data = os.environ.get("SWITCHVISOR_DATA")
    if not data:
        raise ValueError("enter nix develop, or provide --switchvisor-dist for a custom build")
    data = Path(data)
    bootstack = PROJECT / ".scarlet/bootstack"
    command = ["switchvisor-tool", "pack-payload", str(data / "bootstrap.raw"),
               str(bootstack), str(bootstack / "bl33.bin"), "0x68200",
               str(destination / "bl33.bin"), "--usb-uart", "--usb-control", "--no-fallback"]
    if usb_net:
        command.append("--usb-net")
    result = subprocess.run(command, check=True, stdout=subprocess.PIPE, text=True)
    manifest = json.loads(result.stdout)
    manifest["source"] = json.loads((data / "source.json").read_text())
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for name in ("usb-uart.dtbo", "usb-net.dtbo") if usb_net else ("usb-uart.dtbo",):
        shutil.copyfile(data / name, destination / name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    profile = parser.add_mutually_exclusive_group()
    profile.add_argument("--switchvisor-dist", type=Path,
                         help="use an existing custom Switchvisor distribution")
    profile.add_argument("--usb-net", action="store_true",
                         help="enable the USB network bridge in the Nix-built monitor")
    args = parser.parse_args()
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="switchvisor-build-", dir=OUTPUT.parent) as temporary:
        if args.switchvisor_dist:
            dist = args.switchvisor_dist.resolve()
        else:
            dist = Path(temporary)
            build_distribution(dist, args.usb_net)
        package_distribution(dist)


def package_distribution(dist):
    console = json.loads((CONSOLE / "manifest.json").read_text())
    pins = json.loads((PROJECT / "bootstack.json").read_text())["files"]
    switchvisor = json.loads((dist / "manifest.json").read_text())
    if (not switchvisor.get("stage2_enabled")
            or not switchvisor.get("usb_uart", {}).get("enabled")
            or not switchvisor.get("usb_control", {}).get("enabled")
            or switchvisor.get("usb_control", {}).get("packaged_payload_fallback")
            or switchvisor.get("usb_gdb", {}).get("enabled")):
        raise ValueError("expected a no-fallback Switchvisor build with USB UART/control and no GDB")
    if switchvisor["payload"]["sha256"] != pins["bl33.bin"]:
        raise ValueError("Switchvisor payload is not the pinned Noble U-Boot")
    for name, expected in pins.items():
        if switchvisor["bootstack"]["files"][name]["sha256"] != expected:
            raise ValueError(f"Switchvisor bootstack has a different {name}")
    verified(dist / "bl33.bin", switchvisor["sha256"])
    verified(PROJECT / ".scarlet/bootstack/bl33.bin", pins["bl33.bin"])
    sources = {}
    for name in ("bl31.bin", "nx-plat.dtimg", "uImage", "initramfs"):
        relative = f"{SOURCE_DIRECTORY}/{name}"
        sources[name] = verified(CONSOLE / relative, console["sha256"][relative])
    usb_net = switchvisor.get("usb_net") or {"enabled": False}
    overlays = [dist / "usb-uart.dtbo"]
    if usb_net.get("enabled"):
        overlays.append(dist / "usb-net.dtbo")
    for overlay in overlays:
        if not overlay.is_file():
            raise ValueError(f"missing Switchvisor overlay: {overlay}")

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="switchvisor-package-", dir=OUTPUT.parent) as temporary:
        staging = Path(temporary)
        boot = staging / BOOT_DIRECTORY
        boot.mkdir(parents=True)
        for name in ("bl31.bin", "nx-plat.dtimg"):
            shutil.copyfile(sources[name], boot / name)
        shutil.copyfile(dist / "bl33.bin", boot / "bl33.bin")
        for overlay in overlays:
            shutil.copyfile(overlay, boot / overlay.name)
        # The USB bundle supplies these images at their usual U-Boot addresses.
        script = (b"setenv scarlet_switchvisor_payload 1\n"
                  + f"setenv scarlet_switchvisor_net {int(bool(usb_net.get('enabled')))}\n".encode()
                  + (PROJECT / "bootloader/boot.cmd").read_bytes())
        (boot / "boot.scr").write_bytes(legacy_image(
            struct.pack(">II", len(script), 0) + script,
            6, "Scarlet USB boot", arch=2,
        ))
        entry = staging / ENTRY_FILE
        entry.parent.mkdir(parents=True)
        shutil.copyfile(PROJECT / "bootloader/L4T-scarlet-switchvisor.ini", entry)
        hashes = {str(path.relative_to(staging)): digest(path)
                  for path in sorted(staging.rglob("*")) if path.is_file()}
        bundle = {
            "version": 1,
            "entry": "0xaa000000",
            "images": [
                {"path": "../bootstack/bl33.bin", "address": "0xaa000000",
                 "runtime_size": "0x68200"},
                {"path": f"../l4t/{SOURCE_DIRECTORY}/uImage", "address": "0xa0000000"},
                {"path": f"../l4t/{SOURCE_DIRECTORY}/initramfs", "address": "0x92000000"},
            ],
        }
        (staging / "bundle.json").write_text(json.dumps(bundle, indent=2) + "\n")
        source = switchvisor.get("source") or {}
        manifest = {
            "hardware_validated": False,
            "entry_file": ENTRY_FILE,
            "boot_directory": BOOT_DIRECTORY,
            "switchvisor_checkout_revision": source.get("rev"),
            "switchvisor_source": source,
            "switchvisor_bl33_sha256": switchvisor["sha256"],
            "usb_net": usb_net,
            "console_kernel_elf_sha256": console["kernel_elf_sha256"],
            "bundle_input_sha256": {
                "bl33.bin": pins["bl33.bin"],
                "uImage": digest(sources["uImage"]),
                "initramfs": digest(sources["initramfs"]),
            },
            "sha256": hashes,
        }
        (staging / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        if OUTPUT.exists():
            shutil.rmtree(OUTPUT)
        shutil.copytree(staging, OUTPUT)
    print(f"Switchvisor SD package: {OUTPUT}")
    print(f"USB bundle: {OUTPUT / 'bundle.json'}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error

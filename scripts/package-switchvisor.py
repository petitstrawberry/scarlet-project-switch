#!/usr/bin/env python3
"""Package Switchvisor's SD boot and indefinitely waiting USB development entries."""
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
DEV_BOOT_DIRECTORY = "switchroot/scarlet-switchvisor-dev"
SOURCE_DIRECTORY = "switchroot/scarlet-console"
ENTRY_FILE = "bootloader/ini/L4T-scarlet-switchvisor.ini"
DEV_ENTRY_FILE = "bootloader/ini/L4T-scarlet-switchvisor-dev.ini"

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


def build_distribution(destination, usb_net, *, development=False):
    """Combine the Nix-built monitor with the locally imported bootstack."""
    data = os.environ.get("SWITCHVISOR_DATA")
    if not data:
        raise ValueError("enter nix develop, or provide --switchvisor-dist for a custom build")
    data = Path(data)
    destination.mkdir(parents=True, exist_ok=True)
    bootstack = PROJECT / "firmware/bootstack"
    command = ["switchvisor-tool", "pack-payload", str(data / "bootstrap.raw"),
               str(bootstack), str(bootstack / "bl33.bin"), "0x68200",
               str(destination / "bl33.bin"), "--usb-uart", "--usb-control"]
    if development:
        command.append("--no-fallback")
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
                         help="custom fallback-enabled distribution; requires --switchvisor-dev-dist")
    parser.add_argument("--switchvisor-dev-dist", type=Path,
                        help="custom no-fallback distribution paired with --switchvisor-dist")
    profile.add_argument("--usb-net", action="store_true",
                         help="enable the USB network bridge in the Nix-built monitor")
    args = parser.parse_args()
    if bool(args.switchvisor_dist) != bool(args.switchvisor_dev_dist):
        parser.error("provide both --switchvisor-dist and --switchvisor-dev-dist for custom builds")
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="switchvisor-build-", dir=OUTPUT.parent) as temporary:
        if args.switchvisor_dist:
            dist = args.switchvisor_dist.resolve()
            dev_dist = args.switchvisor_dev_dist.resolve()
        else:
            dist = Path(temporary) / "sd"
            dev_dist = Path(temporary) / "dev"
            build_distribution(dist, args.usb_net)
            build_distribution(dev_dist, args.usb_net, development=True)
        package_distribution(dist, dev_dist)


def validate_distribution(dist, pins, *, development):
    switchvisor = json.loads((dist / "manifest.json").read_text())
    if (not switchvisor.get("stage2_enabled")
            or not switchvisor.get("usb_uart", {}).get("enabled")
            or not switchvisor.get("usb_control", {}).get("enabled")
            or switchvisor.get("usb_control", {}).get("packaged_payload_fallback") is not (not development)
            or switchvisor.get("usb_gdb", {}).get("enabled")):
        mode = "no-fallback" if development else "fallback-enabled"
        raise ValueError(f"expected a {mode} Switchvisor build with USB UART/control and no GDB: {dist}")
    if switchvisor["payload"]["sha256"] != pins["bl33.bin"]:
        raise ValueError("Switchvisor payload is not the pinned Noble U-Boot")
    for name, expected in pins.items():
        if switchvisor["bootstack"]["files"][name]["sha256"] != expected:
            raise ValueError(f"Switchvisor bootstack has a different {name}")
    verified(dist / "bl33.bin", switchvisor["sha256"])
    return switchvisor


def package_distribution(dist, dev_dist):
    console = json.loads((CONSOLE / "manifest.json").read_text())
    initramfs_dev = console.get("initramfs_profile") == "initramfs-dev"
    pins = json.loads((PROJECT / "bootstack.json").read_text())["files"]
    switchvisor = validate_distribution(dist, pins, development=False)
    development = validate_distribution(dev_dist, pins, development=True)
    if bool((switchvisor.get("usb_net") or {}).get("enabled")) != bool((development.get("usb_net") or {}).get("enabled")):
        raise ValueError("SD and development distributions must use the same USB network profile")
    verified(PROJECT / "firmware/bootstack/bl33.bin", pins["bl33.bin"])
    sources = {}
    for name in ("bl31.bin", "nx-plat.dtimg", "uImage", "initramfs"):
        relative = f"{SOURCE_DIRECTORY}/{name}"
        sources[name] = verified(CONSOLE / relative, console["sha256"][relative])
    usb_net = switchvisor.get("usb_net") or {"enabled": False}
    overlay_names = ["usb-uart.dtbo"]
    if usb_net.get("enabled"):
        overlay_names.append("usb-net.dtbo")
    for directory in (dist, dev_dist):
        for name in overlay_names:
            if not (directory / name).is_file():
                raise ValueError(f"missing Switchvisor overlay: {directory / name}")

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="switchvisor-package-", dir=OUTPUT.parent) as temporary:
        staging = Path(temporary)
        for directory, boot_directory, entry_file, uploaded in (
            (dist, BOOT_DIRECTORY, ENTRY_FILE, False),
            (dev_dist, DEV_BOOT_DIRECTORY, DEV_ENTRY_FILE, True),
        ):
            boot = staging / boot_directory
            boot.mkdir(parents=True)
            for name in ("bl31.bin", "nx-plat.dtimg"):
                shutil.copyfile(sources[name], boot / name)
            shutil.copyfile(directory / "bl33.bin", boot / "bl33.bin")
            for name in overlay_names:
                shutil.copyfile(directory / name, boot / name)
            # Device ownership is independent of where the kernel came from.
            # The normal entry reuses the installed console images. Development
            # receives them in the USB bundle and never falls back to SD.
            script = (b"setenv scarlet_switchvisor 1\n"
                      + f"setenv scarlet_switchvisor_payload {int(uploaded)}\n".encode()
                      + f"setenv scarlet_kernel_dir /{SOURCE_DIRECTORY}\n".encode()
                      + f"setenv scarlet_switchvisor_net {int(bool(usb_net.get('enabled')))}\n".encode()
                      + f"setenv scarlet_initramfs_dev {int(initramfs_dev)}\n".encode()
                      + (PROJECT / "bootloader/boot.cmd").read_bytes())
            (boot / "boot.scr").write_bytes(legacy_image(
                struct.pack(">II", len(script), 0) + script,
                6, "Scarlet USB boot" if uploaded else "Scarlet SD boot", arch=2,
            ))
            entry = staging / entry_file
            entry.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(PROJECT / "bootloader" / Path(entry_file).name, entry)
        hashes = {str(path.relative_to(staging)): digest(path)
                  for path in sorted(staging.rglob("*")) if path.is_file()}
        bundle = {
            "version": 1,
            "entry": "0xaa000000",
            "images": [
                {"path": "../../firmware/bootstack/bl33.bin", "address": "0xaa000000",
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
            "development_entry_file": DEV_ENTRY_FILE,
            "development_boot_directory": DEV_BOOT_DIRECTORY,
            "switchvisor_checkout_revision": source.get("rev"),
            "switchvisor_source": source,
            "switchvisor_bl33_sha256": switchvisor["sha256"],
            "development_switchvisor_bl33_sha256": development["sha256"],
            "development_switchvisor_source": development.get("source") or {},
            "usb_net": usb_net,
            "console_kernel_elf_sha256": console["kernel_elf_sha256"],
            "initramfs_profile": console.get("initramfs_profile", "sd-root"),
            "root_mode": "initramfs" if initramfs_dev else "sd",
            "bundle_input_sha256": {
                "bl33.bin": pins["bl33.bin"],
                "uImage": digest(sources["uImage"]),
                "initramfs": digest(sources["initramfs"]),
            },
            "sd_boot_dependencies": {
                f"{SOURCE_DIRECTORY}/{name}": digest(sources[name])
                for name in ("uImage", "initramfs")
            },
            "sha256": hashes,
        }
        (staging / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        if OUTPUT.exists():
            shutil.rmtree(OUTPUT)
        shutil.copytree(staging, OUTPUT)
    print(f"Switchvisor SD + development package: {OUTPUT}")
    print(f"USB bundle (use development entry SCR-SWD): {OUTPUT / 'bundle.json'}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error

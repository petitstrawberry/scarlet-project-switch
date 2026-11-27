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
import zlib

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t"
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


def build_distribution(destination, usb_net, *, development=False, boot_mode="hypervisor"):
    """Combine the Nix-built monitor with the locally imported bootstack."""
    data = os.environ.get("SWITCHVISOR_DATA")
    if not data:
        raise ValueError("enter nix develop, or provide --switchvisor-dist for a custom build")
    data = Path(data)
    destination.mkdir(parents=True, exist_ok=True)
    bootstack = PROJECT / "firmware/bootstack"
    command = ["switchvisor-tool", "pack-payload", str(data / "bootstrap.raw"),
               str(bootstack), str(bootstack / "bl33.bin"), "0x68200",
               str(destination / "bl33.bin"), "--usb-control"]
    if boot_mode == "loader":
        with (data / "bootstrap.raw").open("rb") as stream:
            stream.seek(40)
            if stream.read(8) != b"SVBOOT06":
                raise ValueError("loader mode requires a Switchvisor SVBOOT06 build")
        command.extend(["--boot-mode", "loader"])
    else:
        command.append("--usb-uart")
    if development:
        command.append("--no-fallback")
    if usb_net:
        command.append("--usb-net")
    result = subprocess.run(command, check=True, stdout=subprocess.PIPE, text=True)
    manifest = json.loads(result.stdout)
    manifest["source"] = json.loads((data / "source.json").read_text())
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    overlays = []
    if boot_mode == "hypervisor":
        overlays.append("usb-uart.dtbo")
    elif (data / "usb-control.dtbo").is_file():
        overlays.append("usb-control.dtbo")
    if usb_net:
        overlays.append("usb-net.dtbo")
    for name in overlays:
        shutil.copyfile(data / name, destination / name)
    if (data / "switchvisor-control.dtbo").is_file():
        shutil.copyfile(data / "switchvisor-control.dtbo", destination / "switchvisor-control.dtbo")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    profile = parser.add_mutually_exclusive_group()
    profile.add_argument("--switchvisor-dist", type=Path,
                         help="custom fallback-enabled distribution; requires --switchvisor-dev-dist")
    parser.add_argument("--switchvisor-dev-dist", type=Path,
                        help="custom no-fallback distribution paired with --switchvisor-dist")
    profile.add_argument("--usb-net", action="store_true",
                         help="enable the USB network bridge in the Nix-built monitor")
    parser.add_argument("--boot-mode", choices=["hypervisor", "loader"],
                        help="packaged default for a locally built BL33 (default: hypervisor)")
    args = parser.parse_args()
    if args.boot_mode and args.switchvisor_dist:
        parser.error("custom distributions carry their own boot mode")
    if args.boot_mode == "loader" and args.usb_net:
        parser.error("--usb-net requires a hypervisor default")
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
            build_distribution(dist, args.usb_net, boot_mode=args.boot_mode or "hypervisor")
            build_distribution(dev_dist, args.usb_net, development=True, boot_mode=args.boot_mode or "hypervisor")
        package_distribution(dist, dev_dist)


def verified_boot_mode(image, manifest):
    """Read the mode-selecting bootstrap ABI and its checksummed payload descriptor."""
    mode = manifest.get("boot_mode", "hypervisor")
    if mode not in ("hypervisor", "loader"):
        raise ValueError(f"unknown Switchvisor boot mode: {mode}")
    with image.open("rb") as stream:
        stream.seek(40)
        version = stream.read(8)
        if version != b"SVBOOT06":
            if mode == "loader":
                raise ValueError("loader default requires a loader-capable SVBOOT06 BL33")
            return "legacy"
        stream.seek(4096)
        descriptor = bytearray(stream.read(256))
        if len(descriptor) != 256 or descriptor[:8] != b"SVRAW001":
            raise ValueError("missing Switchvisor payload descriptor")
        package_size, bootstrap_size, offset, size = struct.unpack_from("<4Q", descriptor, 16)
        if not (4352 <= bootstrap_size <= offset and size > 0
                and offset + size == package_size == image.stat().st_size):
            raise ValueError("invalid Switchvisor payload extent")
        stream.seek(offset)
        if hashlib.file_digest(stream, "sha256").hexdigest() != manifest["payload"]["sha256"]:
            raise ValueError("embedded U-Boot differs from the Switchvisor manifest")
    version, flags = struct.unpack_from("<II", descriptor, 8)
    checksum, = struct.unpack_from("<I", descriptor, 132)
    struct.pack_into("<I", descriptor, 132, 0)
    if version != 1 or flags & ~63 or zlib.crc32(descriptor) != checksum:
        raise ValueError("invalid Switchvisor payload descriptor")
    default = "loader" if flags & 32 else "hypervisor"
    if manifest.get("boot_mode") != default:
        raise ValueError("Switchvisor manifest and embedded boot mode disagree")
    usb_uart, usb_control, require_upload, usb_net = (bool(flags & (1 << bit)) for bit in range(1, 5))
    if (usb_uart != manifest["usb_uart"]["enabled"]
            or usb_net != bool((manifest.get("usb_net") or {}).get("enabled"))
            or (usb_control or usb_uart or usb_net) != manifest["usb_control"]["enabled"]
            or require_upload == manifest["usb_control"]["packaged_payload_fallback"]):
        raise ValueError("Switchvisor manifest and embedded USB settings disagree")
    return default


def validate_distribution(dist, pins, *, development):
    switchvisor = json.loads((dist / "manifest.json").read_text())
    image = verified(dist / "bl33.bin", switchvisor["sha256"])
    default = verified_boot_mode(image, switchvisor)
    loader = default == "loader"
    if (switchvisor.get("stage2_enabled") is not (not loader)
            or switchvisor.get("usb_uart", {}).get("enabled") is not (not loader)
            or (loader and (switchvisor.get("usb_net") or {}).get("enabled"))
            or not switchvisor.get("usb_control", {}).get("enabled")
            or switchvisor.get("usb_control", {}).get("packaged_payload_fallback") is not (not development)
            or switchvisor.get("usb_gdb", {}).get("enabled")):
        mode = "no-fallback" if development else "fallback-enabled"
        raise ValueError(f"expected a {mode} Switchvisor {default} build with matching USB devices: {dist}")
    if switchvisor["payload"]["sha256"] != pins["bl33.bin"]:
        raise ValueError("Switchvisor payload is not the pinned Noble U-Boot")
    for name, expected in pins.items():
        if switchvisor["bootstack"]["files"][name]["sha256"] != expected:
            raise ValueError(f"Switchvisor bootstack has a different {name}")
    return switchvisor, default


def package_distribution(dist, dev_dist):
    console = json.loads((CONSOLE / "manifest.json").read_text())
    initramfs_dev = console.get("initramfs_profile") == "initramfs-dev"
    pins = json.loads((PROJECT / "bootstack.json").read_text())["files"]
    switchvisor, sd_mode = validate_distribution(dist, pins, development=False)
    development, dev_mode = validate_distribution(dev_dist, pins, development=True)
    if (sd_mode != "loader" and dev_mode != "loader"
            and bool((switchvisor.get("usb_net") or {}).get("enabled"))
            != bool((development.get("usb_net") or {}).get("enabled"))):
        raise ValueError("SD and development distributions must use the same USB network profile")
    verified(PROJECT / "firmware/bootstack/bl33.bin", pins["bl33.bin"])
    sources = {}
    for name in ("bl31.bin", "nx-plat.dtimg", "uImage", "initramfs"):
        relative = f"{SOURCE_DIRECTORY}/{name}"
        sources[name] = verified(CONSOLE / relative, console["sha256"][relative])
    usb_net = switchvisor.get("usb_net") or {"enabled": False}
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="switchvisor-package-", dir=OUTPUT.parent) as temporary:
        staging = Path(temporary)
        for directory, boot_directory, entry_file, uploaded, config, default in (
            (dist, BOOT_DIRECTORY, ENTRY_FILE, False, switchvisor, sd_mode),
            (dev_dist, DEV_BOOT_DIRECTORY, DEV_ENTRY_FILE, True, development, dev_mode),
        ):
            boot = staging / boot_directory
            boot.mkdir(parents=True)
            for name in ("bl31.bin", "nx-plat.dtimg"):
                shutil.copyfile(sources[name], boot / name)
            shutil.copyfile(directory / "bl33.bin", boot / "bl33.bin")
            net = bool((config.get("usb_net") or {}).get("enabled"))
            overlay = "usb-uart.dtbo"
            if default == "loader":
                overlay = "usb-control.dtbo" if (directory / "usb-control.dtbo").is_file() else "control-only"
            overlay_names = [overlay] if overlay != "control-only" else []
            if net:
                overlay_names.append("usb-net.dtbo")
            for name in overlay_names:
                if not (directory / name).is_file():
                    raise ValueError(f"missing Switchvisor overlay: {directory / name}")
                shutil.copyfile(directory / name, boot / name)
            guest_control = directory / "switchvisor-control.dtbo"
            if guest_control.is_file():
                shutil.copyfile(guest_control, boot / guest_control.name)
            # Device ownership is independent of where the kernel came from.
            # The normal entry reuses the installed console images. Development
            # receives them in the USB bundle and never falls back to SD.
            script = (b"setenv scarlet_switchvisor 1\n"
                      + f"setenv scarlet_switchvisor_default {default}\n".encode()
                      + f"setenv scarlet_switchvisor_overlay {overlay}\n".encode()
                      + f"setenv scarlet_switchvisor_control {int(guest_control.is_file())}\n".encode()
                      + f"setenv scarlet_switchvisor_payload {int(uploaded)}\n".encode()
                      + f"setenv scarlet_kernel_dir /{SOURCE_DIRECTORY}\n".encode()
                      + f"setenv scarlet_switchvisor_net {int(net)}\n".encode()
                      + f"setenv scarlet_initramfs_dev {int(initramfs_dev)}\n".encode()
                      + (PROJECT / "bootloader/boot.cmd").read_bytes())
            (boot / "boot.scr").write_bytes(legacy_image(
                struct.pack(">II", len(script), 0) + script,
                6, "Scarlet USB boot" if uploaded else "Scarlet SD boot", arch=2,
            ))
            entry = staging / entry_file
            entry.parent.mkdir(parents=True, exist_ok=True)
            template = (PROJECT / "bootloader" / Path(entry_file).name).read_text()
            if uploaded and default == "hypervisor":
                # Hekate accepts multiple entries in one ini. A second choice
                # shares the USB-upload directory and the same verified BL33.
                loader = template.replace("[switchvisor-dev]", "[switchvisor-dev-loader]").replace(
                    "id=SCR-SWD", "id=SCR-LDR\nswitchvisor_mode=loader"
                )
                template = template.rstrip() + "\n\n" + loader
            entry.write_text(template)
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
            "switchvisor_boot_default": sd_mode,
            "development_switchvisor_boot_default": dev_mode,
            "usb_net": usb_net,
            "development_usb_net": development.get("usb_net") or {"enabled": False},
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

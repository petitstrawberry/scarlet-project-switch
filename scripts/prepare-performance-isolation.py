#!/usr/bin/env python3
"""Prepare RAM-uploaded Switchvisor isolation bundles without accessing a device.

The input bundle supplies an already built kernel, initramfs and Noble BL33.
Six variants share those inputs: SD root, RAM root with SD, RAM root without
SD, each with normal services or init.exec=/bin/sh. Nothing is deployed here.
"""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import struct
import zlib


ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-console"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def replace_once(text, old, new):
    require(text.count(old) == 1, f"expected exactly one occurrence of {old!r}")
    return text.replace(old, new)


def check_legacy_image(path, image_type):
    data = path.read_bytes()
    require(len(data) >= 64, f"truncated legacy image: {path}")
    header = bytearray(data[:64])
    magic, checksum, _, size, _, _, data_checksum = struct.unpack(">7I", header[:28])
    require(magic == 0x27051956 and header[30] == image_type, f"invalid image header: {path}")
    header[4:8] = bytes(4)
    require(zlib.crc32(header) == checksum, f"header CRC mismatch: {path}")
    require(size == len(data) - 64, f"legacy image size mismatch: {path}")
    require(zlib.crc32(data[64:]) == data_checksum, f"data CRC mismatch: {path}")


def prepare(source, output, dtimg, overlay):
    require(not output.exists(), "output already exists; choose a fresh directory to preserve evidence")
    manifest = json.loads((source / "bundle.json").read_text())
    require(manifest["version"] == 1 and int(manifest["entry"], 0) == 0xAA000000,
            "source bundle does not use the inspected Noble BL33 entry")
    bl33_entries = [entry for entry in manifest["images"] if entry["path"] == "bl33.bin"]
    require(len(bl33_entries) == 1, "source bundle must identify bl33.bin exactly once")
    bl33_entry = bl33_entries[0]
    require(int(bl33_entry["address"], 0) == 0xAA000000 and "runtime_size" in bl33_entry,
            "source BL33 needs its inspected runtime size and address")
    for name in ("bl33.bin", "uImage", "initramfs"):
        require((source / name).is_file(), f"missing input: {source / name}")
    require(dtimg.is_file() and overlay.is_file(), "platform DT image or UART overlay is missing")
    check_legacy_image(source / "uImage", 2)
    check_legacy_image(source / "initramfs", 3)

    original = (source / "bl33.bin").read_bytes()
    old = b"bootcmd=run distro_bootcmd\0"
    new = b"bootcmd=source 8fe00000"
    require(original.count(old) == 1, "BL33 defaults differ; inspect the boot command before patching")
    require(len(new) < len(old), "replacement boot command does not fit")
    patched = original.replace(old, new.ljust(len(old) - 1, b" ") + b"\0")

    boot = (PROJECT / "bootloader/boot.cmd").read_text()
    start = boot.index('if test "${scarlet_switchvisor_payload}" = 1; then')
    end = boot.index("if dtimg load", start)
    boot = boot[:start] + "echo Boot inputs supplied in RAM by Switchvisor\n" + boot[end:]
    boot = replace_once(
        boot,
        "if load mmc ${devnum}:${distro_bootpart} 0x8c000000 ${boot_dir}/usb-uart.dtbo; then",
        "if true; then",
    )
    boot = "setenv scarlet_switchvisor_payload 1\n" + boot
    require("load mmc" not in boot, "boot script still loads from SD")

    spec = importlib.util.spec_from_file_location(
        "package_l4t", ROOT / "projects/aarch64-switch-l4t/tools/package_l4t.py"
    )
    package = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(package)

    common = output / "common"
    common.mkdir(parents=True)
    for name in ("uImage", "initramfs"):
        shutil.copyfile(source / name, common / name)
    shutil.copyfile(dtimg, common / "nx-plat.dtimg")
    shutil.copyfile(overlay, common / "usb-uart.dtbo")
    (common / "bl33.bin").write_bytes(patched)
    (common / "boot.original.cmd").write_text((PROJECT / "bootloader/boot.cmd").read_text())
    original_hash = hashlib.sha256(original).hexdigest()
    matrix = []
    for name, sd_root, sd_driver in (
        ("sd-preloaded", True, True),
        ("ram-sd", False, True),
        ("ram-nosd", False, False),
    ):
        for shell in (False, True):
            variant = name + ("-shell" if shell else "")
            directory = output / variant
            directory.mkdir()
            script = boot
            if not sd_root:
                script = replace_once(script, " root=/dev/mmcblk0p4 rootfstype=ext2 rootwait", "")
            if not sd_driver:
                script = replace_once(
                    script, 'setenv bootargs "init=/init',
                    'fdt set /sdhci@700b0000 status disabled\nsetenv bootargs "init=/init',
                )
            if shell:
                script = replace_once(script, 'setenv bootargs "init=/init',
                                      'setenv bootargs "init=/init init.exec=/bin/sh')
            script = replace_once(script, "echo Launching Scarlet",
                                  f"echo PERFORMANCE_VARIANT={variant}\necho Launching Scarlet")
            (directory / "boot.cmd").write_text(script)
            data = script.encode()
            (directory / "boot.scr").write_bytes(package.legacy_image(
                struct.pack(">II", len(data), 0) + data, 6, "Scarlet RAM isolation", arch=2
            ))
            check_legacy_image(directory / "boot.scr", 6)
            images = [
                {"path": "../common/bl33.bin", "address": "0xaa000000",
                 "runtime_size": bl33_entry["runtime_size"]},
                {"path": "../common/uImage", "address": "0xa0000000"},
                {"path": "../common/initramfs", "address": "0x92000000"},
                {"path": "boot.scr", "address": "0x8fe00000"},
                {"path": "../common/nx-plat.dtimg", "address": "0xa8000000"},
                {"path": "../common/usb-uart.dtbo", "address": "0x8c000000"},
            ]
            ranges = []
            hashes = {}
            for entry in images:
                path = directory / entry["path"]
                size = path.stat().st_size
                extent = int(entry.get("runtime_size", str(size)), 0)
                require(extent >= size, f"runtime size smaller than file: {path}")
                address = int(entry["address"], 0)
                ranges.append((address, address + extent, entry["path"]))
                hashes[entry["path"]] = hashlib.sha256(path.read_bytes()).hexdigest()
            ranges.sort()
            for previous, current in zip(ranges, ranges[1:]):
                require(previous[1] <= current[0], f"upload ranges overlap: {previous}, {current}")
            # Reserve the separately expanded DTB buffer and its resize slack.
            for begin, end, path in ranges:
                require(end <= 0x8D000000 or begin >= 0x8D100000, f"upload overlaps DTB buffer: {path}")
            (directory / "bundle.json").write_text(json.dumps(
                {"version": 1, "entry": "0xaa000000", "images": images}, indent=2
            ) + "\n")
            (directory / "sha256.json").write_text(json.dumps(hashes, indent=2) + "\n")
            matrix.append({"variant": variant, "sd_root": sd_root, "sd_driver": sd_driver,
                           "init_exec": "/bin/sh" if shell else "/bin/stemd", "hardware_run": False})
    (output / "matrix.json").write_text(json.dumps(
        {"source_bundle": str(source), "original_bl33_sha256": original_hash, "variants": matrix}, indent=2
    ) + "\n")
    print(f"Prepared and statically checked {len(matrix)} bundles in {output}; none deployed.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source_bundle", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--dtimg", type=Path, default=PROJECT / ".scarlet/bootstack/nx-plat.dtimg")
    parser.add_argument("--overlay", type=Path,
                        default=ROOT.parent / "switchvisor/.cache/scarlet-uart/usb-uart.dtbo")
    args = parser.parse_args()
    prepare(args.source_bundle.resolve(), args.output.resolve(), args.dtimg.resolve(), args.overlay.resolve())

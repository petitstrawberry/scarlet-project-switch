#!/usr/bin/env python3
"""Install only Scarlet FAT32 boot files on the SD described by the handoff.

Defaults to a dry run. Validates the current disk layout via diskutil; never
opens a raw device, formats a filesystem, or modifies existing boot entries.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PACKAGE = ROOT / "projects/aarch64-switch-l4t/.scarlet/l4t"
EXPECTED_BYTES = 123773911040
EXPECTED_PARTITIONS = {1: 105054208 * 512, 2: 32 * 1024**3,
                       3: 61143040 * 512, 4: 4 * 1024**3}
PROTECTED = ["bootloader/hekate_ipl.ini", "bootloader/ini/L4T-noble.ini",
             "bootloader/sys/l4t", "switchroot/ubuntu-noble",
             "emuMMC/emummc.ini", "emuMMC/RAW2/raw_based", "atmosphere"]

def diskutil(*args):
    return plistlib.loads(subprocess.check_output(["/usr/sbin/diskutil", *args]))

def validate_mount(mount):
    if os.uname().sysname != "Darwin":
        raise ValueError("this disk-layout validation uses macOS diskutil")
    info = diskutil("info", "-plist", str(mount))
    if info.get("MountPoint") != str(mount) or info.get("FilesystemType") != "msdos":
        raise ValueError("target must be the mounted FAT32 SD volume itself")
    device = info["DeviceIdentifier"]
    whole = info["ParentWholeDisk"]
    if device != whole + "s1":
        raise ValueError("target must be MBR partition #1")
    disks = diskutil("list", "-plist", whole)["AllDisksAndPartitions"]
    disk = next(item for item in disks if item["DeviceIdentifier"] == whole)
    if disk.get("Content") != "FDisk_partition_scheme" or disk["Size"] != EXPECTED_BYTES:
        raise ValueError("SD capacity/MBR scheme does not match the handoff")
    partitions = {item["DeviceIdentifier"]: item for item in disk["Partitions"]}
    for number, size in EXPECTED_PARTITIONS.items():
        part = partitions.get(f"{whole}s{number}")
        if part is None or part["Size"] != size:
            raise ValueError(f"MBR partition #{number} size does not match the handoff")
    # diskutil reports physical SD partitions as Windows_FAT_32 and some disk
    # images as DOS_FAT_32. Check the actual filesystem, not that partition tag.
    if info.get("FilesystemName") != "MS-DOS FAT32":
        raise ValueError(f"target filesystem is not FAT32: {info.get('FilesystemName', '<unknown>')}")
    return device

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def protected_hashes(mount):
    result = {}
    for relative in PROTECTED:
        path = mount / relative
        if not path.exists(): raise ValueError(f"missing recovery/configuration path: {path}")
        files = sorted(path.rglob("*")) if path.is_dir() else [path]
        for file in files:
            if file.is_file(): result[str(file.relative_to(mount))] = digest(file)
    return result

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mount", type=Path, required=True, help='e.g. "/Volumes/SWITCH SD"; rediscover the current SD')
    parser.add_argument("--write", action="store_true", help="copy the verified files (default: dry run)")
    args = parser.parse_args()
    mount = args.mount.resolve(strict=True)
    device = validate_mount(mount)
    manifest = json.loads((PACKAGE / "manifest.json").read_text())
    files = []
    for relative, expected in manifest["sha256"].items():
        if relative != "bootloader/ini/L4T-scarlet.ini" and not relative.startswith("switchroot/scarlet/"):
            raise ValueError(f"unexpected package destination: {relative}")
        path = Path(relative)
        if path.is_absolute() or ".." in path.parts: raise ValueError("invalid destination path")
        source, target = PACKAGE / path, mount / path
        if not target.resolve().is_relative_to(mount): raise ValueError("destination escapes the volume")
        if digest(source) != expected: raise ValueError(f"package SHA256 mismatch: {source}")
        files.append((source, target, expected))
    before = protected_hashes(mount)
    print(f"Validated {device}; {len(files)} Scarlet files, {len(before)} protected files")
    for source, target, _ in files: print(f"{source.name} -> {target.relative_to(mount)}")
    if not args.write:
        print("Dry run complete. Add --write to install these FAT32 files.")
        return
    for source, target, expected in files:
        target.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=target.parent, prefix=".scarlet-", delete=False) as staged:
            temp = Path(staged.name)
            try:
                staged.write(source.read_bytes())
                staged.flush()
                os.fsync(staged.fileno())
            except BaseException:
                temp.unlink(missing_ok=True)
                raise
        try:
            if digest(temp) != expected: raise ValueError(f"SD staged-file SHA256 mismatch: {target}")
            temp.replace(target)
        finally:
            temp.unlink(missing_ok=True)
        if digest(target) != expected: raise ValueError(f"SD readback SHA256 mismatch: {target}")
    after = protected_hashes(mount)
    if after != before: raise ValueError("protected file content changed during installation")
    receipt = {"device": device, "mount": str(mount), "sha256": manifest["sha256"],
               "protected_files_verified": len(before), "raw_device_accessed": False,
               "hardware_boot_validated": False}
    (PACKAGE.parent / "sd-installation.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print("SD file readback and recovery/configuration hashes match. Eject the SD before unplugging.")

if __name__ == "__main__":
    try: main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error

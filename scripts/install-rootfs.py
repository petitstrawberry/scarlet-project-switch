#!/usr/bin/env python3
"""Deploy an existing ext2 image to the inspected SD's Scarlet partition (p4).

The default is a read-only plan. --write requires macOS administrator access,
unmounts this SD, validates its MBR, writes only p4, and verifies every byte.
This script does not build, resize, format, or repartition an image or device.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import stat
import struct
import subprocess
import time


DISK_BYTES = 123773911040
MBR_SHA256 = "fe40c9f4c23cc7696e566fd1fb7b04bd70c8cbe8dab5f105c8411fe9d1615aba"
PARTITIONS = [(0x0C, 32768, 105054208), (0x83, 105086976, 67108864),
              (0xE0, 180584448, 61143040), (0x83, 172195840, 8388608)]
CHUNK = 4 * 1024 * 1024


def disk_info(device):
    return plistlib.loads(subprocess.check_output(
        ["/usr/sbin/diskutil", "info", "-plist", device]))


def validate_disk(device):
    if os.uname().sysname != "Darwin" or not re.fullmatch(r"/dev/disk[0-9]+", device):
        raise ValueError("specify the rediscovered whole macOS SD device, e.g. /dev/disk12")
    whole = disk_info(device)
    if (not whole.get("WholeDisk") or whole.get("Internal") is not False
            or whole.get("VirtualOrPhysical") != "Physical"
            or whole.get("Content") != "FDisk_partition_scheme"
            or whole.get("Size") != DISK_BYTES or whole.get("DeviceBlockSize") != 512):
        raise ValueError("external SD capacity, sector size, or MBR scheme differs from the handoff")
    parts = []
    for index, (_, first, count) in enumerate(PARTITIONS, 1):
        info = disk_info(f"{device}s{index}")
        if (info.get("ParentWholeDisk") != whole["DeviceIdentifier"]
                or info.get("PartitionMapPartitionOffset") != first * 512
                or info.get("Size") != count * 512):
            raise ValueError(f"partition {index} offset/size differs from the handoff")
        parts.append(info)
    return parts


def read_exact(stream, count):
    result = bytearray()
    while len(result) < count:
        chunk = stream.read(count - len(result))
        if not chunk:
            raise ValueError("unexpected end of input")
        result.extend(chunk)
    return bytes(result)


def stream_hash(stream, size, label=None):
    digest = hashlib.sha256()
    done = 0
    while done < size:
        chunk = read_exact(stream, min(CHUNK, size - done))
        digest.update(chunk)
        done += len(chunk)
        if label and (done % (256 * 1024 * 1024) == 0 or done == size):
            print(f"{label}: {done}/{size} bytes", flush=True)
    return digest.hexdigest()


def protected_samples(stream):
    stream.seek(0)
    mbr = read_exact(stream, 512)
    if hashlib.sha256(mbr).hexdigest() != MBR_SHA256:
        raise ValueError("SD MBR fingerprint differs from the inspected card")
    entries = []
    for index in range(4):
        entry = mbr[446 + index * 16:462 + index * 16]
        first, count = struct.unpack_from("<II", entry, 8)
        entries.append((entry[4], first, count))
    if entries != PARTITIONS:
        raise ValueError("raw MBR partition layout differs from the handoff")
    hashes = {"mbr": MBR_SHA256}
    for number in (2, 3):
        _, first, count = PARTITIONS[number - 1]
        for name, offset in [("start", first * 512), ("end", (first + count) * 512 - CHUNK)]:
            stream.seek(offset)
            hashes[f"p{number}-{name}"] = hashlib.sha256(read_exact(stream, CHUNK)).hexdigest()
    return hashes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", required=True)
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--sha256", required=True, help="expected SHA-256 of the prepared image")
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{64}", args.sha256):
        raise ValueError("expected a lowercase SHA-256 digest")
    validate_disk(args.device)
    image = args.image.resolve(strict=True)
    with image.open("rb") as source:
        metadata = os.fstat(source.fileno())
        size = metadata.st_size
        if (not stat.S_ISREG(metadata.st_mode) or size < 2048
                or size > PARTITIONS[3][2] * 512 or size % 512):
            raise ValueError("image must be a sector-aligned regular file that fits p4")
        source.seek(1024)
        sb = read_exact(source, 1024)
        if struct.unpack_from("<H", sb, 56)[0] != 0xEF53:
            raise ValueError("image does not have an ext2 superblock")
        blocks = struct.unpack_from("<I", sb, 4)[0]
        log_block_size = struct.unpack_from("<I", sb, 24)[0]
        if log_block_size > 2 or blocks * (1024 << log_block_size) != size:
            raise ValueError("ext2 filesystem size does not match the image")
        source.seek(0)
        if stream_hash(source, size) != args.sha256:
            raise ValueError("source image SHA-256 mismatch")
        raw = "/dev/r" + args.device.removeprefix("/dev/")
        target = raw + "s4"
        print(f"Image: {image}\nSHA-256: {args.sha256}", flush=True)
        print(f"Target: {target}, {size} bytes, SD byte offset {PARTITIONS[3][1] * 512}", flush=True)
        if not args.write:
            print("Plan complete. --write also requires the exact raw MBR fingerprint and verifies p4 readback.")
            return
        subprocess.run(["/usr/sbin/diskutil", "unmountDisk", args.device], check=True)
        if any(part.get("MountPoint") for part in validate_disk(args.device)):
            raise ValueError("an SD partition is still mounted")
        for path in (raw, target):
            if not stat.S_ISCHR(os.lstat(path).st_mode):
                raise ValueError(f"expected a raw character device: {path}")
        started = time.monotonic()
        with open(raw, "rb", buffering=0) as disk:
            before = protected_samples(disk)
            with open(target, "r+b", buffering=0) as output:
                disk.seek(PARTITIONS[3][1] * 512)
                if read_exact(disk, CHUNK) != read_exact(output, CHUNK):
                    raise ValueError("p4 device does not match its whole-disk offset")
                output.seek(0)
                source.seek(0)
                written = 0
                while written < size:
                    chunk = read_exact(source, min(CHUNK, size - written))
                    pending = memoryview(chunk)
                    while pending:
                        n = output.write(pending)
                        if not n:
                            raise ValueError("short device write")
                        pending = pending[n:]
                    written += len(chunk)
                    if written % (256 * 1024 * 1024) == 0 or written == size:
                        print(f"write: {written}/{size} bytes", flush=True)
                os.fsync(output.fileno())
            with open(target, "rb", buffering=0) as check:
                if stream_hash(check, size, "verify") != args.sha256:
                    raise ValueError("p4 readback SHA-256 mismatch")
            after = protected_samples(disk)
            if before != after:
                raise ValueError("protected MBR or neighboring partition samples changed")
        print(json.dumps({"device": target, "bytes": size, "sha256": args.sha256,
                          "readback_verified": True, "protected_samples": after,
                          "elapsed_seconds": round(time.monotonic() - started, 1)}, indent=2), flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error

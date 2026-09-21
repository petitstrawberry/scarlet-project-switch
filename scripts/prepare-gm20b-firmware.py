#!/usr/bin/env python3
"""Prepare NVIDIA's pinned GM20B firmware, including WHENCE links."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
MANIFEST = PROJECT / "gpu-firmware.json"
OUTPUT = PROJECT / ".scarlet/gm20b-firmware"


def matches(blob, expected):
    return len(blob) == expected["size"] and hashlib.sha256(blob).hexdigest() == expected["sha256"]


def prepare(source=None, download=False):
    pins = json.loads(MANIFEST.read_text())
    if pins["schema"] != 1 or pins["repository"] != "NVIDIA/linux-firmware":
        raise ValueError("unsupported GPU firmware manifest")
    if len(pins["revision"]) != 40 or any(c not in "0123456789abcdef" for c in pins["revision"]):
        raise ValueError("firmware revision must be a full commit hash")
    for name, expected in pins["files"].items():
        path = Path(name)
        if path.is_absolute() or ".." in path.parts or not (
            name == "LICENCE.nvidia" or name.startswith("nvidia/gm20b/")
        ):
            raise ValueError(f"unexpected GPU firmware path: {name}")
        destination = OUTPUT / "lib/firmware" / path
        # WHENCE at the pinned revision defines this generated distribution
        # link. The git tree contains its GM200 target, not the GM20B link.
        source_name = expected.get("source", name)
        if source_name != name and (name, source_name) != (
            "nvidia/gm20b/gr/sw_method_init.bin",
            "nvidia/gm200/gr/sw_method_init.bin",
        ):
            raise ValueError(f"unexpected WHENCE firmware source: {source_name}")
        if destination.is_file() and matches(destination.read_bytes(), expected):
            continue
        if source is not None:
            blob = (source / source_name).read_bytes()
        elif download:
            # gh returns JSON/base64 so binary firmware never passes through
            # terminal text decoding. Request only the pinned public source.
            entry = json.loads(subprocess.check_output([
                "gh", "api", f"repos/{pins['repository']}/contents/{source_name}?ref={pins['revision']}",
            ]))
            if entry.get("encoding") != "base64":
                raise ValueError(f"unexpected download encoding: {name}")
            blob = base64.b64decode(entry["content"])
        else:
            raise ValueError(
                f"missing/mismatched GPU firmware: {name}; run "
                "python3 scripts/prepare-gm20b-firmware.py --download "
                "or --source <linux-firmware root>"
            )
        if not matches(blob, expected):
            raise ValueError(f"GPU firmware size/hash mismatch: {name}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(blob)
    # Preserve the redistributable firmware's licence beside its installed files.
    (OUTPUT / "lib/firmware/nvidia/gm20b/manifest.json").write_text(json.dumps(pins, indent=2) + "\n")
    return OUTPUT, len(pins["files"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, help="directory containing nvidia/gm20b and LICENCE.nvidia")
    parser.add_argument("--download", action="store_true", help="fetch the pinned public files using gh")
    args = parser.parse_args()
    if args.source is not None and args.download:
        parser.error("choose --source or --download")
    try:
        output, count = prepare(args.source, args.download)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"GPU firmware preparation failed: {error}\n")
    print(f"GM20B firmware: {count} pinned firmware/licence files verified; {output}")


if __name__ == "__main__":
    main()

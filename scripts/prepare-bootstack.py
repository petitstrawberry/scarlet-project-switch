#!/usr/bin/env python3
"""Import the inspected Noble boot files into local generated state."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t"

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path, help="Noble boot directory containing bl31.bin, bl33.bin, nx-plat.dtimg")
    args = parser.parse_args()
    pins = json.loads((PROJECT / "bootstack.json").read_text())
    inputs = []
    for name, expected in pins["files"].items():
        source = args.source / name
        actual = hashlib.sha256(source.read_bytes()).hexdigest()
        if actual != expected:
            parser.error(f"{name}: SHA256 mismatch; review and update bootstack.json for a different stack")
        inputs.append((source, name))
    destination = PROJECT / ".scarlet/bootstack"
    destination.mkdir(parents=True, exist_ok=True)
    for source, name in inputs:
        shutil.copyfile(source, destination / name)
        print(f"verified {name}")

if __name__ == "__main__":
    main()

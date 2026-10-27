#!/usr/bin/env python3
"""Install the board's separately imported, hash-pinned Wi-Fi firmware."""
import hashlib
import json
import sys
from pathlib import Path
import shutil

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
def main():
    if len(sys.argv) != 2:
        raise ValueError("expected the bundle output directory")
    target = Path(sys.argv[1])
    pins = json.loads((PROJECT / "wifi-firmware.json").read_text())
    target.mkdir(parents=True, exist_ok=True)
    for name, pin in pins["files"].items():
        source = PROJECT / "firmware/wifi" / name
        data = source.read_bytes()
        if len(data) != pin["size"] or hashlib.sha256(data).hexdigest() != pin["sha256"]:
            raise ValueError(f"Wi-Fi firmware hash mismatch: {name}")
        shutil.copyfile(source, target / name)
if __name__ == "__main__":
    main()

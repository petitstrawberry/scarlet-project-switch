#!/usr/bin/env python3
"""Verify the checked-in Mesa compiler inputs and GM20B shader pack."""

import hashlib
import json
from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = ROOT / "shared/maxwell-shader-pack/artifacts/gm20b"
MESA_SHA = "e881540692daac6532cefec76699f7a025563767"
METADATA_SHA = "0fd59dd61d6f54645f7adef64f2805954c08b39d46d799243be350a6993883b1"


def main():
    entries = {}
    for line in (ARTIFACTS / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split("  ", 1)
        if Path(name).name != name or name in entries:
            raise SystemExit("invalid shader checksum manifest")
        data = (ARTIFACTS / name).read_bytes()
        if hashlib.sha256(data).hexdigest() != digest:
            raise SystemExit(f"Maxwell shader artifact checksum mismatch: {name}")
        entries[name] = data
    expected = {p.name for p in ARTIFACTS.iterdir() if p.is_file() and p.name != "SHA256SUMS"}
    if entries.keys() != expected:
        raise SystemExit("shader checksum manifest does not cover the complete pack")
    metadata_bytes = entries["mesa-metadata.json"]
    if hashlib.sha256(metadata_bytes).hexdigest() != METADATA_SHA:
        raise SystemExit("shader metadata differs from the kernel's pinned pack")
    metadata = json.loads(metadata_bytes)
    assert metadata["mesa_sha"] == MESA_SHA
    assert metadata["chipset"] == 0x12B
    assert metadata["uniform_bytes"] == 160
    assert len(metadata["variants"]) == 15
    names = set()
    for variant in metadata["variants"]:
        name = variant["name"]
        assert name not in names
        names.add(name)
        code = entries[name + ".bin"]
        header = entries[name + ".header.bin"]
        assert 0 < len(code) <= 0xF80
        assert len(code) == variant["binary_bytes"]
        assert len(header) == 80
        assert list(struct.unpack("<20I", header)) == variant["header"]
        assert variant["header_offset"] == 0x30
        assert variant["code_offset"] == 0x80
        assert variant["tls_bytes"] == 0
        assert 4 <= variant["gprs"] <= 255
        assert variant["aux_cb_slot"] == 15
        assert variant["texture_handle_offset"] == 0x20
    print(f"Verified {len(names)} GM20B Mesa shaders and {len(entries)} pinned artifacts")


if __name__ == "__main__":
    main()

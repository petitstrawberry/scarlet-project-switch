"""Validate a staged Linux ICD package before adding it to the Switch SD image."""
import hashlib
import json
from pathlib import Path
import struct

FILES = {
    "usr/lib/aarch64-linux-gnu/libvulkan_sgfx.so",
    "usr/lib/aarch64-linux-gnu/libsws_client_c.so",
    "usr/lib/sgfx/libsgfx_scarlet_virgl.so",
    "usr/lib/sgfx/libsgfx_scarlet_maxwell.so",
    "usr/lib/sgfx/scarlet-virgl.sgfx-driver",
    "usr/lib/sgfx/scarlet-maxwell.sgfx-driver",
    "usr/share/vulkan/icd.d/sgfx.json",
}


def validate_build(directory):
    directory = Path(directory).resolve(strict=True)
    report = json.loads((directory / "build.json").read_text())
    if (report.get("result") != "PASS" or report.get("target") != "aarch64-unknown-linux-gnu"
            or "nvidia-gm20b" not in report.get("backends", [])):
        raise ValueError("expected a successful Linux ICD build including the Maxwell plugin")
    artifacts = report["artifacts"]
    if len(artifacts) != len(FILES) or {item["path"] for item in artifacts} != FILES:
        raise ValueError("Linux ICD package has an incomplete or unexpected artifact list")
    rootfs = directory / "rootfs"
    for item in artifacts:
        path = rootfs / item["path"]
        if not path.resolve(strict=True).is_relative_to(rootfs):
            raise ValueError(f"artifact escapes staging directory: {path}")
        data = path.read_bytes()
        if hashlib.sha256(data).hexdigest() != item["sha256"]:
            raise ValueError(f"staged artifact differs from build report: {path}")
        if path.suffix == ".so" and (len(data) < 20 or data[:8] != b"\x7fELF\x02\x01\x01\x00"
                                    or struct.unpack_from("<HH", data, 16) != (3, 183)):
            raise ValueError(f"not a Linux ELF64 AArch64 library: {path}")
    return rootfs

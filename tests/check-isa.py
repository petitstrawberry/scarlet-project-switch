#!/usr/bin/env python3
"""Check built executables for LSE instructions unavailable on Cortex-A57."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t"
LSE = re.compile(r"(?:cas[p]?|swp|ld(?:add|clr|eor|set|smax|smin|umax|umin)|st(?:add|clr|eor|set|smax|smin|umax|umin))(?:a|al|l)?(?:b|h)?")


def packaged_elves(project, boot_directory):
    # Inspect the actual bytes passed to the kernel, not a separate Cargo build.
    data = (project / ".scarlet/l4t/switchroot" / boot_directory / "initramfs").read_bytes()[64:]
    position = 0
    while True:
        header = data[position:position + 110]
        if header[:6] != b"070701":
            raise ValueError("packaged RAMDisk is not raw newc CPIO")
        fields = [int(header[6 + i * 8:14 + i * 8], 16) for i in range(13)]
        size, name_size = fields[6], fields[11]
        name = data[position + 110:position + 110 + name_size - 1].decode()
        start = (position + 110 + name_size + 3) & ~3
        if name == "TRAILER!!!":
            return
        payload = data[start:start + size]
        if payload.startswith(b"\x7fELF"):
            yield name.lstrip("./"), payload
        position = (start + size + 3) & ~3


def inspect(path):
    # Enable the extension in the decoder so prohibited instructions do not
    # appear merely as <unknown> under a baseline AArch64 disassembler.
    asm = subprocess.check_output(["llvm-objdump", "--mattr=+lse", "--no-show-raw-insn", "-d", str(path)], text=True)
    names = re.findall(r"^\s*[0-9a-f]+:\s+([a-z0-9]+)\s", asm, re.M)
    matches = [name for name in names if LSE.fullmatch(name)]
    if matches:
        raise ValueError(f"{path}: unsupported LSE instructions: {sorted(set(matches))}")
    return {"file": str(path.relative_to(ROOT)), "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "decoded_instructions": len(names), "lse_instructions": 0}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--console", action="store_true", help="check every executable in the actual SWS console RAMDisk")
    args = parser.parse_args()
    project = ROOT / "projects/aarch64-switch-console" if args.console else PROJECT
    boot_directory = "scarlet-console" if args.console else "scarlet"
    cache = ROOT / ".cache"
    cache.mkdir(exist_ok=True)
    results = [inspect(project / "bsp/target/aarch64-switch-none-elf/release/scarlet")]
    found_init = False
    for name, elf in packaged_elves(project, boot_directory):
        if not args.console and name != "init":
            continue
        if len(elf) < 64 or elf[7] != 0x53:
            raise ValueError(f"/{name} must retain Scarlet Native ELF OSABI 0x53")
        found_init |= name == "init"
        path = cache / "console-packaged-elf" / name if args.console else cache / "switch-init-packaged.elf"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(elf)
        result = inspect(path)
        result["image_path"] = "/" + name
        results.append(result)
    if not found_init:
        raise ValueError("/init missing from packaged RAMDisk")
    for result in results:
        print(f"PASS {result['file']}: {result['decoded_instructions']} instructions; no LSE")
    output = "console-isa-verification.json" if args.console else "isa-verification.json"
    (cache / output).write_text(json.dumps({"native_osabi": "0x53", "executables": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()

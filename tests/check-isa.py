#!/usr/bin/env python3
"""Check built executables for LSE instructions unavailable on Cortex-A57."""
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t"
LSE = re.compile(r"(?:cas[p]?|swp|ld(?:add|clr|eor|set|smax|smin|umax|umin)|st(?:add|clr|eor|set|smax|smin|umax|umin))(?:a|al|l)?(?:b|h)?")


def init_elf():
    # Inspect the actual bytes passed to the kernel, not a separate Cargo build.
    data = (PROJECT / ".scarlet/l4t/switchroot/scarlet/initramfs").read_bytes()[64:]
    position = 0
    while True:
        header = data[position:position + 110]
        if header[:6] != b"070701":
            raise ValueError("packaged RAMDisk is not raw newc CPIO")
        fields = [int(header[6 + i * 8:14 + i * 8], 16) for i in range(13)]
        size, name_size = fields[6], fields[11]
        name = data[position + 110:position + 110 + name_size - 1].decode()
        start = (position + 110 + name_size + 3) & ~3
        if name.lstrip("./") == "init":
            return data[start:start + size]
        if name == "TRAILER!!!":
            raise ValueError("/init missing from packaged RAMDisk")
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
    cache = ROOT / ".cache"
    cache.mkdir(exist_ok=True)
    elf = init_elf()
    if elf[:4] != b"\x7fELF" or elf[7] != 0x53:
        raise ValueError("/init must retain Scarlet Native ELF OSABI 0x53")
    init = cache / "switch-init-packaged.elf"
    init.write_bytes(elf)
    results = [inspect(PROJECT / "bsp/target/aarch64-switch-none-elf/release/scarlet"), inspect(init)]
    for result in results:
        print(f"PASS {result['file']}: {result['decoded_instructions']} instructions; no LSE")
    (cache / "isa-verification.json").write_text(json.dumps({"native_osabi": "0x53", "executables": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()

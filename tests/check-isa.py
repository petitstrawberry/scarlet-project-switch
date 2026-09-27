#!/usr/bin/env python3
"""Reject unguarded LSE; permit recognized userspace outline-atomic fallbacks."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "tests/boot-probe"
LSE = re.compile(r"(?:cas[p]?|swp|ld(?:add|clr|eor|set|smax|smin|umax|umin)|st(?:add|clr|eor|set|smax|smin|umax|umin))(?:a|al|l)?(?:b|h)?")


def packaged_elves(boot):
    # Inspect the actual bytes passed to the kernel, not a separate Cargo build.
    data = (boot / "initramfs").read_bytes()[64:]
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


def audit_disassembly(asm, allow_outline_atomics=False):
    functions = []
    instructions = []
    for line in asm.splitlines():
        symbol = re.fullmatch(r"[0-9a-f]+ <([^>]+)>:", line)
        if symbol:
            instructions = []
            functions.append((symbol[1], instructions))
        instruction = re.match(r"^\s*([0-9a-f]+):\s+([a-z0-9.]+)\s*(.*)", line)
        if instruction:
            if not functions:
                functions.append(("<unknown>", instructions))
            instructions.append((int(instruction[1], 16), instruction[2], instruction[3]))
    lse_count = 0
    for symbol, instructions in functions:
        hits = [i for i, (_, op, _) in enumerate(instructions) if LSE.fullmatch(op)]
        if not hits:
            continue
        # Rust/compiler-builtins dispatches on a runtime LSE flag. On A57 the
        # CBZ takes the LL/SC path; merely containing the alternate opcode is
        # not a minimum-ISA violation. Keep kernel checks strictly LSE-free.
        valid = allow_outline_atomics and re.fullmatch(
            r"__aarch64_(?:cas|swp|ldadd|ldclr|ldeor|ldset)(?:1|2|4|8|16)_(?:relax|acq|rel|acq_rel|sync)", symbol)
        valid = valid and hits == [3] and len(instructions) > 5
        if valid:
            _, first, first_args = instructions[0]
            _, second, second_args = instructions[1]
            _, guard, guard_args = instructions[2]
            target = re.match(r"w16, (0x[0-9a-f]+)(?:\s|$)", guard_args)
            fallback = [op for _, op, _ in instructions[5:]]
            valid = (first == "adrp" and first_args.startswith("x16,")
                     and second == "ldrb" and second_args.startswith("w16, [x16")
                     and guard == "cbz" and target is not None
                     and int(target[1], 16) == instructions[5][0]
                     and instructions[4][1] == "ret"
                     and any(re.fullmatch(r"ld(?:a?xr[bh]?|a?xp)", op) for op in fallback)
                     and any(re.fullmatch(r"st(?:l?xr[bh]?|l?xp)", op) for op in fallback))
        if not valid:
            raise ValueError(f"unsupported/unguarded LSE in {symbol}: {[instructions[i][1] for i in hits]}")
        lse_count += len(hits)
    return sum(len(instructions) for _, instructions in functions), lse_count


def inspect(path, allow_outline_atomics=False):
    asm = subprocess.check_output(["llvm-objdump", "--mattr=+lse", "--no-show-raw-insn", "-d", str(path)], text=True)
    try:
        count, outlined = audit_disassembly(asm, allow_outline_atomics)
    except ValueError as error:
        raise ValueError(f"{path}: {error}") from error
    return {"file": str(path.relative_to(ROOT)), "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "decoded_instructions": count, "unguarded_lse_instructions": 0,
            "guarded_outline_lse_instructions": outlined}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--console", action="store_true", help="check every executable in the actual SWS console RAMDisk")
    args = parser.parse_args()
    project = ROOT / "projects/aarch64-switch-l4t-console" if args.console else PROJECT
    boot = project / (".scarlet/l4t/switchroot/scarlet-console" if args.console else ".scarlet/boot")
    cache = ROOT / ".cache"
    cache.mkdir(exist_ok=True)
    results = [inspect(project / "bsp/target/aarch64-switch-none-elf/release/scarlet")]
    found_init = False
    for name, elf in packaged_elves(boot):
        if not args.console and name != "init":
            continue
        if len(elf) < 64 or elf[7] != 0x53:
            raise ValueError(f"/{name} must retain Scarlet Native ELF OSABI 0x53")
        found_init |= name == "init"
        path = cache / "console-packaged-elf" / name if args.console else cache / "switch-init-packaged.elf"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(elf)
        result = inspect(path, allow_outline_atomics=True)
        result["image_path"] = "/" + name
        results.append(result)
    if not found_init:
        raise ValueError("/init missing from packaged RAMDisk")
    for result in results:
        print(f"PASS {result['file']}: {result['decoded_instructions']} instructions; no unguarded LSE; {result['guarded_outline_lse_instructions']} guarded outline atomics")
    output = "console-isa-verification.json" if args.console else "isa-verification.json"
    (cache / output).write_text(json.dumps({"native_osabi": "0x53", "executables": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Package a physical-link Scarlet Image for the inspected Noble U-Boot."""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import zlib

PROJECT = Path(__file__).resolve().parents[1]
LOAD = 0x80200000
MAGIC = 0x27051956

def legacy_image(payload, kind, name, load=0, entry=0, compression=0, arch=22):
    """U-Boot's 64-byte legacy header; fields and CRCs are big endian."""
    timestamp = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    header = struct.pack(
        ">7I4B32s", MAGIC, 0, timestamp, len(payload), load, entry,
        zlib.crc32(payload), 5, arch, kind, compression,
        name.encode("ascii")[:32].ljust(32, b"\0"),
    )
    return header[:4] + struct.pack(">I", zlib.crc32(header)) + header[8:] + payload

def validate_image(image, elf):
    if len(elf) < 64 or elf[:6] != b"\x7fELF\x02\x01":
        raise ValueError("expected little-endian ELF64")
    if struct.unpack_from("<H", elf, 18)[0] != 183:
        raise ValueError("expected AArch64 ELF")
    entry, phoff = struct.unpack_from("<QQ", elf, 24)
    phsize, phcount = struct.unpack_from("<HH", elf, 54)
    if entry != LOAD or phsize != 56:
        raise ValueError("ELF entry or program header size violates the BSP contract")
    segments = []
    for index in range(phcount):
        kind, flags, offset, vaddr, paddr, filesz, memsz, align = struct.unpack_from("<II6Q", elf, phoff + index * phsize)
        if kind == 1:
            if vaddr != paddr or paddr < LOAD:
                raise ValueError("kernel must be physically linked with identity load segments")
            segments.append((paddr, memsz))
    if not segments or min(start for start, _ in segments) != LOAD:
        raise ValueError("kernel's first load segment must begin at 0x80200000")
    if len(image) < 64 or image[56:60] != b"ARM\x64":
        raise ValueError("missing Linux arm64 Image header")
    text_offset, image_size, flags = struct.unpack_from("<3Q", image, 8)
    if text_offset != 0x200000 or flags != 2:
        raise ValueError("Image placement/page-size flags violate the BSP contract")
    if not len(image) <= image_size < 0x8d000000 - LOAD:
        raise ValueError("Image size exceeds the DTB boundary or omits file data")
    if max(start + size for start, size in segments) != LOAD + image_size:
        raise ValueError("Image size does not cover exactly the ELF runtime reservation")
    return image_size

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=["release", "debug"], default="release")
    args = parser.parse_args()
    elf = PROJECT / f"bsp/target/aarch64-switch-none-elf/{args.profile}/scarlet"
    initrd = PROJECT / ".scarlet/images/initramfs.cpio"
    stack = PROJECT / ".scarlet/bootstack"
    pins = json.loads((PROJECT / "bootstack.json").read_text())
    for name, expected in pins["files"].items():
        path = stack / name
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            parser.error(f"missing or mismatched {path}; run scripts/prepare-bootstack.py --source <Noble boot directory>")
    output = PROJECT / ".scarlet/l4t"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output) as tmp:
        tmp = Path(tmp)
        raw = tmp / "Image"
        # SDK 1.0 adds an allocated .ksym sidecar at address zero, outside every
        # PT_LOAD. Exclude it from the flat image to avoid a 2 GiB address gap.
        subprocess.run(["llvm-objcopy", "--remove-section=.ksym", "-O", "binary", str(elf), str(raw)], check=True)
        image = raw.read_bytes()
        image_size = validate_image(image, elf.read_bytes())
        if not initrd.read_bytes().startswith(b"070701"):
            parser.error("initramfs must be a CPIO newc archive")
        boot = tmp / "switchroot/scarlet"
        boot.mkdir(parents=True)
        (boot / "Image").write_bytes(image)
        (boot / "uImage").write_bytes(legacy_image(gzip.compress(image, mtime=0), 2, "Scarlet Switch", LOAD, LOAD, 1))
        # bootm strips the legacy RAMDisk header but does not decompress its
        # payload. Scarlet's initramfs parser needs raw CPIO, unlike Linux.
        (boot / "initramfs").write_bytes(legacy_image(initrd.read_bytes(), 3, "Scarlet initramfs"))
        script = (PROJECT / "bootloader/boot.cmd").read_bytes()
        # Script payload is a big-endian length, zero terminator, then text.
        (boot / "boot.scr").write_bytes(legacy_image(struct.pack(">II", len(script), 0) + script, 6, "Scarlet L4T boot", arch=2))
        for name in pins["files"]:
            shutil.copyfile(stack / name, boot / name)
        ini = tmp / "bootloader/ini/L4T-scarlet.ini"
        ini.parent.mkdir(parents=True)
        shutil.copyfile(PROJECT / "bootloader/L4T-scarlet.ini", ini)
        hashes = {str(p.relative_to(tmp)): hashlib.sha256(p.read_bytes()).hexdigest()
                  for p in sorted(tmp.rglob("*")) if p.is_file() and p != raw}
        try:
            kernel_rev = subprocess.check_output(["git", "-C", str(PROJECT.parents[2] / "Scarlet"), "rev-parse", "HEAD"], text=True).strip()
        except subprocess.CalledProcessError:
            kernel_rev = "unknown"
        manifest = {
            "hardware_validated": False,
            "kernel_source_revision": kernel_rev,
            "kernel_elf_sha256": hashlib.sha256(elf.read_bytes()).hexdigest(),
            "kernel_load": hex(LOAD), "kernel_entry": hex(LOAD),
            "text_offset": "0x200000", "image_runtime_size": image_size,
            "dtb_selection_buffer": "0x8d000000",
            "initramfs_load_buffer": "0x92000000",
            "initramfs_payload_start": "0x92000040",
            "initramfs_format": "uncompressed CPIO newc in a legacy RAMDisk header",
            "uboot_fdt_initrd_relocation": False,
            "entry_contract": "Linux arm64 Image; x0=physical DTB; EL1/EL2; MMU off; DAIF masked",
            "sha256": hashes,
        }
        (tmp / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        for name in ["switchroot", "bootloader"]:
            shutil.copytree(tmp / name, output / name, dirs_exist_ok=True)
        shutil.copyfile(tmp / "manifest.json", output / "manifest.json")
    print(f"L4T files packaged: {output}")
    print(f"entry={LOAD:#x}, runtime reservation={image_size:#x}; hardware validation pending")

if __name__ == "__main__":
    main()

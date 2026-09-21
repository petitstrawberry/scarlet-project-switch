#!/usr/bin/env python3
"""Package the boot fixture for QEMU without firmware or an SD menu entry."""
import gzip
import importlib.util
import os
from pathlib import Path
import subprocess

PROJECT = Path(__file__).resolve().parent
ROOT = PROJECT.parents[1]
spec = importlib.util.spec_from_file_location(
    "package_l4t",
    ROOT / "projects/aarch64-switch-l4t-console/tools/package_l4t.py",
)
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


def main():
    profile = os.environ.get("SCARLET_PROFILE", "release")
    elf = PROJECT / f"bsp/target/aarch64-switch-none-elf/{profile}/scarlet"
    initrd = (PROJECT / ".scarlet/images/initramfs.cpio").read_bytes()
    if not initrd.startswith(b"070701"):
        raise ValueError("boot fixture must contain an uncompressed newc initramfs")
    output = PROJECT / ".scarlet/boot"
    output.mkdir(parents=True, exist_ok=True)
    subprocess.run([
        "llvm-objcopy", "--remove-section=.ksym", "-O", "binary",
        str(elf), str(output / "Image"),
    ], check=True)
    image = (output / "Image").read_bytes()
    package.validate_image(image, elf.read_bytes())
    (output / "uImage").write_bytes(package.legacy_image(
        gzip.compress(image, mtime=0), 2, "Scarlet boot test",
        package.LOAD, package.LOAD, 1,
    ))
    (output / "initramfs").write_bytes(package.legacy_image(
        initrd, 3, "Scarlet test initramfs",
    ))
    print(f"QEMU boot fixture packaged: {output}")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Stage the Switch Linux-ABI ICD with Maxwell and VirGL plugins; no deployment."""
import argparse
from pathlib import Path
import subprocess
from linux_vulkan_package import validate_build

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scarlet-source", type=Path, required=True,
                        help="Scarlet checkout containing the Linux dynamic ICD build recipe")
    parser.add_argument("--sgfx-source", type=Path, required=True,
                        help="SGFX checkout containing Linux dynamic backend support")
    parser.add_argument("--output", type=Path, default=PROJECT / ".scarlet/linux-vulkan")
    args = parser.parse_args()
    subprocess.run(["bash", str(args.scarlet_source.resolve() / "tools/graphics/build-linux-vulkan.sh"),
                    str(args.sgfx_source.resolve()), "--maxwell-source", str(ROOT),
                    "--output", str(args.output.resolve()), "--stage-only"], check=True)
    rootfs = validate_build(args.output)
    print(f"Validated Linux Vulkan package: {rootfs}")
    print(f"Include it with prepare-console.py --linux-vulkan-build {args.output.resolve()}")


if __name__ == "__main__":
    main()

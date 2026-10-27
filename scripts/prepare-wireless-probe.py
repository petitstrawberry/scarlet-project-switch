#!/usr/bin/env python3
"""Build a RAM-only DT/CAR/PMC inventory probe for the Switchvisor guest.

Run in this project's Nix shell. This only prepares files; it does not deploy.
Inventory is the default. Explicit flags enable PCIe or watchdog diagnostics.
Radio firmware, storage and DMA are not initialized by this probe.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
import os
import re
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
PROBE = ROOT / "tests/wireless-probe"


def prepare(output, initialize=False, watchdog_test=False, radio=False, msgbuf=False, scan=False):
    msgbuf = msgbuf or scan
    radio = radio or msgbuf
    initialize = initialize or radio
    if output.exists():
        raise ValueError("choose a new output directory to preserve earlier evidence")
    stack = PROJECT / "firmware/bootstack"
    pins = json.loads((PROJECT / "bootstack.json").read_text())
    for name in ("bl33.bin", "nx-plat.dtimg"):
        if hashlib.sha256((stack / name).read_bytes()).hexdigest() != pins["files"][name]:
            raise ValueError(f"bootstack hash mismatch: {name}")
    overlay = PROJECT / ".scarlet/switchvisor/switchroot/scarlet-switchvisor/usb-uart.dtbo"
    if not overlay.is_file():
        raise ValueError("prepare the project's Switchvisor UART overlay first")
    output.mkdir(parents=True)
    env = os.environ.copy()
    env["CARGO_HOME"] = str(PROJECT / ".scarlet/cache/cargo-home")
    env["CARGO_TARGET_DIR"] = str(output / "build")
    firmware_sources = []
    if radio:
        firmware_pins = json.loads((PROJECT / "wifi-firmware.json").read_text())
        firmware_sources.append(PROJECT / "wifi-firmware.json")
        for name, expected in firmware_pins["files"].items():
            path = PROJECT / "firmware/wifi" / name
            if path.stat().st_size != expected["size"] or hashlib.sha256(path.read_bytes()).hexdigest() != expected["sha256"]:
                raise ValueError(f"Wi-Fi firmware hash mismatch: {name}")
            firmware_sources.append(path)
        env["SCARLET_PROBE_WIFI_FIRMWARE"] = str(PROJECT / "firmware/wifi/brcmfmac4356A3-pcie.bin")
        env["SCARLET_PROBE_WIFI_NVRAM"] = str(PROJECT / "firmware/wifi/brcmfmac4356A3-pcie.txt")
    env["RUSTFLAGS"] = (f"-C link-arg=-T{PROBE / 'linker.ld'} -C target-cpu=cortex-a57 "
                        "-C code-model=large -C no-vectorize-loops -C no-vectorize-slp")
    with tempfile.TemporaryDirectory(prefix="host-tests-", dir=output) as temporary:
        with (output / "host-tests.log").open("w") as log:
            for source in [ROOT / "drivers/pcie/tegra210/src/lib.rs",
                           ROOT / "drivers/network/brcmfmac/src/lib.rs", PROBE / "src/recovery.rs"]:
                binary = str(Path(temporary) / "tests")
                subprocess.run(["rustc", "--edition=2024", "--test", str(source), "-o", binary],
                               stdout=log, stderr=subprocess.STDOUT, check=True)
                subprocess.run([binary], stdout=log, stderr=subprocess.STDOUT, check=True)
    with (output / "build.log").open("w") as log:
        subprocess.run(["cargo", "build", "--manifest-path", str(PROBE / "Cargo.toml"),
                        "--release", "--target", "aarch64-unknown-none-softfloat",
                        *(["--features", "radio-scan" if scan else "radio-msgbuf" if msgbuf else "radio-firmware"] if radio else []),
                        "-Zbuild-std=core,compiler_builtins", "-Zbuild-std-features=compiler-builtins-mem"],
                       cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    elf = output / "probe.elf"
    shutil.copyfile(output / "build/aarch64-unknown-none-softfloat/release/switch-wireless-probe", elf)
    disassembly = subprocess.check_output(["llvm-objdump", "-d", str(elf)], text=True)
    if re.search(r"\b(?:ld(?:a)?xr[bh]?|st(?:l)?xr[bh]?|ld(?:a)?xp|st(?:l)?xp|cas[a-z]*|swp[a-z]*)\s", disassembly):
        raise ValueError("MMU-off probe contains exclusive/atomic RMW instructions")
    subprocess.run(["llvm-objcopy", "-O", "binary", str(elf), str(output / "Image")], check=True)
    spec = importlib.util.spec_from_file_location("package_l4t", PROJECT / "tools/package_l4t.py")
    package = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(package)
    image = (output / "Image").read_bytes()
    package.validate_image(image, elf.read_bytes())
    (output / "uImage").write_bytes(package.legacy_image(
        gzip.compress(image, mtime=0), 2, "Scarlet wireless inventory", package.LOAD, package.LOAD, 1))
    original = (stack / "bl33.bin").read_bytes()
    old = b"bootcmd=run distro_bootcmd\0"
    new = b"bootcmd=source 8fe00000"
    if original.count(old) != 1 or len(new) >= len(old):
        raise ValueError("unexpected U-Boot default environment")
    (output / "bl33.bin").write_bytes(original.replace(old, new.ljust(len(old) - 1, b" ") + b"\0"))
    shutil.copyfile(stack / "nx-plat.dtimg", output / "nx-plat.dtimg")
    shutil.copyfile(overlay, output / "usb-uart.dtbo")
    # No SD reads/writes, regulators, PCIe reset or clock changes in this script.
    script = '''setenv fdt_high 0xffffffffffffffff
if test "${t210b01}" = 0 -a "${sku}" = 0; then
    if dtimg load 0xa8000000 ${sku} 0x8d000000 fdtrsize; then
        fdt addr 0x8d000000 ${fdtrsize}
        fdt resize 16384
        if fdt apply 0x8c000000; then
            setenv bootargs scarlet.wireless-probe=1
            bootm 0xa0000000 - 0x8d000000
        fi
    fi
fi
echo WIRELESS_PROBE_BOOT_FAILED
'''
    script = script.replace("setenv bootargs scarlet.wireless-probe=1", '''if test -n "${device_wifi_mac}"; then
                fdt set /chosen nvidia,wifi-mac ${device_wifi_mac}
            fi
            setenv bootargs scarlet.wireless-probe=1''')
    if initialize:
        script = script.replace("setenv bootargs scarlet.wireless-probe=1",
                                "fdt set /chosen scarlet,wireless-probe-initialize\n            setenv bootargs scarlet.wireless-probe=1")
    if watchdog_test:
        script = script.replace("setenv bootargs scarlet.wireless-probe=1",
                                "fdt set /chosen scarlet,wireless-probe-watchdog-test\n            setenv bootargs scarlet.wireless-probe=1")
    (output / "boot.cmd").write_text(script)
    (output / "boot.scr").write_bytes(package.legacy_image(
        struct.pack(">II", len(script), 0) + script.encode(), 6, "Scarlet Wi-Fi inventory", arch=2))
    images = [
        {"path": "bl33.bin", "address": "0xaa000000", "runtime_size": "0x68200"},
        {"path": "uImage", "address": "0xa0000000"},
        {"path": "boot.scr", "address": "0x8fe00000"},
        {"path": "nx-plat.dtimg", "address": "0xa8000000"},
        {"path": "usb-uart.dtbo", "address": "0x8c000000"},
    ]
    ranges = sorted((int(e["address"], 0), int(e["address"], 0) +
                     int(e.get("runtime_size", str((output / e["path"]).stat().st_size)), 0)) for e in images)
    if any(a[1] > b[0] for a, b in zip(ranges, ranges[1:])):
        raise ValueError("RAM upload regions overlap")
    (output / "bundle.json").write_text(json.dumps({"version": 1, "entry": "0xaa000000", "images": images}, indent=2) + "\n")
    sources = [PROBE / "src/main.rs", PROBE / "linker.ld", PROBE / "Cargo.toml", PROBE / "Cargo.lock", Path(__file__),
               ROOT / "drivers/pcie/tegra210/src/controller.rs", ROOT / "drivers/soc/tegra210/src/packet.rs",
               ROOT / "drivers/rtc/max77620/src/xusb_supplies.rs", PROBE / "src/recovery.rs", PROBE / "src/radio.rs",
               ROOT / "drivers/network/brcmfmac/src/chip.rs", ROOT / "drivers/network/brcmfmac/src/pcie.rs",
               ROOT / "drivers/network/brcmfmac/src/msgbuf.rs", PROBE / "src/dma.rs", *firmware_sources]
    sources.append(ROOT / "drivers/network/brcmfmac/src/control.rs")
    sources.extend([ROOT / "drivers/network/brcmfmac/src/wireless.rs", PROBE / "src/scan.rs"])
    hashes = {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sources}
    hashes.update({p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in output.iterdir() if p.is_file()})
    (output / "sha256.json").write_text(json.dumps(hashes, indent=2) + "\n")
    # ELF/Image and build log preserve the result; do not retain another cache.
    shutil.rmtree(output / "build")
    print(f"Prepared RAM probe (initialize PCIe: {initialize}, firmware: {radio}): {output / 'bundle.json'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--initialize-pcie", action="store_true", help="enable board supplies, Wi-Fi GPIOs, PHY and host")
    parser.add_argument("--initialize-radio", action="store_true", help="initialize PCIe and boot the pinned radio firmware; no DMA or AP connection")
    parser.add_argument("--initialize-msgbuf", action="store_true", help="initialize PCIe/firmware and test DMA with GET_VERSION; no AP connection")
    parser.add_argument("--scan", action="store_true", help="initialize the radio and run a passive scan; no AP connection")
    parser.add_argument("--watchdog-test", action="store_true", help="exercise diagnostic timeout back to RCM without PCIe initialization")
    args = parser.parse_args()
    if (args.initialize_pcie or args.initialize_radio or args.initialize_msgbuf or args.scan) and args.watchdog_test:
        parser.error("choose PCIe initialization or the recovery test")
    prepare(args.output.resolve(), args.initialize_pcie, args.watchdog_test, args.initialize_radio, args.initialize_msgbuf, args.scan)

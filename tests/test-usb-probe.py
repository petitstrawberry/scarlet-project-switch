#!/usr/bin/env python3
"""Run the pinned Scarlet platform PHY pre-probe regression in QEMU.

The mock driver performs no Tegra MMIO. This checks driver dispatch and FDT
metadata, not physical USB. Only this test module is linked into the QA kernel.
Temporary build outputs are removed after serial/build logs are preserved.
"""
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
PRODUCTION = ROOT / "projects/aarch64-switch-l4t-console"
RESULT = ROOT / ".cache/usb-probe-qa"
REV = "6fa4a4ac2c4a1b05034057b16f614736a44344b2"
PASS = "USB_PROBE_QA_PASS ordinary=deferred private=probed metadata=preserved"


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    RESULT.mkdir(parents=True, exist_ok=True)
    (RESULT / "result.json").unlink(missing_ok=True)
    smoke = load_module("qemu_smoke", ROOT / "tests/qemu-smoke.py")
    with tempfile.TemporaryDirectory(prefix="usb-probe-", dir=ROOT / ".cache") as temporary:
        work = Path(temporary)
        shutil.copytree(PRODUCTION / "bsp", work / "bsp",
                        ignore=shutil.ignore_patterns("target", "Cargo.lock"))
        (work / "scarlet.toml").write_text(f'''schema_version = 2
[project]
name = "usb-probe-qa"
[bsp]
path = "bsp"
package = "scarlet"
[bsp.kernel]
source = {{ git = "https://github.com/petitstrawberry/Scarlet", rev = "{REV}" }}
features = {{ linux-boot = true, limine = false, hypervisor = false, network = true, user-fpu = true, user-vector = false }}
[modules]
scarlet-usb-probe-qa = {{ path = "{ROOT / 'tests/usb-probe-qa'}", enabled = true }}
''')
        env = os.environ.copy()
        env["CARGO_HOME"] = str(PRODUCTION / ".scarlet/cache/cargo-home")
        # cargo-scarlet sets CARGO_HOME itself. Share that directory explicitly
        # instead of creating another dependency cache in the temporary project.
        cache = work / ".scarlet/cache"
        cache.mkdir(parents=True)
        (cache / "cargo-home").symlink_to(Path(env["CARGO_HOME"]), target_is_directory=True)
        env.pop("CARGO_TARGET_DIR", None)
        env.pop("CARGO_UNSTABLE_BUILD_STD", None)
        env.pop("CARGO_UNSTABLE_BUILD_STD_FEATURES", None)
        with (RESULT / "build.log").open("w") as log:
            subprocess.run(["cargo", "scarlet", "build", "--project", str(work), "--release"],
                           cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
        elf = work / "bsp/target/aarch64-switch-none-elf/release/scarlet"
        image = work / "Image"
        subprocess.run(["llvm-objcopy", "--remove-section=.ksym", "-O", "binary", str(elf), str(image)], check=True)
        package = load_module("package_l4t", PRODUCTION / "tools/package_l4t.py")
        package.validate_image(image.read_bytes(), elf.read_bytes())
        subprocess.run(["aarch64-unknown-linux-gnu-as", "--defsym=ENTER_EL1=0", str(ROOT / "tests/entry.S"),
                        "-o", str(work / "entry.o")], check=True)
        subprocess.run(["llvm-objcopy", "-O", "binary", str(work / "entry.o"), str(work / "entry.bin")], check=True)
        dts = smoke.fixture(0, mode="kernel", framebuffer=False)
        # The fixture parks before initramfs setup. A zero-length initrd range
        # is rejected before the early console, so omit both optional fields.
        dts = "\n".join(line for line in dts.splitlines() if "linux,initrd-" not in line)
        # Real ODIN phandle cells; deliberately register no PhyProvider.
        nodes = '''
    ordinary-phys {
        compatible = "scarlet,usb-qa-ordinary";
        scarlet,usb-host;
        phys = <0x55 0x58>; phy-names = "usb2-0", "usb3-0";
    };
    private-phys {
        compatible = "scarlet,usb-qa-private";
        scarlet,usb-host;
        scarlet,usb-host-phys = <0x55 0x58>;
        phy-names = "usb2-0", "usb3-0";
    };
'''
        dts = dts.rsplit("};", 1)[0] + nodes + "};\n"
        (RESULT / "fixture.dts").write_text(dts)
        dtb = work / "fixture.dtb"
        subprocess.run(["dtc", "-q", "-I", "dts", "-O", "dtb", "-o", str(dtb), str(RESULT / "fixture.dts")], check=True)
        serial = RESULT / "serial.log"
        serial.unlink(missing_ok=True)
        command = ["qemu-system-aarch64", "-machine", "virt,secure=on,virtualization=on,gic-version=2",
                   "-cpu", "cortex-a57", "-m", "3G", "-smp", "4", "-accel", "tcg", "-nodefaults",
                   "-display", "none", "-serial", f"file:{serial}", "-monitor", "none",
                   "-device", f"loader,file={work / 'entry.bin'},addr=0x80000000,cpu-num=0,force-raw=on",
                   "-device", f"loader,file={image},addr=0x80200000,force-raw=on",
                   "-device", f"loader,file={dtb},addr=0x8d000000,force-raw=on"]
        for cpu in range(1, 4):
            command.extend(["-device", f"loader,file={work / 'entry.bin'},addr=0x80000000,cpu-num={cpu},force-raw=on"])
        (RESULT / "command.json").write_text(json.dumps(command, indent=2) + "\n")
        with (RESULT / "qemu.log").open("w") as log:
            proc = subprocess.Popen(command, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 45
                while time.monotonic() < deadline:
                    text = serial.read_text(errors="replace") if serial.exists() else ""
                    if PASS in text:
                        break
                    if proc.poll() is not None:
                        raise RuntimeError((RESULT / "qemu.log").read_text())
                    time.sleep(0.05)
                else:
                    raise TimeoutError(f"missing QA completion: {text[-4000:]}")
                assert "[probe] deferred Standard Devices device: ordinary-phys" in text
                assert "Successfully probed Standard Devices device: private-phys" in text
                assert text.count("USB_PROBE_QA_PRIVATE_METADATA_OK") == 1
                assert "Panic occurred" not in text and "[panic]" not in text
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
        (RESULT / "result.json").write_text(json.dumps({
            "kernel_revision": REV, "cpu": "cortex-a57", "result": "pass",
            "image_sha256": hashlib.sha256(image.read_bytes()).hexdigest(),
            "fixture_dtb_sha256": hashlib.sha256(dtb.read_bytes()).hexdigest(),
            "cases": {"ordinary_phys": "deferred before probe", "private_phys": "probe called once; metadata preserved"},
            "physical_usb_tested": False,
        }, indent=2) + "\n")
    print(f"{PASS}\nLogs: {RESULT}")


if __name__ == "__main__":
    main()

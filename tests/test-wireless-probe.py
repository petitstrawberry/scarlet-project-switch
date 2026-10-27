#!/usr/bin/env python3
"""Boot the actual inventory Image at EL1; verify lossless DT framing in QEMU."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main(output, case="qemu", fault=False):
    smoke = load("smoke", ROOT / "tests/qemu-smoke.py")
    collector = load("collector", ROOT / "scripts/collect-wireless-probe.py")
    inspector = load("inspector", ROOT / "scripts/inspect-wireless-handoff.py")
    if not case or any(c not in "abcdefghijklmnopqrstuvwxyz0123456789-_" for c in case):
        raise ValueError("case must be a simple lowercase name")
    work = output / case
    work.mkdir()
    dts = smoke.fixture(0, framebuffer=False, pci_host="tegra").replace(
        "chosen {", "chosen { scarlet,wireless-probe-fixture;")
    if fault:
        dts = dts.replace("chosen {", "chosen { scarlet,wireless-probe-fault;")
    (work / "fixture.dts").write_text(dts)
    subprocess.run(["dtc", "-I", "dts", "-O", "dtb", "-o", str(work / "fixture.dtb"),
                    str(work / "fixture.dts")], check=True, stderr=subprocess.DEVNULL)
    subprocess.run(["aarch64-unknown-linux-gnu-as", "--defsym=ENTER_EL1=1", str(ROOT / "tests/entry.S"),
                    "-o", str(work / "entry.o")], check=True)
    subprocess.run(["llvm-objcopy", "-O", "binary", str(work / "entry.o"), str(work / "entry.bin")], check=True)
    serial = work / "serial.log"
    command = ["qemu-system-aarch64", "-machine", "virt,secure=on,virtualization=on,gic-version=2",
               "-cpu", "cortex-a57", "-m", "3G", "-smp", "1", "-accel", "tcg", "-nodefaults",
               "-display", "none", "-monitor", "none", "-serial", f"file:{serial}"]
    for filename, address, extra in [(work / "entry.bin", "0x80000000", ",cpu-num=0"),
                                     (output / "Image", "0x80200000", ""),
                                     (work / "fixture.dtb", "0x8d000000", "")]:
        command += ["-device", f"loader,file={filename},addr={address},force-raw=on{extra}"]
    with (work / "qemu.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                text = serial.read_text() if serial.exists() else ""
                if "WIRELESS_PROBE_COMPLETE" in text:
                    break
                time.sleep(0.05)
            recovered = collector.decode(text)
            assert recovered == (work / "fixture.dtb").read_bytes(), "DTB did not round-trip"
            parsed = inspector.nodes(recovered)
            assert inspector.cells(parsed["/pcie@1003000"]["reg"])[1] == 0x01003000
            assert inspector.value(bytes(4)) == [0], "zero cell must not become an empty string list"
            assert "ENTRY el=1 " in text and "TEGRA_MMIO skipped=" in text
            assert "RECOVERY_FIXTURE armed=true duplicate_rejected=true restored=true" in text
            if fault:
                assert "WIRELESS_PROBE_FAULT esr=0x00000000f2000777" in text
                assert "WIRELESS_PROBE_RECURSIVE_FAULT" not in text
            else:
                assert "WIRELESS_PROBE_FAULT" not in text
            assert "REG " not in text and "PCIE_MMIO" not in text
            # Dropped/repeated/truncated serial output must not produce success.
            lines = text.splitlines(keepends=True)
            chunk = next(line for line in lines if line.startswith("DTB "))
            for damaged in [text.replace(chunk, "", 1), text.replace(chunk, chunk * 2, 1),
                            text.replace("WIRELESS_PROBE_COMPLETE", "")]:
                try:
                    collector.decode(damaged)
                except ValueError:
                    pass
                else:
                    raise AssertionError("accepted incomplete/duplicated output")
            (work / "result.json").write_text(json.dumps({"pass": True, "el": 1,
                "dtb_roundtrip": True, "tegra_mmio_skipped": True, "damaged_capture_rejected": True,
                "mmu_off_recovery_state": True, "fault_report_test": fault}, indent=2) + "\n")
            print("WIRELESS_PROBE_QEMU_PASS EL1 DTB-roundtrip MMIO-skip damaged-capture recovery-state")
        finally:
            process.terminate()
            process.wait(timeout=5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--case", default="qemu")
    parser.add_argument("--fault", action="store_true")
    args = parser.parse_args()
    main(args.output.resolve(), args.case, args.fault)

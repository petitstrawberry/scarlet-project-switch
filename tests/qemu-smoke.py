#!/usr/bin/env python3
"""Exercise the actual physical-link Image on an emulated Cortex-A57.

This verifies CPU/entry/framebuffer behavior. It does not emulate Tegra or Hekate.
"""
import argparse
import gzip
import json
import functools
import os
from pathlib import Path
import re
import socket
import subprocess
import struct
import time
import zlib

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "tests/boot-probe"
OUT = ROOT / ".cache/qa"
FB_BASE = 0xb0000000
FB_SIZE = 720 * 1280 * 4

def fixture(initrd_size, mode="probe", framebuffer=True, invalid_stride=False, uart=True, pci_host=None,
            framebuffer_format="a8b8g8r8"):
    fb = ""
    if framebuffer:
        fb = f"""
        framebuffer@b0000000 {{
            compatible = "simple-framebuffer";
            reg = <0 0xb0000000 0 0x384000>;
            width = <720>; height = <1280>;
            stride = <{4 if invalid_stride else 2880}>;
            format = "{framebuffer_format}"; scarlet,rotation = <3>;
        }};"""
    stdout = 'stdout-path = "/pl011@9000000";' if uart else ""
    uart_node = '''pl011@9000000 { compatible = "arm,pl011"; reg = <0 0x09000000 0 0x1000>;
        interrupts = <0 1 4>; clock-frequency = <24000000>; status = "okay";
    };''' if uart else ""
    pci_nodes = ""
    if pci_host in ("tegra", "mixed"):
        # The real ODIN DTB's first region is pads, not generic ECAM.
        pci_nodes += '''pcie@1003000 {
            compatible = "nvidia,tegra210-pcie", "nvidia,tegra124-pcie";
            reg = <0 0x01003000 0 0x800 0 0x01003800 0 0x800 0 0x11fff000 0 0x1000>;
            reg-names = "pads", "afi", "cs"; status = "okay";
            #address-cells = <3>; #size-cells = <2>; ranges;
        };'''
    if pci_host in ("disabled", "mixed", "short"):
        size = 0x800 if pci_host == "short" else 0x100000
        status = "disabled" if pci_host == "disabled" else "okay"
        # QEMU virt's real high ECAM, limited to bus 0. An undersized window
        # must complete without touching a function outside its declared reg.
        pci_nodes += f'''pcie@4010000000 {{
            compatible = "pci-host-ecam-generic";
            reg = <0x40 0x10000000 0 {hex(size)}>;
            #address-cells = <3>; #size-cells = <2>; ranges;
            bus-range = <0 0>; status = "{status}";
        }};'''
    return f"""/dts-v1/;
/memreserve/ 0xb0000000 0x400000;
/ {{
    #address-cells = <2>; #size-cells = <2>;
    compatible = "linux,dummy-virt";
    model = "Scarlet Switch CPU/entry test (QEMU virt)";
    interrupt-parent = <1>;
    memory@80000000 {{ device_type = "memory"; reg = <0 0x80000000 0 0x80000000>; }};
    chosen {{
        #address-cells = <2>; #size-cells = <2>; ranges;
        {stdout}
        bootargs = "init=/init maxcpus=1";
        scarlet,boot-mode = "{mode}";
        linux,initrd-start = <0 0x92000040>;
        linux,initrd-end = <0 {hex(0x92000040 + initrd_size)}>;
        {fb}
    }};
    {uart_node}
    {pci_nodes}
    interrupt-controller@8000000 {{
        compatible = "arm,cortex-a15-gic"; #interrupt-cells = <3>;
        #address-cells = <0>; interrupt-controller; phandle = <1>;
        reg = <0 0x08000000 0 0x10000 0 0x08010000 0 0x10000>;
    }};
    timer {{ compatible = "arm,armv8-timer";
        interrupts = <1 13 4 1 14 4 1 11 4 1 10 4>; }};
    cpus {{ #address-cells = <1>; #size-cells = <0>;
        cpu@0 {{ device_type = "cpu"; compatible = "arm,cortex-a57"; reg = <0>; }};
    }};
}};
"""

class Qmp:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(5)
        self.sock.connect(str(path))
        self.file = self.sock.makefile("rwb")
        json.loads(self.file.readline())
        self.command("qmp_capabilities")

    def command(self, name, arguments=None):
        self.file.write((json.dumps({"execute": name, "arguments": arguments or {}}) + "\n").encode())
        self.file.flush()
        while True:
            response = json.loads(self.file.readline())
            if "return" in response: return response["return"]
            if "error" in response: raise RuntimeError(response["error"])

    def close(self):
        self.file.close()
        self.sock.close()

@functools.cache
def font_lookup():
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    sources = list(cargo_home.glob("registry/src/*/font8x8-0.3.1/src/legacy.rs"))
    assert sources, "build first so the locked font8x8 source is available"
    table = sources[0].read_text().split("pub const BASIC_LEGACY:", 1)[1].split("=", 1)[1].split("];", 1)[0]
    rows = re.findall(r"NOTHING_TO_DISPLAY|\[\s*(?:0x[0-9A-Fa-f]+\s*,?\s*){8}\]", table)
    assert len(rows) == 128, "unexpected locked font table"
    glyphs = [bytes(8) if row == "NOTHING_TO_DISPLAY" else
              bytes(int(value, 16) for value in re.findall(r"0x[0-9A-Fa-f]+", row)) for row in rows]
    return {glyphs[code]: chr(code) for code in range(32, 127)}

def framebuffer_text(data):
    lookup = font_lookup()
    lines = []
    for cell_y in range(720 // 16):
        line = []
        for cell_x in range(1280 // 16):
            glyph = bytearray(8)
            for row in range(8):
                for col in range(8):
                    x, y = cell_x * 16 + col * 2, cell_y * 16 + row * 2
                    offset = ((1280 - 1 - x) * 720 + y) * 4
                    if data[offset:offset + 3] == b"\xff\xff\xff":
                        glyph[row] |= 1 << col
            line.append(lookup.get(bytes(glyph), "?"))
        lines.append("".join(line).rstrip())
    return "\n".join(lines)

def legacy_payload(path, kind, compression, expected_arch=22):
    data = path.read_bytes()
    assert len(data) >= 64, "truncated legacy image"
    magic, header_crc, timestamp, size, load, entry, crc, os_id, arch, image_type, comp, name = struct.unpack_from(">7I4B32s", data)
    assert magic == 0x27051956 and os_id == 5 and arch == expected_arch
    assert image_type == kind and comp == compression, "unexpected legacy image type/compression"
    assert header_crc == zlib.crc32(data[:4] + bytes(4) + data[8:64])
    payload = data[64:]
    assert len(payload) == size and crc == zlib.crc32(payload), "legacy payload size/CRC mismatch"
    if kind == 2:
        assert load == entry == 0x80200000
    return payload

def run(name, el1=False, kernel=False, framebuffer=True, invalid_stride=False, invalid_magic=False, uart=True, pci_host=None):
    case = OUT / name
    case.mkdir(parents=True, exist_ok=True)
    (case / "entry.o").unlink(missing_ok=True)
    subprocess.run(["aarch64-unknown-linux-gnu-as", f"--defsym=ENTER_EL1={int(el1)}", str(ROOT / "tests/entry.S"), "-o", str(case / "entry.o")], check=True)
    subprocess.run(["llvm-objcopy", "-O", "binary", str(case / "entry.o"), str(case / "entry.bin")], check=True)
    boot = PROJECT / ".scarlet/boot"
    image = case / "Image"
    image.write_bytes(gzip.decompress(legacy_payload(boot / "uImage", 2, 1)))
    assert image.read_bytes() == (boot / "Image").read_bytes(), "packaged uImage differs from raw Image"
    initrd = legacy_payload(boot / "initramfs", 3, 0)
    assert initrd.startswith(b"070701"), "bootm must hand raw newc CPIO to Scarlet"
    dts = case / "input.dts"
    dts.write_text(fixture(len(initrd), "kernel" if kernel else "probe", framebuffer, invalid_stride, uart, pci_host))
    dtb = case / "input.dtb"
    subprocess.run(["dtc", "-q", "-I", "dts", "-O", "dtb", "-o", str(dtb), str(dts)], check=True)
    if invalid_magic:
        dtb.write_bytes(b"BAD!" + dtb.read_bytes()[4:])
    qmp_path = Path(f"/tmp/scr-nx-{name}.sock")
    qmp_path.unlink(missing_ok=True)
    serial = case / "serial.log"
    cmd = ["qemu-system-aarch64", "-machine", "virt,secure=on,virtualization=on,gic-version=2",
           # Model the Switch's four-CPU GIC while firmware leaves only CPU0
           # running. A uniprocessor GIC may expose ITARGETSR as RAZ.
           "-cpu", "cortex-a57", "-m", "3G", "-smp", "4", "-accel", "tcg",
           "-nodefaults", "-display", "none", "-serial", f"file:{serial}", "-monitor", "none",
           "-qmp", f"unix:{qmp_path},server=on,wait=off",
           "-device", f"loader,file={case / 'entry.bin'},addr=0x80000000,cpu-num=0,force-raw=on",
           "-device", f"loader,file={image},addr=0x80200000,force-raw=on",
           "-device", f"loader,file={dtb},addr=0x8d000000,force-raw=on",
           "-device", f"loader,file={boot / 'initramfs'},addr=0x92000000,force-raw=on"]
    for cpu in range(1, 4):
        cmd.extend(["-device", f"loader,file={case / 'entry.bin'},addr=0x80000000,cpu-num={cpu},force-raw=on"])
    (case / "command.json").write_text(json.dumps(cmd, indent=2) + "\n")
    with (case / "qemu.log").open("w") as stderr:
        proc = subprocess.Popen(cmd, stderr=stderr, stdout=stderr)
        qmp = None
        try:
            deadline = time.monotonic() + 30
            while not qmp_path.exists():
                if proc.poll() is not None: raise RuntimeError((case / "qemu.log").read_text())
                if time.monotonic() >= deadline: raise TimeoutError("QMP socket")
                time.sleep(0.05)
            qmp = Qmp(qmp_path)
            expected = "SCARLET SWITCH TIMER WAKE REACHED" if kernel else "PROBE COMPLETE"
            dump = case / "framebuffer.bin"
            while time.monotonic() < deadline:
                text = serial.read_text(errors="replace") if serial.exists() else ""
                if kernel and not uart:
                    qmp.command("pmemsave", {"val": FB_BASE, "size": FB_SIZE, "filename": str(dump)})
                    text = framebuffer_text(dump.read_bytes())
                if invalid_magic:
                    if time.monotonic() > deadline - 27: break
                elif expected in text: break
                time.sleep(0.05)
            if uart:
                text = serial.read_text(errors="replace") if serial.exists() else ""
            if invalid_magic:
                assert text == "", f"invalid FDT should park silently: {text[-500:]}"
            else:
                assert expected in text, f"missing {expected}: {text[-2500:]}"
                if uart:
                    assert f"CurrentEL = EL{1 if el1 else 2}" in text
                if kernel:
                    if uart:
                        assert "[linux-boot] temporary identity/HHDM page table active" in text
                    assert "SCARLET SWITCH USERSPACE REACHED" in text
                    if uart:
                        assert "Successfully probed Critical Infrastructure device: interrupt-controller@8000000" in text
                        assert "Init task added to scheduler" in text
                        if pci_host in ("tegra", "disabled"):
                            assert "[PCI] no PCI ECAM found in FDT" in text
                            assert "Scanning PCI bus..." not in text
                            assert "[PCI] ECAM discovered" not in text
                        elif pci_host in ("mixed", "short"):
                            size = "0x800" if pci_host == "short" else "0x100000"
                            assert f"[PCI] ECAM discovered from FDT paddr=0x4010000000 size={size}" in text
                            devices = 0 if pci_host == "short" else 1
                            assert f"PCI scan complete: found {devices} devices" in text
                            if pci_host == "mixed":
                                assert "pci 0000:00:00.0: [1b36:0008]" in text
                            assert "[PCI] ECAM discovered from FDT paddr=0x1003000" not in text
                    assert "[panic]" not in text and "Panic occurred" not in text and "Failed to probe" not in text
                    sleep_checks = [tuple(map(int, values)) for values in re.findall(
                        r"TIMER_CHECK requested_ns=(\d+) elapsed_ns=(\d+) result=(-?\d+)", text)]
                    assert [requested for requested, _, _ in sleep_checks] == [20_000_000, 100_000_000, 1_000_000_000] * 2
                    assert all(result == 0 and requested <= elapsed < requested + 2_000_000_000
                               for requested, elapsed, result in sleep_checks), sleep_checks
            qmp.command("stop")
            qmp.command("pmemsave", {"val": FB_BASE, "size": FB_SIZE, "filename": str(dump)})
            data = dump.read_bytes()
            screen_text = ""
            if not framebuffer or invalid_stride or invalid_magic:
                assert not any(data), "rejected framebuffer must remain untouched"
            else:
                if kernel:
                    screen_text = framebuffer_text(data)
                    (case / "framebuffer.txt").write_text(screen_text + "\n")
                    assert expected in screen_text, "userspace timer wake was not rendered on the framebuffer"
                else:
                    assert data[:4] == bytes.fromhex("101520ff"), "background color or rotation incorrect"
                    assert data.count(bytes.fromhex("f4766fff")) > 1000, "marker text was not drawn"
                # Render the rotated ABGR surface as a portable RGB image.
                rgb = bytearray()
                for y in range(720):
                    for x in range(1280):
                        offset = ((1280 - 1 - x) * 720 + y) * 4
                        rgb.extend(data[offset:offset + 3])
                (case / "marker.ppm").write_bytes(b"P6\n1280 720\n255\n" + rgb)
            return {"case": name, "passed": True, "current_el": 1 if el1 else 2,
                    "serial_log": str(serial.relative_to(ROOT)), "hardware_validated": False,
                    "timer_wake_required": kernel,
                    "timer_wake_observed": "SCARLET SWITCH TIMER WAKE REACHED" in text if kernel else None,
                    "framebuffer_console_required": kernel and framebuffer,
                    "framebuffer_console_observed": expected in screen_text if kernel and framebuffer else None,
                    "uart_present": uart,
                    "pci_host_fixture": pci_host,
                    "pci_scan_observed": "Scanning PCI bus..." in text if pci_host else None,
                    "sleep_checks": [{"requested_ns": requested, "elapsed_ns": elapsed, "result": result}
                                     for requested, elapsed, result in sleep_checks] if kernel else []}
        finally:
            if qmp is not None: qmp.close()
            proc.terminate()
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill(); proc.wait()
            qmp_path.unlink(missing_ok=True)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", action="store_true", help="require kernel, userspace and timed sleep wake at EL1 and EL2")
    parser.add_argument("--timer", action="store_true", help="alias for --kernel; sleep wake is mandatory for kernel checks")
    parser.add_argument("--case", help="run one named case")
    args = parser.parse_args()
    cases = [("probe-el2", {}), ("probe-el1", {"el1": True}),
             ("uart-only", {"framebuffer": False}),
             ("invalid-stride", {"invalid_stride": True}),
             ("invalid-fdt", {"invalid_magic": True})]
    if args.kernel or args.timer:
        cases.extend([("kernel-el2", {"kernel": True, "framebuffer": False}),
                      ("kernel-el1", {"el1": True, "kernel": True, "framebuffer": False}),
                      ("kernel-fb-el2", {"kernel": True}),
                      ("kernel-fb-el1", {"kernel": True, "el1": True}),
                      ("kernel-screen-el2", {"kernel": True, "uart": False}),
                      ("kernel-pci-tegra-el2", {"kernel": True, "pci_host": "tegra"}),
                      ("kernel-pci-disabled-el2", {"kernel": True, "framebuffer": False, "pci_host": "disabled"}),
                      ("kernel-pci-mixed-el2", {"kernel": True, "framebuffer": False, "pci_host": "mixed"}),
                      ("kernel-pci-short-el2", {"kernel": True, "framebuffer": False, "pci_host": "short"})])
    if args.case:
        if args.case == "kernel-timer-el2": args.case = "kernel-el2"
        cases = [(name, options) for name, options in cases if name == args.case]
        if not cases: parser.error("unknown case; kernel entries require --kernel or --timer")
    results = []
    try:
        for name, options in cases:
            try:
                result = run(name, **options)
            except Exception as error:
                results.append({"case": name, "passed": False, "error": str(error), "hardware_validated": False})
                raise
            results.append(result)
            print(f"PASS {name}", flush=True)
    finally:
        OUT.mkdir(parents=True, exist_ok=True)
        (OUT / "results.json").write_text(json.dumps(results, indent=2) + "\n")

if __name__ == "__main__":
    main()

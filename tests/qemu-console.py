#!/usr/bin/env python3
"""Boot the normal console service stack and observe SWS's rotated scanout.

The guest is the real console image. Two test-only stemd services export its
process snapshot and journal through QEMU's ordinary PL011 tty. No test code
draws the framebuffer or substitutes for SWS/ScarletShell.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import struct
import time
import zlib

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-console"
spec = importlib.util.spec_from_file_location("smoke", ROOT / "tests/qemu-smoke.py")
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def cpio_entries(data):
    position = 0
    while True:
        header = data[position:position + 110]
        assert header[:6] == b"070701", "invalid newc archive"
        fields = [int(header[6 + i * 8:14 + i * 8], 16) for i in range(13)]
        size, name_size = fields[6], fields[11]
        name = data[position + 110:position + 110 + name_size - 1].decode()
        start = (position + 110 + name_size + 3) & ~3
        if name == "TRAILER!!!":
            return position
        position = (start + size + 3) & ~3


def cpio_file(name, data, inode, mode=0o100644):
    name = name.encode() + b"\0"
    fields = (inode, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name), 0)
    entry = b"070701" + b"".join(f"{field:08x}".encode() for field in fields) + name
    entry += bytes(-len(entry) % 4)
    entry += data
    return entry + bytes(-len(entry) % 4)


def logical_rgb(data):
    rgb = bytearray()
    for y in range(720):
        for x in range(1280):
            offset = ((1280 - 1 - x) * 720 + y) * 4
            rgb.extend(data[offset:offset + 3])
    return rgb


def run(el1=False, timeout=120, screen_only=False, settle_seconds=20):
    case_name = ("screen-only-" if screen_only else "") + ("el1" if el1 else "el2")
    case = ROOT / ".cache/console-qa" / case_name
    case.mkdir(parents=True, exist_ok=True)
    boot = PROJECT / ".scarlet/l4t/switchroot/scarlet-console"
    image = gzip.decompress(smoke.legacy_payload(boot / "uImage", 2, 1))
    assert image == (boot / "Image").read_bytes()
    (case / "Image").write_bytes(image)
    initrd = smoke.legacy_payload(boot / "initramfs", 3, 0)
    trailer = cpio_entries(initrd)
    services = b'''[service.console-qa-processes]
exec = "/bin/ps -l"
tty = "/dev/tty0"
depends = ["scarlet-desktop"]

[service.console-qa-journal]
exec = "/bin/logctl -f -n all"
tty = "/dev/tty0"
depends = ["scarlet-desktop", "logd"]

[service.console-qa-functional]
exec = "/bin/console-qa"
tty = "/dev/tty0"
depends = ["scarlet-desktop"]
'''
    if not screen_only:
        qa = ROOT / ".cache/console-qa-target/aarch64-unknown-scarlet/release/console-qa"
        assert qa.is_file(), "build the functional guest QA with tests/test-console.sh"
        extra = cpio_file("etc/stemd.d/services/99-console-qa.toml", services, 0x7ffffffe)
        extra += cpio_file("bin/console-qa", qa.read_bytes(), 0x7ffffffd, 0o100755)
        initrd = initrd[:trailer] + extra + initrd[trailer:]
    (case / "initramfs.cpio").write_bytes(initrd)
    dts = smoke.fixture(len(initrd), mode="kernel", pci_host="tegra", uart=not screen_only)
    console = "/dev/null" if screen_only else "/dev/tty0"
    dts = dts.replace('bootargs = "init=/init maxcpus=1";', f'bootargs = "init=/init init.console={console} maxcpus=1";')
    (case / "input.dts").write_text(dts)
    subprocess.run(["dtc", "-q", "-I", "dts", "-O", "dtb", "-o", str(case / "input.dtb"), str(case / "input.dts")], check=True)
    subprocess.run(["aarch64-unknown-linux-gnu-as", f"--defsym=ENTER_EL1={int(el1)}", str(ROOT / "tests/entry.S"), "-o", str(case / "entry.o")], check=True)
    subprocess.run(["llvm-objcopy", "-O", "binary", str(case / "entry.o"), str(case / "entry.bin")], check=True)
    qmp_path = Path(f"/tmp/scr-console-{case_name}.sock")
    qmp_path.unlink(missing_ok=True)
    serial = case / "serial.log"
    cmd = ["qemu-system-aarch64", "-machine", "virt,secure=on,virtualization=on,gic-version=2",
           "-cpu", "cortex-a57", "-m", "3G", "-smp", "4", "-accel", "tcg",
           "-nodefaults", "-display", "none", "-serial", f"file:{serial}", "-monitor", "none",
           "-qmp", f"unix:{qmp_path},server=on,wait=off",
           "-device", f"loader,file={case / 'Image'},addr=0x80200000,force-raw=on",
           "-device", f"loader,file={case / 'input.dtb'},addr=0x8d000000,force-raw=on",
           "-device", f"loader,file={case / 'initramfs.cpio'},addr=0x92000040,force-raw=on"]
    for cpu in range(4):
        cmd.extend(["-device", f"loader,file={case / 'entry.bin'},addr=0x80000000,cpu-num={cpu},force-raw=on"])
    (case / "command.json").write_text(json.dumps(cmd, indent=2) + "\n")
    with (case / "qemu.log").open("w") as stderr:
        process = subprocess.Popen(cmd, stdout=stderr, stderr=stderr)
        qmp = None
        try:
            deadline = time.monotonic() + timeout
            while not qmp_path.exists():
                assert process.poll() is None, (case / "qemu.log").read_text()
                assert time.monotonic() < deadline, "QMP startup timed out"
                time.sleep(0.05)
            qmp = smoke.Qmp(qmp_path)
            match_ratio = None
            if screen_only:
                reference = (ROOT / ".cache/console-qa/el2/desktop.ppm").read_bytes().split(b"\n", 3)[3]
                # The Home grid is stable; omit status clock and controls.
                points = [(y * 1280 + x) * 3 for y in range(120, 600, 4) for x in range(256, 1024, 4)]
            while time.monotonic() < deadline:
                text = serial.read_text(errors="replace") if serial.exists() else ""
                if any(marker in text for marker in ("[panic]", "Panic occurred", "PanicInfo")):
                    # Let the diagnostic finish before collecting its location.
                    time.sleep(2)
                    text = serial.read_text(errors="replace")
                    raise AssertionError(text[-7000:])
                if screen_only:
                    dump = case / "framebuffer.bin"
                    qmp.command("pmemsave", {"val": smoke.FB_BASE, "size": smoke.FB_SIZE, "filename": str(dump)})
                    rgb = logical_rgb(dump.read_bytes())
                    match_ratio = sum(rgb[i:i + 3] == reference[i:i + 3] for i in points) / len(points)
                    if match_ratio > 0.98:
                        break
                elif all(marker in text for marker in ("Compositor ready. Starting main loop...", "[Shell] Initializing Scarlet workspace shell", "CONSOLE_FUNCTIONAL_QA_PASS")):
                    break
                assert process.poll() is None, (case / "qemu.log").read_text()
                time.sleep(0.5 if screen_only else 0.1)
            else:
                raise AssertionError(f"SWS/ScarletShell did not become ready: {text[-6000:]}")
            # Give the real renderer time to commit its first full frame.
            time.sleep(settle_seconds)
            qmp.command("stop")
            dump = case / "framebuffer.bin"
            qmp.command("pmemsave", {"val": smoke.FB_BASE, "size": smoke.FB_SIZE, "filename": str(dump)})
            data = dump.read_bytes()
            rgb = logical_rgb(data)
            (case / "desktop.ppm").write_bytes(b"P6\n1280 720\n255\n" + rgb)
            def chunk(kind, payload):
                return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))
            rows = b"".join(b"\0" + rgb[y * 1280 * 3:(y + 1) * 1280 * 3] for y in range(720))
            png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">2I5B", 1280, 720, 8, 2, 0, 0, 0))
            png += chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
            (case / "desktop.png").write_bytes(png)
            colors = {bytes(rgb[i:i + 3]) for i in range(0, len(rgb), 3)}
            assert len(colors) > 32, "scanout still contains only boot diagnostics"
            text = serial.read_text(errors="replace")
            if not screen_only:
                assert "init: starting /bin/stemd in the default Environment" in text
                assert "Scarlet Window Server (SWS)" in text
                assert "name=Console Controls" in text and "name=Home" in text
                assert "[PCI] no PCI ECAM found in FDT" in text
                assert "CONSOLE_FILE_IO_PASS" in text and "CONSOLE_CATALOG_PASS applications=6" in text
                timers = re.findall(r"CONSOLE_TIMER api=(std|native) requested_ns=(\d+) elapsed_ns=(\d+) result=(-?\d+)", text)
                for api in ("std", "native"):
                    checks = [(int(requested), int(elapsed), int(result)) for kind, requested, elapsed, result in timers if kind == api]
                    assert [requested for requested, _, _ in checks] == [20_000_000, 100_000_000, 1_000_000_000] * 2
                    assert all(result == 0 and requested <= elapsed < requested + 2_000_000_000 for requested, elapsed, result in checks)
            else:
                timers = []
                assert text == "", "screen-only fixture unexpectedly produced UART output"
            assert not any(marker in text for marker in ("[panic]", "Panic occurred", "PanicInfo"))
            assert "Failed to initialize display" not in text
            result = {"passed": True, "current_el": 1 if el1 else 2,
                      "hardware_validated": False, "sws_ready": True,
                      "scarlet_shell_started": True, "logical_surface": [1280, 720],
                      "scanout_unique_colors": len(colors), "image_sha256": hashlib.sha256(image).hexdigest(),
                      "uart_present": not screen_only, "home_reference_match": match_ratio,
                      "sleep_wakes": [{"api": api, "requested_ns": int(requested), "elapsed_ns": int(elapsed), "result": int(result)} for api, requested, elapsed, result in timers],
                      "file_io_observed": not screen_only, "application_catalog_observed": not screen_only,
                      "serial_log": str(serial.relative_to(ROOT)),
                      "framebuffer_image": str((case / "desktop.png").relative_to(ROOT)),
                      "test_overrides": [] if screen_only else ["init.console=/dev/tty0", "journal/process observation services"]}
            (case / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result))
            return result
        finally:
            if qmp is not None:
                qmp.close()
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            qmp_path.unlink(missing_ok=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--el1", action="store_true")
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--screen-only", action="store_true", help="use the unchanged RAMDisk and /dev/null stdio, with no UART or tty")
    parser.add_argument("--settle-seconds", type=int, default=20)
    args = parser.parse_args()
    run(args.el1, args.timeout, args.screen_only, args.settle_seconds)

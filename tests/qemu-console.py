#!/usr/bin/env python3
"""Boot the normal console service stack and observe SWS's rotated scanout.

The guest boots the production initramfs and an isolated copy of the ext2 root.
Test-only stemd services export its process snapshot and journal through
QEMU's ordinary PL011 tty. No test code
draws the framebuffer or substitutes for SWS/ScarletShell.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import struct
import time
import zlib

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
spec = importlib.util.spec_from_file_location("smoke", ROOT / "tests/qemu-smoke.py")
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def file_sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def logical_rgb(data, framebuffer_format):
    # FDT format names describe a packed word. The guest is little-endian.
    channels = {"a8b8g8r8": (0, 1, 2), "a8r8g8b8": (2, 1, 0)}[framebuffer_format]
    rgb = bytearray()
    for y in range(720):
        for x in range(1280):
            offset = ((1280 - 1 - x) * 720 + y) * 4
            rgb.extend(data[offset + channel] for channel in channels)
    return rgb


def boot_framebuffer_format(boot):
    payload = smoke.legacy_payload(boot / "boot.scr", 6, 0, expected_arch=2)
    size, terminator = struct.unpack_from(">II", payload)
    assert terminator == 0 and len(payload) == size + 8, "invalid legacy script payload"
    script = payload[8:]
    assert script == (PROJECT / "bootloader/boot.cmd").read_bytes(), "boot.scr is stale"
    formats = re.findall(rb"^fdt set /chosen/framebuffer@f5a00000 format (\S+)$", script, re.M)
    assert len(formats) == 1, "missing or ambiguous framebuffer format"
    framebuffer_format = formats[0].decode()
    assert framebuffer_format in ("a8b8g8r8", "a8r8g8b8"), "unsupported scanout format"
    return framebuffer_format


def run(el1=False, timeout=120, screen_only=False, settle_seconds=20, input_qa=False, rootfs=None, input_panel_qa=False):
    input_fixture = input_qa or input_panel_qa
    assert not (screen_only and input_fixture), "input QA requires its observation service"
    boot = PROJECT / ".scarlet/l4t/switchroot/scarlet-console"
    framebuffer_format = boot_framebuffer_format(boot)
    case_name = ("screen-only-" if screen_only else "") + ("el1" if el1 else "el2")
    if input_fixture:
        case_name = ("panel-" if input_panel_qa else "input-") + case_name
    case_root = ROOT / ".cache/console-qa" / framebuffer_format
    case = case_root / case_name
    case.mkdir(parents=True, exist_ok=True)
    image = gzip.decompress(smoke.legacy_payload(boot / "uImage", 2, 1))
    assert image == (boot / "Image").read_bytes()
    production_image_sha256 = hashlib.sha256(image).hexdigest()
    if input_fixture:
        image = (ROOT / ".cache/input-qa-project/Image").read_bytes()
    (case / "Image").write_bytes(image)
    initrd = smoke.legacy_payload(boot / "initramfs", 3, 0)
    assert initrd.startswith(b"070701"), "expected production newc initramfs"
    prepared_rootfs = rootfs is not None
    rootfs = rootfs or PROJECT / ".scarlet/images/rootfs-switch-full.ext2"
    rootfs = rootfs.resolve()
    assert rootfs.is_file(), "build the production rootfs with scripts/build-console.sh"
    production_rootfs_sha256 = file_sha256(rootfs)
    root_disk = case / "rootfs.ext2"
    shutil.copyfile(rootfs, root_disk)
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
        additions = [
            ("console-qa-services.toml", "/etc/stemd.d/services/99-console-qa.toml", services, False),
            ("console-qa", "/bin/console-qa", qa.read_bytes(), True),
        ]
        if input_fixture:
            input_elf = ROOT / ".cache/input-qa-target/aarch64-unknown-scarlet/release" / ("input-panel-qa" if input_panel_qa else "input-qa")
            additions += [
                ("input-qa", "/bin/input-qa", input_elf.read_bytes(), True),
                ("input-qa-services.toml", "/etc/stemd.d/services/98-input-qa.toml", b'''[service.input-qa]
exec = "/bin/input-qa"
tty = "/dev/tty0"
depends = ["scarlet-desktop", "console-qa-functional"]
''', False),
            ]
        commands = []
        for name, destination, content, executable in additions:
            (case / name).write_bytes(content)
            commands.append(f"write {name} {destination}")
            if executable:
                commands.append(f"set_inode_field {destination} mode 0100755")
        (case / "qa-files.debugfs").write_text("\n".join(commands) + "\n")
        subprocess.run(["debugfs", "-w", "-f", "qa-files.debugfs", "rootfs.ext2"], cwd=case, check=True)
        # debugfs can report a failed command without a failing exit status.
        for name, destination, content, _ in additions:
            copy = case / ("verified-" + name)
            copy.unlink(missing_ok=True)
            subprocess.run(["debugfs", "-R", f"dump {destination} {copy.name}", "rootfs.ext2"], cwd=case, check=True)
            assert copy.read_bytes() == content, f"failed to install QA file: {destination}"
    (case / "result.json").unlink(missing_ok=True)
    (case / "initramfs.cpio").write_bytes(initrd)
    dts = smoke.fixture(len(initrd), mode="kernel", pci_host="tegra", uart=not screen_only,
                        framebuffer_format=framebuffer_format)
    console = "/dev/null" if screen_only else "/dev/tty0"
    dts = dts.replace('bootargs = "init=/init maxcpus=1";',
                      f'bootargs = "init=/init init.console={console} root=/dev/vblk0 rootfstype=ext2 rootwait maxcpus=1";')
    root_end = dts.rfind("};")
    dts = dts[:root_end] + '''
    virtio_mmio@a000000 {
        compatible = "virtio,mmio";
        reg = <0 0x0a000000 0 0x200>;
        interrupts = <0 16 1>;
        dma-coherent;
    };
''' + dts[root_end:]
    if input_fixture:
        dts = dts.replace('model = "Scarlet Switch CPU/entry test (QEMU virt)";',
                          'model = "Scarlet Switch input test (QEMU virt)";')
        root_end = dts.rfind("};")
        dts = dts[:root_end] + '''
    input-qa-provider {
        phandle = <0x7f000001>;
        #reset-cells = <1>;
        #iommu-cells = <1>;
        #dma-cells = <1>;
    };
    input-qa {
        compatible = "scarlet,input-qa";
        status = "okay";
        resets = <0x7f000001 7>;
        iommus = <0x7f000001 14>;
        dmas = <0x7f000001 9>;
    };
    input-qa-required-reset {
        compatible = "scarlet,input-qa-required-reset";
        resets = <0x7f000001 7>;
    };
    input-qa-required-iommu {
        compatible = "scarlet,input-qa-required-iommu";
        iommus = <0x7f000001 14>;
    };
    input-qa-required-dma {
        compatible = "scarlet,input-qa-required-dma";
        dmas = <0x7f000001 9>;
    };
''' + dts[root_end:]
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
           "-global", "virtio-mmio.force-legacy=false",
           "-drive", f"if=none,id=rootfs,format=raw,file={root_disk},snapshot=on",
           "-device", "virtio-blk-device,drive=rootfs,bus=virtio-mmio-bus.0",
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
            panel_snapshot = False
            if screen_only:
                reference = (case_root / "el2/desktop.ppm").read_bytes().split(b"\n", 3)[3]
                # The Home grid is stable; omit status clock and controls.
                points = [(y * 1280 + x) * 3 for y in range(120, 600, 4) for x in range(256, 1024, 4)]
            while time.monotonic() < deadline:
                text = serial.read_text(errors="replace") if serial.exists() else ""
                if any(marker in text for marker in ("[panic]", "Panic occurred", "PanicInfo", "panicked at")):
                    # Let the diagnostic finish before collecting its location.
                    time.sleep(2)
                    text = serial.read_text(errors="replace")
                    raise AssertionError(text[-7000:])
                if input_panel_qa and not panel_snapshot and "INPUT_PANEL_READY" in text:
                    dump = case / "keyboard-framebuffer.bin"
                    qmp.command("pmemsave", {"val": smoke.FB_BASE, "size": smoke.FB_SIZE, "filename": str(dump)})
                    (case / "keyboard.ppm").write_bytes(b"P6\n1280 720\n255\n" + logical_rgb(dump.read_bytes(), framebuffer_format))
                    panel_snapshot = True
                if screen_only:
                    dump = case / "framebuffer.bin"
                    qmp.command("pmemsave", {"val": smoke.FB_BASE, "size": smoke.FB_SIZE, "filename": str(dump)})
                    rgb = logical_rgb(dump.read_bytes(), framebuffer_format)
                    match_ratio = sum(rgb[i:i + 3] == reference[i:i + 3] for i in points) / len(points)
                    if match_ratio > 0.98:
                        break
                elif all(marker in text for marker in ("Compositor ready. Starting main loop...", "[Shell] Initializing Scarlet workspace shell", "CONSOLE_FUNCTIONAL_QA_PASS")) and (not input_fixture or ("INPUT_PANEL_QA_PASS" if input_panel_qa else "INPUT_QA_PASS") in text):
                    break
                assert process.poll() is None, (case / "qemu.log").read_text()
                time.sleep(0.5 if screen_only else 0.1)
            else:
                raise AssertionError(f"SWS/ScarletShell did not become ready: {text[-6000:]}")
            # Give the real renderer time to commit its first full frame.
            time.sleep(settle_seconds)
            if not screen_only:
                # The full catalog decodes more cover artwork than the small
                # console image did. An empty Home surface is not a ready UI.
                while time.monotonic() < deadline:
                    dump = case / "framebuffer.bin"
                    qmp.command("pmemsave", {"val": smoke.FB_BASE, "size": smoke.FB_SIZE, "filename": str(dump)})
                    rgb = logical_rgb(dump.read_bytes(), framebuffer_format)
                    if len({bytes(rgb[i:i + 3]) for i in range(0, len(rgb), 3)}) > (64 if input_panel_qa else 1024):
                        break
                    time.sleep(1)
                else:
                    raise AssertionError("Home application artwork did not become visible")
            qmp.command("stop")
            dump = case / "framebuffer.bin"
            qmp.command("pmemsave", {"val": smoke.FB_BASE, "size": smoke.FB_SIZE, "filename": str(dump)})
            data = dump.read_bytes()
            rgb = logical_rgb(data, framebuffer_format)
            (case / "desktop.ppm").write_bytes(b"P6\n1280 720\n255\n" + rgb)
            def chunk(kind, payload):
                return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))
            rows = b"".join(b"\0" + rgb[y * 1280 * 3:(y + 1) * 1280 * 3] for y in range(720))
            png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">2I5B", 1280, 720, 8, 2, 0, 0, 0))
            png += chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
            (case / "desktop.png").write_bytes(png)
            colors = {bytes(rgb[i:i + 3]) for i in range(0, len(rgb), 3)}
            assert len(colors) > (64 if input_panel_qa else 1024), "scanout is missing application content"
            text = serial.read_text(errors="replace")
            if not screen_only:
                assert "init: starting /bin/stemd in the default Environment" in text
                assert "Scarlet Window Server (SWS)" in text
                assert "name=Console Controls" in text and "name=Home" in text
                assert "[PCI] no PCI ECAM found in FDT" in text
                assert "CONSOLE_FILE_IO_PASS" in text
                catalog = re.search(r"CONSOLE_CATALOG_PASS applications=(\d+)", text)
                # The guest checks all six console applications by name; the
                # full root also supplies upstream desktop applications.
                assert catalog and int(catalog[1]) >= 6
                if input_panel_qa:
                    assert "INPUT_PANEL_QA_PASS" in text, "software keyboard scenario did not finish"
                if input_qa:
                    for marker in ("INPUT_QA_FIXTURE_READY", "INPUT_GAMEPAD_SWS_PASS", "INPUT_SCARLET_UI_PASS", "INPUT_TOUCH_SCROLL_PASS", "INPUT_CONSOLE_SHELL_PASS", "INPUT_QA_PASS"):
                        assert marker in text, f"missing input observation: {marker}"
                    for provider in ("reset", "iommu", "dma"):
                        name = f"input-qa-required-{provider}"
                        assert f"deferred Standard Devices device: {name}" in text, f"default dependency hook did not defer {name}"
                timers = re.findall(r"CONSOLE_TIMER api=(std|native) requested_ns=(\d+) elapsed_ns=(\d+) result=(-?\d+)", text)
                for api in ("std", "native"):
                    checks = [(int(requested), int(elapsed), int(result)) for kind, requested, elapsed, result in timers if kind == api]
                    assert [requested for requested, _, _ in checks] == [20_000_000, 100_000_000, 1_000_000_000] * 2
                    assert all(result == 0 and requested <= elapsed < requested + 2_000_000_000 for requested, elapsed, result in checks)
            else:
                timers = []
                assert text == "", "screen-only fixture unexpectedly produced UART output"
            assert not any(marker in text for marker in ("[panic]", "Panic occurred", "PanicInfo", "panicked at"))
            assert "Failed to initialize display" not in text
            if not screen_only:
                assert "init: rootwait: /dev/vblk0 mounted" in text, "production init did not mount the ext2 root"
            result = {"passed": True, "current_el": 1 if el1 else 2,
                      "hardware_validated": False, "sws_ready": True,
                      "scarlet_shell_started": True, "logical_surface": [1280, 720],
                      "scanout_unique_colors": len(colors), "scanout_format": framebuffer_format,
                      "boot_script_sha256": hashlib.sha256((boot / "boot.scr").read_bytes()).hexdigest(),
                      "image_sha256": hashlib.sha256(image).hexdigest(),
                      "production_image_sha256": production_image_sha256,
                      "production_initramfs_sha256": hashlib.sha256(initrd).hexdigest(),
                      "production_rootfs_sha256": production_rootfs_sha256,
                      "rootfs_image": str(rootfs),
                      "root_device": "/dev/vblk0",
                      "software_keyboard_observed": input_panel_qa,
                      "native_gamepad_delivery_observed": input_qa,
                      "platform_probe_options_observed": input_qa,
                      "scarlet_ui_gamepad_callback_observed": input_qa,
                      "scarlet_ui_touch_scroll_observed": input_qa,
                      "console_shell_gamepad_navigation_observed": input_qa,
                      "uart_present": not screen_only, "home_reference_match": match_ratio,
                      "sleep_wakes": [{"api": api, "requested_ns": int(requested), "elapsed_ns": int(elapsed), "result": int(result)} for api, requested, elapsed, result in timers],
                      "file_io_observed": not screen_only, "application_catalog_observed": not screen_only,
                      "serial_log": str(serial.relative_to(ROOT)),
                      "framebuffer_image": str((case / "desktop.png").relative_to(ROOT)),
                      "test_overrides": [] if screen_only else ["init.console=/dev/tty0", "journal/process observation services"]}
            if input_fixture:
                result["test_overrides"] += ["isolated QA kernel", "native input injection fixture", "input QA service"]
            if prepared_rootfs:
                result["test_overrides"].append("prepared SD rootfs")
            (case / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result))
            return result
        except BaseException:
            if qmp is not None:
                try:
                    qmp.command("stop")
                    dump = case / "failure-framebuffer.bin"
                    qmp.command("pmemsave", {"val": smoke.FB_BASE, "size": smoke.FB_SIZE, "filename": str(dump)})
                    (case / "failure.ppm").write_bytes(b"P6\n1280 720\n255\n" + logical_rgb(dump.read_bytes(), framebuffer_format))
                except Exception:
                    pass
            raise
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
    parser.add_argument("--screen-only", action="store_true", help="use the production images and /dev/null stdio, with no UART or tty")
    parser.add_argument("--settle-seconds", type=int, default=20)
    parser.add_argument("--input-qa", action="store_true", help="use the isolated input fixture built by tests/test-input.py")
    parser.add_argument("--rootfs", type=Path, help="test a prepared ext2 image before SD installation")
    args = parser.parse_args()
    run(args.el1, args.timeout, args.screen_only, args.settle_seconds, args.input_qa, args.rootfs)

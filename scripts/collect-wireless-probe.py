#!/usr/bin/env python3
"""Deploy a prepared RAM inventory probe and preserve its complete DT handoff."""
import argparse
import errno
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import select
import subprocess
import termios
import time
import tty


def decode(text):
    start = re.search(r"WIRELESS_PROBE_BEGIN version=1 dtb=(0x[0-9a-f]+) bytes=(\d+)\r?\n", text)
    if start is None or "WIRELESS_PROBE_COMPLETE" not in text[start.end():]:
        raise ValueError("probe did not complete")
    body = text[start.end():text.index("WIRELESS_PROBE_COMPLETE", start.end())]
    result = bytearray()
    for offset, data in re.findall(r"^DTB ([0-9a-f]{8}) ([0-9a-f]+)\r?$", body, re.M):
        if int(offset, 16) != len(result) or len(data) % 2:
            raise ValueError("DTB output has a gap, duplicate or malformed chunk")
        result.extend(bytes.fromhex(data))
    if len(result) != int(start[2]) or result[:4] != bytes.fromhex("d00dfeed"):
        raise ValueError("DTB output is incomplete or has the wrong magic")
    if int.from_bytes(result[4:8], "big") != len(result):
        raise ValueError("DTB total size differs from the transferred size")
    return bytes(result)


def collect(bundle, console, ctl, timeout):
    output = bundle.parent
    if (output / "serial.log").exists():
        raise ValueError("serial.log already exists; preserve it and use a fresh prepared bundle")
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be finite and positive")
    status = subprocess.check_output([ctl, "status"], text=True, timeout=10)
    (output / "status-before.txt").write_text(status)
    if any(line + "\n" not in status for line in ("state=preboot", "fallback=disabled", "loader=idle")):
        raise ValueError("expected an idle Switchvisor development entry with fallback disabled")
    fd = os.open(console, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    original = termios.tcgetattr(fd)
    result = {"hardware_run": True, "probe_complete": False, "pcie_initialized": False,
              "radio_initialized": False}
    try:
        fcntl.ioctl(fd, termios.TIOCEXCL)
        tty.setraw(fd)
        with (output / "deploy.log").open("w") as log:
            subprocess.run([ctl, "deploy", str(bundle)], stdout=log, stderr=subprocess.STDOUT, check=True, timeout=45)
        captured = bytearray()
        deadline = time.monotonic() + timeout
        with (output / "serial.log").open("wb", buffering=0) as log:
            while time.monotonic() < deadline:
                if select.select([fd], [], [], min(0.2, max(0, deadline - time.monotonic())))[0]:
                    try:
                        data = os.read(fd, 65536)
                    except OSError as error:
                        if error.errno not in (errno.EIO, errno.ENXIO, errno.ENODEV):
                            raise
                        break
                    if not data:
                        break
                    captured.extend(data)
                    log.write(data)
                    if b"WIRELESS_PROBE_COMPLETE" in captured:
                        break
        text = captured.decode("ascii", errors="replace")
        dtb = decode(text)
        result["probe_complete"] = True
        (output / "handoff.dtb").write_bytes(dtb)
        subprocess.run(["dtc", "-I", "dtb", "-O", "dts", "-o", str(output / "handoff.dts"),
                        str(output / "handoff.dtb")], stderr=subprocess.DEVNULL, check=True)
        after = subprocess.check_output([ctl, "status"], text=True, timeout=10)
        (output / "status-after.txt").write_text(after)
        endpoint = re.search(r"PCIE_RESULT Ok\((0x[0-9a-f]+|[0-9]+)\)", text)
        faults = re.findall(r"^WIRELESS_PROBE_(?:RECURSIVE_)?FAULT.*$", text, re.M)
        recovery_failed = "RECOVERY_TEST_FAILED" in text
        result.update({"dtb_size": len(dtb),
                  "dtb_sha256": hashlib.sha256(dtb).hexdigest(),
                  "registers": re.findall(r"^REG (.+)$", text, re.M),
                  "pcie_initialized": endpoint is not None,
                  "pci_endpoint_id": hex(int(endpoint[1], 0)) if endpoint else None,
                  "pcie_result": re.findall(r"^PCIE_RESULT (.+)$", text, re.M),
                  "faults": faults, "watchdog_test_failed": recovery_failed,
                  "guest_state": "diagnostic idle with virtual UART polling"})
        if faults or recovery_failed:
            raise RuntimeError("probe reported a fault or failed watchdog recovery; see serial.log")
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2))
    except Exception as error:
        result["error"] = str(error)
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        raise
    finally:
        # A diagnostic watchdog intentionally disconnects this tty. Preserve
        # the original capture/decode failure instead of masking it in cleanup.
        try:
            try:
                termios.tcsetattr(fd, termios.TCSANOW, original)
            except (OSError, termios.error):
                pass
            try:
                fcntl.ioctl(fd, termios.TIOCNXCL)
            except OSError:
                pass
        finally:
            os.close(fd)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("--console", default="/dev/cu.usbmodemSWV00011")
    parser.add_argument("--ctl", default="switchvisorctl")
    parser.add_argument("--timeout", type=float, default=60)
    args = parser.parse_args()
    collect(args.bundle.resolve(), args.console, args.ctl, args.timeout)

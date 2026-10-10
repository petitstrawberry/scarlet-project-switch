#!/usr/bin/env python3
"""Boot Switchvisor from RCM, deploy the guest bundle, and open its console."""
import argparse
import datetime
import math
import os
from pathlib import Path
import platform
import shutil
import subprocess
import time


ROOT = Path(__file__).resolve().parents[1]
STATE = ROOT / "projects/aarch64-switch-l4t-console/.scarlet"
CONSOLE = Path("/dev/cu.usbmodemSWV00011")


def positive_seconds(value):
    seconds = float(value)
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("timeout must be a finite positive number")
    return seconds


def wait_console(port, timeout):
    deadline = time.monotonic() + timeout
    while not port.is_char_device():
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ValueError(f"console did not appear within {timeout:g}s: {port}; "
                             "check the SD SCR-SWV entry, USB cable, or --console")
        time.sleep(min(0.2, remaining))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("payload", type=Path, help="Hekate payload .bin; put the Switch in RCM first")
    parser.add_argument("--bundle", type=Path, default=STATE / "switchvisor/bundle.json")
    parser.add_argument("--console", type=Path, default=CONSOLE, help="guest CDC port (not the control port)")
    parser.add_argument("--timeout", type=positive_seconds, default=60, help="seconds to wait for the console after injection (default: 60)")
    parser.add_argument("--log", type=Path, help="minicom capture file (default: .scarlet/logs/switchvisor-TIMESTAMP.log)")
    args = parser.parse_args(argv)
    if platform.system() != "Darwin":
        raise ValueError("this RCM launcher requires macOS NXBoot")
    payload = args.payload.resolve(strict=True)
    bundle = args.bundle.resolve(strict=True)
    if not payload.is_file() or not bundle.is_file():
        raise ValueError("payload and bundle must be regular files")
    tools = {name: shutil.which(name) for name in ("nxboot", "switchvisorctl", "minicom")}
    missing = [name for name, path in tools.items() if path is None]
    if missing:
        raise ValueError("missing " + ", ".join(missing) + "; run nix develop --accept-flake-config first")
    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S-%f")
    log = (args.log or STATE / "logs" / f"switchvisor-{stamp}.log").resolve()
    log.parent.mkdir(parents=True, exist_ok=True)
    # Check capture-file access before starting the device.
    with log.open("ab"):
        pass
    console = args.console.absolute()
    print("Starting Hekate entry SCR-SWV (waiting for RCM if necessary)...", flush=True)
    subprocess.run([tools["nxboot"], "--hekate", "id", "SCR-SWV", str(payload)], check=True)
    print(f"Waiting for guest console: {console}", flush=True)
    wait_console(console, args.timeout)
    print(f"Deploying guest bundle: {bundle}", flush=True)
    subprocess.run([tools["switchvisorctl"], "deploy", str(bundle)], check=True)
    # Recheck the port in case USB enumerated again during deployment.
    wait_console(console, args.timeout)
    print(f"Opening console; capture: {log}\nExit minicom with Ctrl-A, then X.", flush=True)
    os.execv(tools["minicom"], [tools["minicom"], "-D", str(console), "-b", "115200", "-8", "-C", str(log)])


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        raise SystemExit(130)
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.returncode)
    except (OSError, ValueError) as error:
        raise SystemExit(str(error))

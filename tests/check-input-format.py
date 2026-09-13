#!/usr/bin/env python3
"""Check Rust formatting for the coordinated input changes, without rewriting."""
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SCARLET = ROOT.parent / "Scarlet"
UI = ROOT.parent / "scarlet-ui"


def main():
    files = list((ROOT / "drivers").rglob("*.rs"))
    for crate in ("input-host", "input-fixture", "input-qa"):
        files.extend((ROOT / "tests" / crate / "src").rglob("*.rs"))
    files.extend(SCARLET / path for path in (
        "kernel/src/device/mod.rs",
        "kernel/src/device/platform/mod.rs",
        "kernel/src/device/manager.rs",
        "kernel/src/device/input/event_device.rs",
        "user/lib/scarlet-os/src/input.rs",
        "user/lib/sws-client/src/connection.rs",
        "user/lib/sws-client/src/event.rs",
        "user/lib/sws-protocol/src/lib.rs",
        "user/lib/sws-protocol/src/gamepad.rs",
        "user/std-bin/src/sws/compositor.rs",
        "user/std-bin/src/sws/input.rs",
        "user/std-bin/src/sws/ipc.rs",
        "user/std-bin/src/sws/key_repeat.rs",
        "user/std-bin/src/sws/window.rs",
        "user/std-bin/src/sws/gamepad.rs",
    ))
    files.extend(UI / path for path in (
        "crates/scarlet-ui-core/src/application.rs",
        "crates/scarlet-ui-core/src/platform/mod.rs",
        "crates/scarlet-ui-core/src/lib.rs",
        "crates/scarlet-ui-core/src/event/mod.rs",
        "crates/scarlet-ui-core/src/event/gamepad.rs",
        "crates/scarlet-ui-core/src/event/dispatcher.rs",
        "crates/scarlet-ui-core/src/input_environment.rs",
        "crates/scarlet-ui-platform-sws/src/lib.rs",
    ))
    subprocess.run(["rustfmt", "--check", "--edition", "2024", "--config",
                    "skip_children=true", *(str(path) for path in sorted(files))],
                   check=True)
    print(f"INPUT_FORMAT_PASS files={len(files)}")


if __name__ == "__main__":
    main()

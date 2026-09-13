#!/usr/bin/env python3
"""Reuse Scarlet's normal base, CLI and desktop layers for a RAM-only console."""
import hashlib
import json
from pathlib import Path
import shlex
import shutil
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-console"
SCARLET = ROOT.parent / "Scarlet"
UI = ROOT.parent / "scarlet-ui"
DESKTOP_BINS = {
    "scarlet-desktop", "desktop-settings", "files", "notepad", "scarlet-shell",
    "terminal", "sws", "sas", "sasctl", "settings", "task-manager", "clock",
}


def value(item):
    if isinstance(item, bool):
        return str(item).lower()
    if isinstance(item, str):
        return json.dumps(item, ensure_ascii=False)
    if isinstance(item, list):
        return "[" + ", ".join(map(value, item)) + "]"
    if isinstance(item, dict):
        return "{ " + ", ".join(f"{value(k)} = {value(v)}" for k, v in item.items()) + " }"
    return str(item)


def flatten(path):
    for original in tomllib.loads(path.read_text())["layers"]:
        layer = dict(original)
        if layer["kind"] == "bundle":
            yield from flatten((path.parent / layer["path"]).resolve())
            continue
        if path.parent.name == "desktop":
            if layer["kind"] == "cargo" and layer.get("bin") not in DESKTOP_BINS:
                continue
            if layer["kind"] == "script":
                continue
        source = layer.get("source")
        if isinstance(source, str):
            layer["source"] = str((path.parent / source).resolve())
        if layer["kind"] == "cargo" and layer["source"] == str(SCARLET / "user/std-bin"):
            # Rebuild std without LSE for the A57. Optional AAC's dependency
            # named `std` conflicts with Cargo's build-std injected std crate.
            layer["default-features"] = False
        yield layer


def main():
    cache = PROJECT / ".scarlet/cache"
    cargo_home = cache / "cargo-home"
    cargo_home.mkdir(parents=True, exist_ok=True)
    config = ['[patch."https://github.com/petitstrawberry/scarlet-ui"]']
    for name in ("scarlet-ui", "scarlet-ui-core", "scarlet-ui-macros",
                 "scarlet-ui-platform-sws", "scarlet-ui-platform-winit",
                 "scarlet-ui-renderer-wgpu", "scarlet-ui-renderer-sgfx",
                 "scarlet-ui-icons-tabler"):
        path = UI / "crates" / name
        if not (path / "Cargo.toml").is_file():
            raise SystemExit(f"missing matching UI checkout: {path}")
        config.append(f"{name} = {{ path = {value(str(path))} }}")
    config.append('\n[patch."https://github.com/petitstrawberry/Scarlet"]')
    for name in ("scarlet-abi", "scarlet-os", "scarlet-rt", "scarlet-std",
                 "scarlet-sys", "gpu-raw", "sws-client", "sws-protocol", "sws-remote-protocol"):
        path = SCARLET / "user/lib" / ("std" if name == "scarlet-std" else name)
        config.append(f"{name} = {{ path = {value(str(path))} }}")
    config.extend(['\n[target.aarch64-unknown-scarlet]',
                   'rustflags = ["-C", "target-cpu=cortex-a57", "-C", "target-feature=-lse", "--cfg", "getrandom_backend=\\\"custom\\\""]'])
    (cargo_home / "config.toml").write_text("\n".join(config) + "\n")
    for name in ("registry", "git"):
        link = cargo_home / name
        shared = Path.home() / ".cargo" / name
        if not link.exists() and shared.exists():
            link.symlink_to(shared, target_is_directory=True)
    layers = list(flatten(SCARLET / "bundles/desktop/bundle.toml"))
    # Keep the original desktop assets and configuration, including the
    # resident Files service. Catalog entries and optional services must match
    # the applications actually installed by this first RAM-only image.
    desktop_source = SCARLET / "bundles/desktop/fs"
    desktop_fs = PROJECT / ".scarlet/desktop-fs"
    if desktop_fs.exists():
        shutil.rmtree(desktop_fs)
    shutil.copytree(desktop_source, desktop_fs)
    installed = {layer["to"] for layer in layers if layer["kind"] == "cargo"}
    for entry in (desktop_fs / "etc/stemd.d/apps").glob("*.desktop"):
        commands = [line.split("=", 1)[1] for line in entry.read_text().splitlines() if line.startswith("Exec=")]
        if commands and shlex.split(commands[0])[0] not in installed:
            entry.unlink()
    for entry in (desktop_fs / "etc/stemd.d/services").glob("*.toml"):
        # stemd accepts legacy order = 01 syntax, which strict TOML rejects.
        commands = [shlex.split(line.split("=", 1)[1].strip())[0]
                    for line in entry.read_text().splitlines()
                    if line.split("=", 1)[0].strip() == "exec"]
        if any(shlex.split(command)[0] not in installed for command in commands):
            entry.unlink()
    for layer in layers:
        if layer["kind"] == "copy" and layer["source"] == str(desktop_source):
            layer["source"] = str(desktop_fs)
    text = "\n\n".join("[[layers]]\n" + "\n".join(f"{key} = {value(item)}" for key, item in layer.items()) for layer in layers)
    (PROJECT / ".scarlet/console-bundle.toml").write_text(text + "\n")
    pins = json.loads((PROJECT / "bootstack.json").read_text())["files"]
    stack = PROJECT / ".scarlet/bootstack"
    stack.mkdir(parents=True, exist_ok=True)
    for name, expected in pins.items():
        source = ROOT / "projects/aarch64-switch-l4t/.scarlet/bootstack" / name
        if not source.is_file() or hashlib.sha256(source.read_bytes()).hexdigest() != expected:
            raise SystemExit("import the pinned Noble bootstack with scripts/prepare-bootstack.py first")
        shutil.copyfile(source, stack / name)
    print(f"Prepared {len(layers)} normal distribution layers and A57 Cargo configuration")


if __name__ == "__main__":
    main()

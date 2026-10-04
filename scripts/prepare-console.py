#!/usr/bin/env python3
"""Prepare console bundles from published pins or a selected local Scarlet."""
import hashlib
import json
import argparse
from pathlib import Path
import subprocess
import sys
import tomllib
from project_sources import pins, prepare

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"


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


def full_layers(path, sources, local_roots=None):
    """Retain the complete upstream distribution with this project's source pins."""
    for original in tomllib.loads(path.read_text())["layers"]:
        layer = dict(original)
        if layer["kind"] == "bundle":
            if "path" in layer:
                nested = path.parent / layer["path"]
            else:
                source = layer["source"]
                if isinstance(source, dict):
                    checkout = sources[source["git"].removesuffix(".git")]
                else:
                    checkout = path.parent / source
                nested = checkout / layer.get("subdir", "") / layer.get("bundle", "bundle.toml")
            yield from full_layers(nested.resolve(), sources, local_roots)
            continue
        source = layer.get("source")
        if isinstance(source, str):
            resolved = (path.parent / source).resolve()
            for upstream_root, local_root in (local_roots or {}).items():
                if resolved.is_relative_to(upstream_root):
                    resolved = local_root / resolved.relative_to(upstream_root)
                    break
            layer["source"] = str(resolved)
        elif isinstance(source, dict) and source.get("git", "").removesuffix(".git") in sources:
            layer["source"] = str(sources[source["git"].removesuffix(".git")])
        yield layer


def development_layers(bundles, sources, local_roots=None):
    """Add only the compositor and GPU diagnostics to the base/CLI image."""
    programs = {
        "sws", "sas", "scarlet-desktop", "task-manager",
        "sgfx-probe", "sgfx-cube", "sgfx-texture",
    }
    selected = [layer for layer in full_layers(
        bundles / "desktop/bundle.toml", sources, local_roots
    ) if layer.get("kind") == "cargo" and layer.get("bin") in programs]
    missing = programs - {layer["bin"] for layer in selected}
    if missing:
        raise ValueError(f"missing development programs: {', '.join(sorted(missing))}")
    yield from selected
    for name in ("fonts", "cursors"):
        yield {"kind": "copy", "source": str(bundles / "desktop/fs/share" / name),
               "to": f"/share/{name}"}
    yield {"kind": "copy", "source": str(PROJECT / "rootfs"), "to": "/"}
    yield {"kind": "copy", "source": str(PROJECT / "initramfs-dev"), "to": "/"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--published", action="store_true",
                      help="select published bundles and clear the saved local selection")
    mode.add_argument("--local-bundles", type=Path, metavar="SCARLET",
                      help="use this Scarlet checkout's bundles; remember the selection locally")
    parser.add_argument("--initramfs-dev", action="store_true",
                        help="include the small GPU desktop and USB network config in initramfs")
    args = parser.parse_args()
    if (PROJECT / "scarlet.local.toml").exists():
        parser.error("remove the project's scarlet.local.toml override; use upstream commit pins")
    checkouts = prepare()
    selection = PROJECT / ".scarlet/bundle-source.local"
    local = args.local_bundles
    if not args.published and local is None and selection.is_file():
        local = Path(selection.read_text().strip())
    local_roots = {}
    if local is not None:
        local = local.resolve(strict=True)
        for bundle in ("base", "cli-utils", "full"):
            if not (local / "bundles" / bundle / "bundle.toml").is_file():
                parser.error(f"missing local Scarlet bundle: {local / 'bundles' / bundle}")
        local_roots[checkouts["scarlet-distribution"]] = local
        checkouts["scarlet-distribution"] = local
    sgfx = checkouts["sgfx"]
    sgfx_manifest = tomllib.loads((sgfx / "crates/sgfx/Cargo.toml").read_text())
    if "backend-dynamic" not in sgfx_manifest.get("features", {}).get("default", []):
        raise SystemExit(
            f"SGFX checkout {sgfx} does not enable generic dynamic driver discovery; "
            "select a compatible upstream revision"
        )
    subprocess.run([sys.executable, str(ROOT / "scripts/verify-maxwell-shaders.py")], check=True)
    subprocess.run([sys.executable, str(ROOT / "scripts/prepare-gm20b-firmware.py")], check=True)
    # The client linker needs this audited shared input before the SDK builds
    # application layers. The bundle subsequently installs the same driver.
    subprocess.run([sys.executable, str(ROOT / "scripts/build-sgfx-maxwell.py"),
                    "--project", str(PROJECT)], check=True)
    # Native runtime/core pins preserve crate identity in applications.
    # Bundle sources use the distribution pin instead of the native API pin.
    sources = {pin["git"].removesuffix(".git"): checkouts[name] for name, pin in pins().items()
               if name not in ("scarlet", "scarlet-native", "sgfx-core")}
    bundles = checkouts["scarlet-distribution"] / "bundles"
    for name, inputs in {
        "initramfs": [bundles / "base/bundle.toml", bundles / "cli-utils/bundle.toml",
                      PROJECT / "bundles/sgfx-maxwell.toml"],
        "full": [bundles / "full/bundle.toml", PROJECT / "bundles/nvdec-player.toml",
                 PROJECT / "bundles/sgfx-maxwell.toml"],
    }.items():
        # Canonical paths avoid treating a source symlink and its destination
        # as two separate Cargo packages in the same dependency graph.
        layers = [layer for path in inputs for layer in full_layers(path, sources, local_roots)]
        if name == "initramfs" and args.initramfs_dev:
            layers.extend(development_layers(bundles, sources, local_roots))
        text = "\n\n".join("[[layers]]\n" + "\n".join(f"{key} = {value(item)}" for key, item in layer.items()) for layer in layers)
        (PROJECT / f".scarlet/{name}-bundle.toml").write_text(text + "\n")
    (PROJECT / ".scarlet/initramfs-profile").write_text(
        "initramfs-dev\n" if args.initramfs_dev else "sd-root\n")
    cache = PROJECT / ".scarlet/cache"
    cargo_home = cache / "cargo-home"
    cargo_home.mkdir(parents=True, exist_ok=True)
    # Remove only the config generated by the previous A57/build-std flow.
    legacy_config = cargo_home / "config.toml"
    if legacy_config.is_file():
        contents = legacy_config.read_text()
        if ("target-cpu=cortex-a57" in contents
                and "target-feature=-lse" in contents
                and '[patch."https://github.com/petitstrawberry/scarlet-ui"]' in contents):
            legacy_config.unlink()
    for name in ("registry", "git"):
        link = cargo_home / name
        shared = Path.home() / ".cargo" / name
        if not link.exists() and shared.exists():
            link.symlink_to(shared, target_is_directory=True)
    bootstack_pins = json.loads((PROJECT / "bootstack.json").read_text())["files"]
    stack = PROJECT / ".scarlet/bootstack"
    for name, expected in bootstack_pins.items():
        source = stack / name
        if not source.is_file() or hashlib.sha256(source.read_bytes()).hexdigest() != expected:
            raise SystemExit("import the pinned Noble bootstack with scripts/prepare-bootstack.py first")
    if local is not None:
        selection.write_text(str(local) + "\n")
    elif args.published:
        selection.unlink(missing_ok=True)
    if args.initramfs_dev:
        print(f"Prepared RAM development bundle (base + CLI + GPU desktop) from {bundles}")
    else:
        print(f"Prepared base + CLI initramfs and full SD rootfs from {bundles}")


if __name__ == "__main__":
    main()

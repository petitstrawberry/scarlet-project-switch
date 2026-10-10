#!/usr/bin/env python3
"""Build and package the board USB patch without changing project Git pins.

Uses the existing offline Cargo cache and six unchanged .scarlet/l4t payloads.
The temporary core clone, BSP, optional board copy and target are removed after the build. This
command creates a candidate package only; it never accesses the Switch or SD.
"""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import runpy
import shutil
import subprocess
import tempfile
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
PATCH = ROOT / "patches/scarlet/xhci-cooperative-waits.patch"
BASE = "6fa4a4ac2c4a1b05034057b16f614736a44344b2"
PATCH_SHA256 = "b169f30efa94c9758d445c3a79c65b9b66d12d5dd6fa544074d6b783f22196ae"
GIT_URL = "https://github.com/petitstrawberry/Scarlet"
BOARD_COPY_IGNORES = {".git", "target", ".scarlet", ".cache", "cache", "__pycache__"}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree_hashes(root):
    return {str(p.relative_to(root)): digest(p) for p in sorted(root.rglob("*"))
            if p.is_file() and not {".git", "target"}.intersection(p.relative_to(root).parts)}


def protected_hashes(project):
    paths = [project / name for name in ("scarlet.toml", "scarlet.lock",
                                         "bsp/Cargo.toml", "bsp/Cargo.lock")]
    paths += [p for p in (project / ".scarlet/l4t").rglob("*") if p.is_file()]
    return {str(p.relative_to(project)): digest(p) for p in sorted(paths)}


def cached_core(cargo_home):
    matches = []
    for path in cargo_home.glob(f"git/checkouts/scarlet-*/{BASE[:7]}*"):
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=path,
                                           text=True).strip()
        if revision == BASE:
            matches.append(path)
    if len(matches) != 1:
        raise ValueError(f"expected one cached Scarlet checkout at {BASE}, found {len(matches)}")
    return matches[0]


def patched_source_hashes(core):
    """Include files added by git apply, which are untracked until committed."""
    tracked = subprocess.check_output(["git", "diff", "--name-only", "-z"], cwd=core)
    added = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"], cwd=core)
    paths = sorted({os.fsdecode(path) for path in (tracked + added).split(b"\0") if path})
    return {relative: digest(core / relative) if (core / relative).is_file() else None
            for relative in paths}


def board_patch_targets(board, patch):
    """Reject patch targets outside the copied drivers or through symlinks."""
    # git apply --numstat reports only the destination of a rename/copy, so it
    # cannot establish scope for the source. Board candidates need plain edits
    # and new/deleted files only; reject extended moves before invoking Git.
    if any(line.startswith((b"rename from ", b"rename to ", b"copy from ", b"copy to "))
           for line in patch.read_bytes().splitlines()):
        raise ValueError("board patch rename/copy operations are not supported")
    records = subprocess.check_output(["git", "apply", "--numstat", "-z", str(patch)],
                                      cwd=board).split(b"\0")
    targets, index = [], 0
    while index < len(records) and records[index]:
        fields = records[index].split(b"\t", 2)
        if len(fields) != 3:
            raise ValueError("invalid board patch numstat record")
        index += 1
        paths = [fields[2]]
        if not fields[2]:
            raise ValueError("invalid board patch path record")
        for name in paths:
            relative = PurePosixPath(os.fsdecode(name))
            if relative.is_absolute() or ".." in relative.parts or \
                    len(relative.parts) < 2 or relative.parts[0] != "drivers":
                raise ValueError(f"board patch target must be under copied drivers: {relative}")
            target = board
            for component in relative.parts:
                target /= component
                if target.is_symlink():
                    raise ValueError(f"board patch target traverses a symlink: {relative}")
            if not target.parent.is_dir() or (target.exists() and not target.is_file()):
                raise ValueError(f"board patch target is not a regular copied path: {relative}")
            targets.append(str(relative))
    if not targets:
        raise ValueError("board patch has no file targets")
    return sorted(set(targets))


def copy_board_sources(board, repository):
    drivers = repository / "drivers"
    for path in drivers.rglob("*"):
        if BOARD_COPY_IGNORES.intersection(path.relative_to(drivers).parts):
            continue
        if path.is_symlink():
            raise ValueError(f"board source contains a symlink: {path}")
    shutil.copytree(drivers, board / "drivers",
                    ignore=shutil.ignore_patterns(*sorted(BOARD_COPY_IGNORES)))
    # GM20B has ../../../shared path dependencies. Keep those unchanged inputs
    # in their original tree; board patches cannot address this symlink.
    (board / "shared").symlink_to(repository / "shared", target_is_directory=True)
    subprocess.run(["git", "init", "--quiet"], cwd=board, check=True)


def apply_board_patches(board, paths, metadata):
    before = tree_hashes(board / "drivers")
    for patch, record in zip(paths, metadata, strict=True):
        if digest(patch) != record["sha256"]:
            raise ValueError(f"board patch changed before application: {patch}")
        targets = board_patch_targets(board, patch)
        subprocess.run(["git", "apply", "--check", str(patch)], cwd=board, check=True)
        subprocess.run(["git", "apply", str(patch)], cwd=board, check=True)
        for relative in targets:
            if (board / relative).is_symlink():
                raise ValueError(f"board patch created a symlink: {relative}")
    after = tree_hashes(board / "drivers")
    return {f"drivers/{relative}": after.get(relative)
            for relative in sorted(before.keys() | after.keys())
            if before.get(relative) != after.get(relative)}


def module_paths(project, config, board=None, repository=ROOT):
    paths = {}
    drivers = (repository / "drivers").resolve()
    for name, module in config["modules"].items():
        if "path" not in module:
            raise ValueError(f"expected local board module: {name}")
        source = project / module["path"]
        absolute = source.resolve()
        if board is not None:
            # A lexical covered path resolving outside drivers is an escape,
            # not an unrelated module to leave in its original location.
            lexical = Path(os.path.abspath(source))
            if lexical.is_relative_to(drivers) and not absolute.is_relative_to(drivers):
                raise ValueError(f"board module escapes drivers: {name}")
            if absolute.is_relative_to(drivers):
                absolute = board / "drivers" / absolute.relative_to(drivers)
                if absolute.is_symlink() or absolute.resolve() != absolute or \
                        not (absolute / "Cargo.toml").is_file():
                    raise ValueError(f"board module is missing from isolated drivers: {name}")
        paths[name] = absolute
    return paths


def create_project(work, project, core, config, cargo_home, board=None, repository=ROOT):
    shutil.copytree(project / "bsp", work / "bsp", ignore=shutil.ignore_patterns("target"))
    quoted = json.dumps
    features = ", ".join(f"{key} = {str(value).lower()}"
                         for key, value in config["bsp"]["kernel"]["features"].items())
    lines = ["schema_version = 2", "[project]", 'name = "switch-usb-patched-kernel"',
             "[bsp]", 'path = "bsp"', 'package = "scarlet"', "[bsp.kernel]",
             f"source = {{ git = {quoted(GIT_URL)}, rev = {quoted(BASE)} }}",
             f"features = {{ {features} }}", "[modules]"]
    paths = module_paths(project, config, board, repository)
    for name, module in config["modules"].items():
        absolute = paths[name]
        lines.append(f"{quoted(name)} = {{ path = {quoted(str(absolute))}, "
                     f"enabled = {str(module['enabled']).lower()} }}")
    (work / "scarlet.toml").write_text("\n".join(lines) + "\n")
    # The scalar replaces the original Git source table in cargo-scarlet's
    # configuration merge. A path table would leave the Git keys present.
    (work / "scarlet.local.toml").write_text(
        f"[bsp.kernel]\nsource = {quoted(str(core / 'kernel'))}\n")
    manifest = work / "bsp/Cargo.toml"
    manifest.write_text(manifest.read_text() + f'\n[patch."{GIT_URL}"]\n'
                        f"scarlet = {{ path = {quoted(str(core / 'kernel'))} }}\n"
                        f"scarlet-abi = {{ path = {quoted(str(core / 'user/lib/scarlet-abi'))} }}\n")
    cache = work / ".scarlet/cache"
    cache.mkdir(parents=True)
    (cache / "cargo-home").symlink_to(cargo_home, target_is_directory=True)
    return paths


def check_identity(work, core, env):
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
               "--filter-platform", str(work / "bsp/targets/aarch64-switch-none-elf.json")]
    metadata = json.loads(subprocess.check_output(command, cwd=work / "bsp", env=env))
    selected = {}
    for name, relative in (("scarlet", "kernel"), ("scarlet-abi", "user/lib/scarlet-abi")):
        packages = [p for p in metadata["packages"] if p["name"] == name]
        expected = (core / relative / "Cargo.toml").resolve()
        if len(packages) != 1 or packages[0]["source"] is not None or \
                Path(packages[0]["manifest_path"]).resolve() != expected:
            raise ValueError(f"duplicate or incorrect crate identity: {name}")
        selected[name] = {key: packages[0][key] for key in ("id", "source", "version", "manifest_path")}
    return selected


def package_candidate(stage, original, manifest, elf, image, core_patches, board_patches=None):
    package = runpy.run_path(str(PROJECT / "tools/package_l4t.py"))
    image_size = package["validate_image"](image.read_bytes(), elf.read_bytes())
    boot = f"switchroot/{manifest['boot_directory']}"
    names = {f"{boot}/{name}" for name in ("Image", "uImage", "bl31.bin", "bl33.bin",
                                         "boot.scr", "initramfs", "nx-plat.dtimg")}
    names.add(f"bootloader/ini/{manifest['entry_file']}")
    if set(manifest["sha256"]) != names:
        raise ValueError("the original package must contain exactly the eight console payloads")
    for relative, expected in manifest["sha256"].items():
        source = original / relative
        if digest(source) != expected:
            raise ValueError(f"original package SHA256 mismatch: {relative}")
        target = stage / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
    (stage / boot / "Image").write_bytes(image.read_bytes())
    (stage / boot / "uImage").write_bytes(package["legacy_image"](
        gzip.compress(image.read_bytes(), mtime=0), 2, "Scarlet Switch",
        package["LOAD"], package["LOAD"], 1))
    hashes = {relative: digest(stage / relative) for relative in sorted(names)}
    changed = {name for name in names if hashes[name] != manifest["sha256"][name]}
    if not changed.issubset({f"{boot}/Image", f"{boot}/uImage"}):
        raise ValueError("candidate changed a non-kernel payload")
    candidate = dict(manifest)
    candidate.pop("kernel_source_revision", None)
    candidate.update(hardware_validated=False, kernel_source_base_revision=BASE,
                     kernel_source_patch_sha256=PATCH_SHA256,
                     kernel_source_status="base revision plus recorded patches; physical validation pending",
                     kernel_source={"git": GIT_URL, "base_revision": BASE,
                                    "patch": str(PATCH.relative_to(ROOT)), "patch_sha256": PATCH_SHA256,
                                    "patches": core_patches},
                     kernel_elf_sha256=digest(elf), image_runtime_size=image_size, sha256=hashes)
    if board_patches:
        candidate["board_source_patches"] = board_patches
    (stage / "manifest.json").write_text(json.dumps(candidate, indent=2) + "\n")
    return candidate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=PROJECT / ".scarlet/usb-xhci-candidate")
    parser.add_argument("--core-patch", type=Path, action="append", default=[],
                        help="additional repository kernel patch, applied after the USB patch")
    parser.add_argument("--board-patch", type=Path, action="append", default=[],
                        help="repository patch with drivers/... paths, applied to an isolated board copy")
    args = parser.parse_args()
    patch_paths = [PATCH]
    for path in args.core_patch:
        path = path.resolve()
        if not path.is_relative_to(ROOT / "patches/scarlet") or not path.is_file():
            parser.error(f"core patch must be a file under patches/scarlet: {path}")
        if path in patch_paths:
            parser.error(f"duplicate core patch: {path}")
        patch_paths.append(path)
    core_patches = [{"patch": str(path.relative_to(ROOT)), "sha256": digest(path)}
                    for path in patch_paths]
    board_paths = []
    for path in args.board_patch:
        path = path.resolve()
        if not path.is_relative_to(ROOT / "patches") or not path.is_file():
            parser.error(f"board patch must be a file under patches: {path}")
        if path in board_paths:
            parser.error(f"duplicate board patch: {path}")
        board_paths.append(path)
    board_patches = [{"patch": str(path.relative_to(ROOT)), "sha256": digest(path)}
                     for path in board_paths]
    output = args.output.resolve()
    original = PROJECT / ".scarlet/l4t"
    cargo_home = (PROJECT / ".scarlet/cache/cargo-home").resolve()
    protected_outputs = [original, cargo_home, PROJECT / "bsp"]
    if board_paths:
        protected_outputs.extend((ROOT / "drivers", ROOT / "shared"))
    for protected in protected_outputs:
        if output.is_relative_to(protected) or protected.is_relative_to(output):
            parser.error(f"output overlaps protected input: {protected}")
    for command in ("git", "cargo", "cargo-scarlet", "llvm-objcopy", "llvm-objdump"):
        if shutil.which(command) is None:
            parser.error(f"missing required command on PATH: {command}")
    if digest(PATCH) != PATCH_SHA256:
        parser.error("the durable USB patch differs from the reviewed patch")
    config = tomllib.loads((PROJECT / "scarlet.toml").read_text())
    if config["bsp"]["kernel"]["source"] != {"git": GIT_URL, "rev": BASE}:
        parser.error("project kernel source no longer matches this patch's base")
    manifest = json.loads((original / "manifest.json").read_text())
    if manifest["kernel_source_revision"] != BASE:
        parser.error("existing console package does not match the pinned base")
    source = cached_core(cargo_home)
    before = {"production": protected_hashes(PROJECT), "cached_core": tree_hashes(source)}
    if board_paths:
        before.update(board_sources=tree_hashes(ROOT / "drivers"),
                      shared_sources=tree_hashes(ROOT / "shared"))
    output.mkdir(parents=True, exist_ok=True)
    result = {"result": "fail", "base_revision": BASE, "patch_sha256": PATCH_SHA256,
              "core_patches": core_patches,
              "sd_written": False, "physical_tested": False, "switchvisor_used": False}
    started = time.monotonic()
    if board_paths:
        result["board_patches"] = board_patches

    def input_hashes():
        values = {"production": protected_hashes(PROJECT), "cached_core": tree_hashes(source)}
        if board_paths:
            values.update(board_sources=tree_hashes(ROOT / "drivers"),
                          shared_sources=tree_hashes(ROOT / "shared"))
        return values
    try:
        with tempfile.TemporaryDirectory(prefix="usb-patched-build-", dir=output.parent) as temporary:
            temporary = Path(temporary)
            core, work, stage = temporary / "core", temporary / "project", temporary / "package"
            subprocess.run(["git", "clone", "--quiet", "--shared", str(source), str(core)], check=True)
            subprocess.run(["git", "checkout", "--quiet", "--detach", BASE], cwd=core, check=True)
            for path, metadata in zip(patch_paths, core_patches):
                if digest(path) != metadata["sha256"]:
                    raise ValueError(f"core patch changed before application: {path}")
                subprocess.run(["git", "apply", "--check", str(path)], cwd=core, check=True)
                subprocess.run(["git", "apply", str(path)], cwd=core, check=True)
            result["patched_source_sha256"] = patched_source_hashes(core)
            board = None
            if board_paths:
                board = temporary / "board"
                copy_board_sources(board, ROOT)
                result["board_patched_source_sha256"] = apply_board_patches(
                    board, board_paths, board_patches)
                result["original_board_module_sources"] = {
                    name: tree_hashes(path)
                    for name, path in module_paths(PROJECT, config).items()
                    if config["modules"][name]["enabled"]}
            effective_modules = create_project(work, PROJECT, core, config, cargo_home, board)
            env = os.environ.copy()
            env.update(CARGO_HOME=str(cargo_home), CARGO_NET_OFFLINE="true")
            for key in ("CARGO_TARGET_DIR", "CARGO_UNSTABLE_BUILD_STD", "CARGO_UNSTABLE_BUILD_STD_FEATURES"):
                env.pop(key, None)
            command = ["cargo", "scarlet", "build", "--project", str(work), "--release", "--locked"]
            result["command"] = command
            result["board_module_sources"] = {
                name: tree_hashes(path) for name, path in effective_modules.items()
                if config["modules"][name]["enabled"]}
            with (output / "build.log").open("w") as log:
                try:
                    subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
                finally:
                    lock = work / "bsp/Cargo.lock"
                    if lock.is_file():
                        shutil.copyfile(lock, output / "isolated-Cargo.lock")
                        result["isolated_cargo_lock_sha256"] = digest(output / "isolated-Cargo.lock")
            result["crate_identity"] = check_identity(work, core, env)
            stage.mkdir()
            elf = stage / "kernel.elf"
            shutil.copyfile(work / "bsp/target/aarch64-switch-none-elf/release/scarlet", elf)
            assembly = subprocess.check_output(
                ["llvm-objdump", "--mattr=+lse", "--no-show-raw-insn", "-d", str(elf)], text=True)
            count, outlined = runpy.run_path(str(ROOT / "tests/check-isa.py"))["audit_disassembly"](assembly)
            if count == 0 or outlined != 0:
                raise ValueError("kernel ISA audit did not establish an LSE-free executable")
            result["isa"] = {"decoded_instructions": count, "unguarded_lse_instructions": 0,
                             "guarded_outline_lse_instructions": outlined, "elf_sha256": digest(elf)}
            image = temporary / "Image"
            subprocess.run(["llvm-objcopy", "--remove-section=.ksym", "-O", "binary",
                            str(elf), str(image)], check=True)
            candidate = package_candidate(stage, original, manifest, elf, image, core_patches, board_patches)
            after = input_hashes()
            result["unchanged_inputs"] = {key: before[key] == after[key] for key in before}
            if not all(result["unchanged_inputs"].values()):
                raise ValueError("production input or cached source changed during build")
            for relative in [*candidate["sha256"], "manifest.json", "kernel.elf"]:
                destination = output / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(stage / relative, destination)
            result.update(result="pass", kernel_elf_sha256=digest(output / "kernel.elf"),
                          package_sha256=candidate["sha256"], image_runtime_size=candidate["image_runtime_size"],
                          original_package_sha256=digest(original / "manifest.json"))
    except Exception as error:
        result["error"] = str(error)
        raise
    finally:
        fingerprint_error = None
        try:
            after = input_hashes()
            result["unchanged_inputs"] = {key: before[key] == after[key] for key in before}
            if not all(result["unchanged_inputs"].values()):
                fingerprint_error = "production input or cached source changed during build"
        except Exception as error:
            fingerprint_error = f"protected input verification failed: {error}"
        if fingerprint_error:
            result.update(result="fail", error=fingerprint_error)
        result.update(elapsed_seconds=time.monotonic() - started, temporary_sources_removed=True)
        (output / "build-receipt.json").write_text(json.dumps(result, indent=2) + "\n")
        if fingerprint_error:
            raise ValueError(fingerprint_error)
    print(f"USB candidate packaged: {output}")
    print(f"base={BASE}, patch={PATCH_SHA256}; physical validation pending")


if __name__ == "__main__":
    main()

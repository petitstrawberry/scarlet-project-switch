#!/usr/bin/env python3
"""Boot Scarlet with a test-only H4 radio, then exercise btctl, joycond and SWS navigation."""
import argparse
import importlib.util
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import tomllib

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.dont_write_bytecode = True


PROJECT = ROOT / "projects/aarch64-switch-l4t"
DAEMON = ROOT / "userspace/joycond/Cargo.toml"
USER_TARGET = "aarch64-unknown-scarlet"


def pinned_source(source):
    # Metadata resolves the actual Cargo checkout, including its non-crate
    # files (linker script, QEMU image helper and SWS navigation policy).
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version=1", "--locked",
         "--manifest-path", str(HERE / "Cargo.toml")], cwd=ROOT))
    for package in metadata["packages"]:
        if package["name"] == "scarlet" and (package["source"] or "").endswith("#" + source["rev"]):
            return Path(package["manifest_path"]).parents[1]
    raise RuntimeError("QA kernel dependency does not match the Switch project pin")


def build(out, override):
    source = tomllib.loads((PROJECT / "scarlet.toml").read_text())["bsp"]["kernel"]["source"]
    core = override.resolve(strict=True) if override else pinned_source(source)
    print(f"Scarlet source: {core}", flush=True)
    base_env = {**os.environ, "SCARLET_SOURCE": str(core)}

    def cargo(manifest, args, target_dir, log, env=None, workspace=None):
        cmd = ["cargo", "build", "--release", "--manifest-path", str(manifest),
               "--target-dir", str(target_dir)]
        if override and workspace:
            # Only the lockfiles in output may change for path-patched builds.
            lock = out / "locks" / workspace.name / "Cargo.lock"
            lock.parent.mkdir(exist_ok=True, parents=True)
            shutil.copyfile(workspace / "Cargo.lock", lock)
            cmd += ["-Zunstable-options", "--lockfile-path", str(lock)]
            names = ["scarlet", "scarlet-os", "scarlet-abi", "sws-protocol"] if workspace == HERE else ["scarlet-os", "scarlet-abi"]
            for name in names:
                path = core / ("kernel" if name == "scarlet" else f"user/lib/{name}")
                cmd += ["--config", f'patch.{json.dumps(source["git"])}.{name}.path={json.dumps(str(path))}']
        else:
            cmd.append("--locked")
        cmd += args
        with (out / log).open("w") as destination:
            result = subprocess.run(cmd, cwd=core, env=env or base_env, stdout=destination, stderr=subprocess.STDOUT)
        if result.returncode:
            lines = (out / log).read_text().splitlines()
            first = next((i for i, line in enumerate(lines) if line.startswith("error")), None)
            excerpt = lines[-30:] if first is None else lines[first:first + 30]
            raise RuntimeError(f"Build failed ({log}):\n" + "\n".join(excerpt))

    spec = json.loads((core / "kernel/targets/aarch64-unknown-none-elf.json").read_text())
    spec.pop("pre-link-args")
    target = out / "bt-none.json"
    target.write_text(json.dumps(spec))
    env = base_env.copy()
    env["CARGO_UNSTABLE_JSON_TARGET_SPEC"] = "true"
    env["RUSTFLAGS"] = (f"-C link-arg=-T{core}/tools/network-management/guest/kernel.ld "
                        "-C no-vectorize-loops -C no-vectorize-slp")
    cargo(HERE / "fixture/Cargo.toml", ["--target", str(target), "-Z",
          "build-std=core,alloc,compiler_builtins", "-Z", "build-std-features=compiler-builtins-mem"],
          out / "kernel", "kernel.log", env, workspace=HERE)
    subprocess.run(["llvm-objcopy", "--remove-section=.ksym", "-O", "binary",
                    out / "kernel/bt-none/release/bluetooth-qa-kernel", out / "Image"], check=True)
    for manifest, args, log, workspace in [
        (core / "user/bin/Cargo.toml", ["--no-default-features", "--bin", "init"], "init.log", None),
        (core / "user/scarlet-ld/Cargo.toml", [], "loader.log", None),
        (core / "user/bluetooth/Cargo.toml", ["--bin", "btctl"], "btctl.log", None),
        (DAEMON, ["--bin", "joycond"], "joycond.log", DAEMON.parent),
        (HERE / "guest/Cargo.toml", [], "guest.log", HERE),
    ]:
        cargo(manifest, ["--target", USER_TARGET, *args], out / "user", log, workspace=workspace)

    module = importlib.util.spec_from_file_location("loader_smoke", core / "tests/loader-smoke/run-qemu.py")
    smoke = importlib.util.module_from_spec(module)
    module.loader.exec_module(smoke)
    stage = out / "stage"
    for name in ["bin", "dev", "etc", "mnt/newroot", "root", "tmp"]:
        (stage / name).mkdir(mode=0o755, parents=True, exist_ok=True)
    for name in ["init", "bin/scarlet-ld", "bin/btctl", "bin/joycond", "bin/bluetooth-qa"]:
        binary = out / "user/aarch64-unknown-scarlet/release" / Path(name).name
        (stage / name).write_bytes(binary.read_bytes())
        (stage / name).chmod(0o755)
    smoke.create_initramfs(stage, out / "initramfs.cpio")


def boot(out):
    stage = out / "stage"
    # btctl verifies the inode permissions protecting /etc and its bond files.
    # Use a private ext2 root as on the console. Archive-only cpio metadata
    # cannot describe the ownership/ACL information required for pairing.
    disk = out / "root.ext2"
    with disk.open("wb") as stream:
        stream.truncate(48 * 1024 * 1024)
    subprocess.run(["mke2fs", "-q", "-F", "-t", "ext2", "-b", "4096",
                    "-E", "root_owner=0:0", "-d", str(stage), str(disk)], check=True)
    for field in ["uid", "gid"]:
        subprocess.run(["debugfs", "-w", "-R", f"set_inode_field /etc {field} 0", str(disk)],
                       check=True, stdout=subprocess.DEVNULL)
    cmd = ["qemu-system-aarch64", "-machine", "virt,gic-version=3", "-cpu", "max", "-m", "2G",
           "-smp", "2", "-nic", "none", "-display", "none", "-monitor", "none", "-serial", "stdio",
           "-no-reboot", "-kernel", str(out / "Image"), "-initrd", str(out / "initramfs.cpio"),
           "-global", "virtio-mmio.force-legacy=false",
           "-drive", f"id=rootfs,file={disk},if=none,format=raw",
           "-device", "virtio-blk-device,drive=rootfs",
           "-append", "console=ttyAMA0 root=/dev/vblk0 rootwait rootfstype=ext2 init.exec=/bin/bluetooth-qa"]
    log = out / "serial.log"
    with log.open("wb") as serial, subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=serial,
                                                    stderr=subprocess.STDOUT) as qemu:
        try:
            deadline = time.monotonic() + 180
            reported = 0
            while time.monotonic() < deadline:
                output = log.read_text(errors="replace")
                lines = output.splitlines()
                complete = len(lines) if output.endswith("\n") else max(0, len(lines) - 1)
                for line in lines[reported:complete]:
                    if line.startswith(("BT_QA_", "BLUETOOTH_")):
                        print(line, flush=True)
                    if line == "BLUETOOTH_QEMU_PASS":
                        return
                reported = complete
                if qemu.poll() is not None or any("panicked at" in line or "[panic]" in line for line in lines):
                    break
                time.sleep(0.1)
            raise RuntimeError("Bluetooth QEMU failed:\n" + "\n".join(lines[-60:]))
        finally:
            if qemu.poll() is None:
                qemu.terminate()
                try:
                    qemu.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    qemu.kill()
                    qemu.wait()


def run(out, skip_build, override):
    out.mkdir(exist_ok=True, parents=True)
    if not skip_build:
        build(out, override)
    boot(out)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scarlet-source", type=Path, help="test a local Scarlet checkout instead of the board pin")
    parser.add_argument("--output", type=Path, help="retain images and serial.log in a dedicated scratch directory")
    parser.add_argument("--no-build", action="store_true", help="reuse this test's previously built artifacts")
    args = parser.parse_args()
    if args.no_build and not args.output:
        parser.error("--no-build requires --output")
    if args.output:
        run(args.output.resolve(), args.no_build, args.scarlet_source)
    else:
        with tempfile.TemporaryDirectory(prefix="scarlet-bluetooth-qa-") as scratch:
            run(Path(scratch), False, args.scarlet_source)

#!/usr/bin/env python3
"""Execute the actual Maxwell DSO under scarlet-ld on a QEMU Cortex-A57.

Run in the project's Nix development shell after building the native driver
and tests/boot-probe kernel. This covers loading, ABI negotiation, native std
on missing-GPU calls, and rejected boundary outputs; it cannot test GM20B work.
All generated clients, loader builds, archives, and logs are isolated in --output.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
sys.dont_write_bytecode = True


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def newc_entry(archive, name, mode, content, inode):
    encoded = name.encode() + b"\0"
    fields = (inode, mode, 0, 0, 1, 0, len(content), 0, 0, 0, 0, len(encoded), 0)
    archive.write(b"070701" + b"".join(f"{field:08x}".encode() for field in fields))
    archive.write(encoded)
    archive.write(bytes(-(110 + len(encoded)) % 4))
    archive.write(content)
    archive.write(bytes(-len(content) % 4))


def archive(staging, destination):
    with destination.open("wb") as output:
        paths = [staging, *sorted(staging.rglob("*"))]
        for inode, path in enumerate(paths, 1):
            mode = path.stat().st_mode
            name = "." if path == staging else path.relative_to(staging).as_posix()
            if not stat.S_ISDIR(mode) and not stat.S_ISREG(mode):
                raise ValueError(f"unsupported staging entry: {path}")
            newc_entry(output, name, mode, path.read_bytes() if path.is_file() else b"", inode)
        newc_entry(output, "TRAILER!!!", 0, b"", len(paths) + 1)
        output.write(bytes(-output.tell() % 512))


def run(command, commands, log, **kwargs):
    resolved = [str(value) for value in command]
    resolved[0] = shutil.which(resolved[0]) or resolved[0]
    commands.append(resolved)
    subprocess.run(commands[-1], stdout=log, stderr=subprocess.STDOUT, check=True, **kwargs)


def prepare(args, output, commands):
    distribution = args.scarlet_source.resolve(strict=True)
    sgfx = args.sgfx_source.resolve(strict=True)
    audit = module("maxwell_dynamic_builder", ROOT / "scripts/build-sgfx-maxwell.py")
    elf = module("maxwell_dynamic_elf", distribution / "tools/elf_audit.py")
    boot = module("maxwell_dynamic_boot", ROOT / "tests/qemu-smoke.py")
    package = module("maxwell_dynamic_package", ROOT / "projects/aarch64-switch-l4t-console/tools/package_l4t.py")
    staging = output / "staging"
    staging.mkdir(parents=True, exist_ok=True)
    for directory in ("bin", "dev", "system/lib/sgfx"):
        (staging / directory).mkdir(parents=True, exist_ok=True)
    library = staging / "system/lib/sgfx/libsgfx_scarlet_maxwell.so"
    shutil.copy2(args.library, library)
    report = audit.audit_driver(library)
    has_ycbcr = "sgfx_backend_get_ycbcr_api_v2" in report["exports"]
    (output / "driver-elf.json").write_text(json.dumps(report, indent=2) + "\n")
    env = os.environ.copy()
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_UNSTABLE_BUILD_STD",
                 "CARGO_UNSTABLE_BUILD_STD_FEATURES"):
        env.pop(name, None)
    with (output / "build.log").open("w") as log:
        if args.loader:
            loader = args.loader.resolve()
        else:
            target_dir = output / "loader-target"
            run(["cargo", "build", "--locked", "--offline", "--release", "--target", "aarch64-unknown-scarlet",
                 "--manifest-path", distribution / "user/scarlet-ld/Cargo.toml", "--target-dir", target_dir],
                commands, log, cwd=distribution, env=env)
            loader = target_dir / "aarch64-unknown-scarlet/release/scarlet-ld"
        shutil.copy2(loader, staging / "bin/scarlet-ld")
        compiler = os.environ.get("TARGET_CC", "clang")
        client_object = output / "client.o"
        run([compiler, "--target=aarch64-unknown-none-elf", "-std=c11", "-fPIC", "-ffreestanding",
             "-fno-stack-protector", "-fno-builtin", "-fno-asynchronous-unwind-tables", "-O1",
             f"-DSCARLET_REQUIRE_YCBCR={int(has_ycbcr)}", "-I", sgfx / "crates/sgfx-backend-abi/include",
             "-c", ROOT / "tests/sgfx-maxwell-dynamic-smoke.c", "-o", client_object], commands, log)
        # A shared input makes LLD keep interpreter-supplied dl* imports.
        # --as-needed leaves the Maxwell DSO out of DT_NEEDED; only dlopen loads it.
        linker = shutil.which("ld.lld") or shutil.which("rust-lld")
        if linker is None:
            raise ValueError("ld.lld or rust-lld is required")
        link_command = [linker, "-flavor", "gnu"] if Path(linker).name == "rust-lld" else [linker]
        run([*link_command, "--hash-style=both", "-z", "now", "-z", "max-page-size=4096", "-pie",
             "--export-dynamic", "--unresolved-symbols=ignore-all", "--dynamic-linker=/bin/scarlet-ld",
             "-e", "_start", client_object, "--as-needed", library, "-o", staging / "init"], commands, log)
        with (staging / "init").open("r+b") as client:
            client.seek(7)
            client.write(bytes([83]))
        run(["aarch64-unknown-linux-gnu-as", "--defsym=ENTER_EL1=0", ROOT / "tests/entry.S",
             "-o", output / "entry.o"], commands, log)
        run(["llvm-objcopy", "-O", "binary", output / "entry.o", output / "entry.bin"], commands, log)
        run(["llvm-objcopy", "--remove-section=.ksym", "-O", "binary", args.kernel, output / "Image"], commands, log)
        package.validate_image((output / "Image").read_bytes(), args.kernel.read_bytes())
        client_report = elf.Elf(staging / "init").report()
        undefined = {item["name"] for item in client_report["undefined_relocated_symbols"]}
        if (client_report["osabi"] != 83 or client_report["machine"] != "aarch64"
                or client_report["interpreter"] != "/bin/scarlet-ld" or client_report["needed"]
                or undefined != {"dlopen", "dlsym", "dlerror", "dlclose"}):
            raise ValueError(f"unexpected native client linkage: {client_report}")
        (output / "client-elf.json").write_text(json.dumps(client_report, indent=2) + "\n")
        loader_report = elf.Elf(staging / "bin/scarlet-ld").report()
        if (loader_report["osabi"] != 83 or loader_report["machine"] != "aarch64"
                or loader_report["interpreter"] or loader_report["needed"]):
            raise ValueError(f"unexpected interpreter linkage: {loader_report}")
        (output / "loader-elf.json").write_text(json.dumps(loader_report, indent=2) + "\n")
        archive(staging, output / "initramfs.cpio")
        payload = (output / "initramfs.cpio").read_bytes()
        (output / "initramfs").write_bytes(package.legacy_image(payload, 3, "Maxwell dynamic smoke"))
        boot.legacy_payload(output / "initramfs", 3, 0)
        (output / "input.dts").write_text(boot.fixture(len(payload), "kernel", framebuffer=False))
        run(["dtc", "-q", "-I", "dts", "-O", "dtb", "-o", output / "input.dtb", output / "input.dts"], commands, log)
    return {"library_sha256": report["sha256"], "library_source": str(args.library.resolve()),
            "has_ycbcr": has_ycbcr, "scarlet_distribution_revision": distribution.name,
            "sgfx_abi_revision": sgfx.name,
            "kernel_elf_sha256": hashlib.sha256(args.kernel.read_bytes()).hexdigest(),
            "kernel_elf_source": str(args.kernel.resolve()),
            "loader_sha256": loader_report["sha256"], "client_sha256": client_report["sha256"]}


def execute(output, timeout, commands):
    command = [shutil.which("qemu-system-aarch64") or "qemu-system-aarch64", "-machine", "virt,secure=on,virtualization=on,gic-version=2",
               "-cpu", "cortex-a57", "-m", "3G", "-smp", "4", "-accel", "tcg",
               "-nodefaults", "-display", "none", "-serial", f"file:{output / 'serial.log'}", "-monitor", "none",
               "-device", f"loader,file={output / 'entry.bin'},addr=0x80000000,cpu-num=0,force-raw=on",
               "-device", f"loader,file={output / 'Image'},addr=0x80200000,force-raw=on",
               "-device", f"loader,file={output / 'input.dtb'},addr=0x8d000000,force-raw=on",
               "-device", f"loader,file={output / 'initramfs'},addr=0x92000000,force-raw=on"]
    for cpu in range(1, 4):
        command += ["-device", f"loader,file={output / 'entry.bin'},addr=0x80000000,cpu-num={cpu},force-raw=on"]
    commands.append(command)
    (output / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
    started = time.monotonic()
    reason = "timeout waiting for the dynamic smoke marker"
    with (output / "qemu.log").open("w") as log, subprocess.Popen(command, stdout=log, stderr=log) as process:
        try:
            while time.monotonic() - started < timeout:
                serial = output / "serial.log"
                text = serial.read_text(errors="replace") if serial.exists() else ""
                if re.search(r"SCARLET_MAXWELL_DYNAMIC_FAIL|scarlet-ld: |panicked at|Panic occurred|\[panic\]", text):
                    reason = "guest driver/interpreter failure"
                    break
                if "\nSCARLET_MAXWELL_DYNAMIC_OK\n" in text.replace("\r", ""):
                    reason = "PASS"
                    break
                if process.poll() is not None:
                    reason = "QEMU exited before the dynamic smoke marker"
                    break
                time.sleep(0.05)
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    return {"result": reason, "elapsed_seconds": round(time.monotonic() - started, 3),
            "qemu_exit_code": process.returncode}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scarlet-source", type=Path, required=True)
    parser.add_argument("--sgfx-source", type=Path, required=True)
    parser.add_argument("--library", type=Path, default=ROOT / "projects/aarch64-switch-l4t-console/.scarlet/sgfx-maxwell/libsgfx_scarlet_maxwell.so")
    parser.add_argument("--kernel", type=Path, default=ROOT / "tests/boot-probe/bsp/target/aarch64-switch-none-elf/release/scarlet")
    parser.add_argument("--loader", type=Path, help="reuse a built pinned interpreter instead of building one")
    parser.add_argument("--output", type=Path, default=ROOT / ".cache/qa/sgfx-maxwell-dynamic")
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--prepare-only", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    (output / "result.json").write_text('{"result":"PREPARING"}\n')
    (output / "serial.log").unlink(missing_ok=True)
    commands = []
    evidence = {}
    try:
        if not 0 < args.timeout < float("inf"):
            raise ValueError("--timeout must be positive")
        evidence = prepare(args, output, commands)
        result = {"result": "PREPARED"} if args.prepare_only else execute(output, args.timeout, commands)
        result.update(evidence)
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"Maxwell native dynamic smoke: {result['result']} ({output})")
        if not args.prepare_only and (output / "serial.log").exists():
            print((output / "serial.log").read_text(errors="replace")[-6000:])
        return 0 if result["result"] in {"PREPARED", "PASS"} else 1
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        result = {"result": "ERROR", "error": str(error), **evidence}
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"Maxwell native dynamic smoke: {error}; inspect {output / 'build.log'}", file=sys.stderr)
        return 1
    finally:
        (output / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")


if __name__ == "__main__":
    sys.exit(main())

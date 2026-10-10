#!/usr/bin/env python3
"""Verify exact pinned TCP receive methods before and after bulk deque drain.

Apply the packaged core patches in a temporary clone, execute extracted methods
in a host boundary harness, and inspect optimized AArch64 drain-only code. No
cached source, kernel package, board, SD, or remote host is changed. Passing
these tests establishes semantics and code generation, not physical throughput.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BASE = "6fa4a4ac2c4a1b05034057b16f614736a44344b2"
SOURCE = "kernel/src/network/tcp.rs"
PATCHES = ["xhci-cooperative-waits.patch", "tcp-registry-drop-order.patch",
           "xhci-network-fairness.patch", "tcp-rx-stats-lock-scope.patch",
           "network-stage-profile.patch", "tegra-xhci-interrupt-moderation.patch"]
CANDIDATE = "tcp-bulk-receive-drain.patch"
HARNESS = ROOT / "tests/usb-probe-qa/tcp-bulk-receive-drain-host-tests.rs"
TEST_COUNT = 17


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def command(args, *, cwd=None, timeout=30):
    return subprocess.run(args, cwd=cwd, check=True, capture_output=True, timeout=timeout)


def block(source, marker):
    if source.count(marker) != 1:
        raise ValueError(f"expected exactly one production scope: {marker}")
    start = source.index(marker)
    opening = source.index("{", start)
    depth = 1
    end = opening + 1
    # These selected scopes have balanced braces in comments and strings.
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def extraction(source, patched):
    predicates = "\n\n".join(block(source, f"const fn {name}(") for name in
                              ["tcp_receive_side_open", "tcp_receive_side_eof"])
    helper = block(source, "fn drain_recv_buffer(") if patched else ""
    methods = "\n\n".join(block(source, f"    pub fn {name}(") for name in
                           ["recv_data", "recv_blocking"])
    return predicates, helper, methods


def run_semantics(tmp, output, label, source, rustc, host, patched):
    predicates, helper, methods = extraction(source, patched)
    harness = HARNESS.read_text().replace("/* PRODUCTION_STATE_PREDICATES */", predicates)
    harness = harness.replace("/* PRODUCTION_DRAIN_HELPER */", helper)
    harness = harness.replace("    /* PRODUCTION_RECEIVE_METHODS */", methods)
    path = tmp / f"{label}-host.rs"
    binary = tmp / f"{label}-host"
    path.write_text(harness)
    (output / f"{label}-production-methods.rs").write_text(helper + "\n" + methods + "\n")
    shutil.copyfile(path, output / path.name)
    compiled = command([rustc, "--edition=2024", "--target", host, "-C", "opt-level=2",
                        "--test", str(path), "-o", str(binary)])
    (output / f"{label}-compile.log").write_bytes(compiled.stdout + compiled.stderr)
    tested = command([str(binary), "--test-threads=1", "--nocapture"])
    logs = tested.stdout + tested.stderr
    (output / f"{label}-tests.log").write_bytes(logs)
    if f"{TEST_COUNT} passed; 0 failed".encode() not in logs:
        raise AssertionError(logs.decode())
    return {"label": label, "tests_passed": TEST_COUNT, "production_source_sha256":
            hashlib.sha256(source.encode()).hexdigest(), "harness_sha256": sha(path),
            "methods_sha256": sha(output / f"{label}-production-methods.rs")}


def codegen(tmp, output, label, source, rustc, patched):
    method = block(source, "    pub fn recv_data(")
    start = method.index("\n", method.index("let drain_profile =")) + 1
    end = method.index("            drop(drain_profile);", start)
    drain_body = method[start:end]
    helper = block(source, "fn drain_recv_buffer(") if patched else ""
    probe = "#![no_std]\nextern crate alloc;\nuse alloc::collections::VecDeque;\n" + helper + "\n"
    probe += "#[unsafe(no_mangle)]\npub fn drain_probe(mut recv_buf: &mut VecDeque<u8>, buffer: &mut [u8]) -> usize {\n"
    probe += "    let len = buffer.len().min(recv_buf.len());\n" + drain_body
    probe += "    len\n}\n"
    path = tmp / f"{label}_drain.rs"
    path.write_text(probe)
    args = [rustc, "--edition=2024", "--crate-type=lib", "--crate-name", f"{label}_drain",
            "--target", "aarch64-unknown-none", "-C", "opt-level=3", "-C", "target-cpu=cortex-a57",
            "-C", "target-feature=+strict-align,-neon,-fp-armv8", "-C", "panic=abort",
            "--emit=asm,llvm-ir", "--out-dir", str(tmp), str(path)]
    result = subprocess.run(args, capture_output=True, timeout=30)
    (output / f"{label}-codegen.log").write_bytes(result.stdout + result.stderr)
    result.check_returncode()
    for suffix in ["rs", "ll", "s"]:
        shutil.copyfile(tmp / f"{label}_drain.{suffix}", output / f"{label}-drain.{suffix}")
    ir = (tmp / f"{label}_drain.ll").read_text()
    match = re.search(r"^define .*@drain_probe\(.*?^}", ir, re.MULTILINE | re.DOTALL)
    if not match:
        raise AssertionError("optimized drain_probe definition missing")
    body = match.group(0)
    byte_loads = len(re.findall(r"\bload i8\b", body))
    byte_stores = len(re.findall(r"\bstore i8\b", body))
    memcpy_calls = len(re.findall(r"\bcall void @llvm\.memcpy", body))
    if patched:
        assert byte_loads == byte_stores == 0, "per-byte deque copy remained in optimized probe"
        assert memcpy_calls == 2, "two-slice production copy did not lower to two bulk copies"
        assert "@llvm.memmove" not in body, "prefix drain unexpectedly moved unread deque bytes"
    else:
        assert byte_loads > 0 and byte_stores > 0, "baseline per-byte deque loop not found"
    return {"label": label, "target": "aarch64-unknown-none", "optimization": 3,
            "target_cpu": "cortex-a57", "features": "+strict-align,-neon,-fp-armv8",
            "byte_load_sites": byte_loads, "byte_store_sites": byte_stores,
            "bulk_copy_call_sites": memcpy_calls, "probe_sha256": sha(path),
            "llvm_ir_sha256": sha(tmp / f"{label}_drain.ll"),
            "assembly_sha256": sha(tmp / f"{label}_drain.s"),
            "scope": "exact extracted production drain body with external VecDeque/copy boundaries; not a linked kernel"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, default=ROOT / "projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home/git/checkouts/scarlet-26bf9663864ed506/6fa4a4a")
    parser.add_argument("--output", type=Path, default=ROOT / ".cache/network-perf/tcp-bulk-drain")
    args = parser.parse_args()
    core = args.core.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    revision = command(["git", "rev-parse", "HEAD"], cwd=core).stdout.decode().strip()
    if revision != BASE:
        raise ValueError(f"expected pinned core {BASE}, found {revision}")
    before = {"source_sha256": sha(core / SOURCE), "git_status": command(["git", "status", "--porcelain"], cwd=core).stdout.decode()}
    rustc = shutil.which(os.environ.get("RUSTC", "rustc"))
    version = command([rustc, "-vV"]).stdout.decode()
    host = re.search(r"^host: (.+)$", version, re.MULTILINE).group(1)
    receipt = {"base_revision": BASE, "patches": {name: sha(ROOT / "patches/scarlet" / name)
               for name in PATCHES + [CANDIDATE]}, "runner_sha256": sha(Path(__file__)),
               "harness_sha256": sha(HARNESS), "rustc": version, "physical_tested": False,
               "throughput_measured": False, "boundary_models": ["IRQ lock", "profile span", "task waker", "state", "window ACK"],
               "runs": [], "codegen": []}
    with tempfile.TemporaryDirectory(prefix="tcp-drain-validation-", dir=output) as directory:
        tmp = Path(directory)
        copy = tmp / "core"
        command(["git", "clone", "--quiet", "--local", "--no-hardlinks", str(core), str(copy)])
        command(["git", "checkout", "--quiet", "--detach", BASE], cwd=copy)
        for name in PATCHES:
            patch = ROOT / "patches/scarlet" / name
            command(["git", "apply", "--check", str(patch)], cwd=copy)
            command(["git", "apply", str(patch)], cwd=copy)
        baseline = (copy / SOURCE).read_text()
        (output / "baseline-tcp.rs").write_text(baseline)
        command(["git", "apply", "--check", str(ROOT / "patches/scarlet" / CANDIDATE)], cwd=copy)
        command(["git", "apply", str(ROOT / "patches/scarlet" / CANDIDATE)], cwd=copy)
        command(["git", "diff", "--check"], cwd=copy)
        patched = (copy / SOURCE).read_text()
        (output / "patched-tcp.rs").write_text(patched)
        baseline_methods = extraction(baseline, False)[2]
        patched_methods = extraction(patched, True)[2]
        assert baseline_methods.count("recv_buf.pop_front().unwrap()") == 2
        assert patched_methods.count("drain_recv_buffer(&mut recv_buf, buffer, len);") == 2
        assert "pop_front" not in patched_methods
        # The method changes are exactly the two drain-loop substitutions.
        normalized = baseline_methods
        for spaces in [12, 20]:
            indent = " " * spaces
            loop = indent + "for i in 0..len {\n" + indent + "    buffer[i] = recv_buf.pop_front().unwrap();\n" + indent + "}"
            assert normalized.count(loop) == 1
            normalized = normalized.replace(loop, indent + "drain_recv_buffer(&mut recv_buf, buffer, len);")
        assert normalized == patched_methods
        for label, source, candidate in [("baseline", baseline, False), ("patched", patched, True)]:
            receipt["runs"].append(run_semantics(tmp, output, label, source, rustc, host, candidate))
            receipt["codegen"].append(codegen(tmp, output, label, source, rustc, candidate))
    after = {"source_sha256": sha(core / SOURCE), "git_status": command(["git", "status", "--porcelain"], cwd=core).stdout.decode()}
    assert before == after, "cached source changed during isolated validation"
    receipt.update(result="pass", tests_passed_before=TEST_COUNT, tests_passed_after=TEST_COUNT,
                   cached_source_unchanged=True, temporary_clone_and_binaries_removed=True,
                   exact_receive_method_only_changes_verified=True,
                   preserved_existing_limitation="recv_blocking with blocking=true, a zero-length output and a nonempty receive deque retries without sleeping; unchanged and not executed by the finite harness")
    (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()

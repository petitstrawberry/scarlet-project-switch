#!/usr/bin/env python3
"""Exercise production xHCI init/binding/MMIO code with modeled hardware.

Apply the exact deployed network patch chain in a temporary clone, compare the
unchanged default binding trace, then test the optional Tegra moderation policy.
This does not modify cached dependencies, build a kernel, or access hardware.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BASE = "6fa4a4ac2c4a1b05034057b16f614736a44344b2"
ARTIFACT = ROOT / ".cache/network-perf/imod-fix"
RELATIVE = "kernel/src/drivers/usb/xhci/mod.rs"
PATCHES = ["xhci-cooperative-waits.patch", "tcp-registry-drop-order.patch",
           "xhci-network-fairness.patch", "tcp-rx-stats-lock-scope.patch",
           "network-stage-profile.patch"]
PATCH = ROOT / "patches/scarlet/tegra-xhci-interrupt-moderation.patch"
BOARD_PATCH = ROOT / "patches/tegra210-xusb/linux-imod-policy.patch"
HARNESS = ROOT / "tests/usb-probe-qa/tegra-xhci-interrupt-moderation-host-tests.rs"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def block(source, marker):
    start = source.index(marker)
    opening = source.index("{", start)
    depth, end = 1, opening + 1
    # Selected scopes have balanced braces, including comments and strings.
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def paths(source, register_source, patched):
    constants = ("EVENT_RING_TRBS", "ERDP_EVENT_HANDLER_BUSY", "IMAN_INTERRUPT_ENABLE", "IMAN_INTERRUPT_PENDING")
    code = "\n".join(re.search(rf"^const {name}:.*?;$", source, re.MULTILINE).group(0) for name in constants)
    code += "\nmod registers {\n" + block(register_source, "pub mod runtime {") + "\n}\n"
    functions = ["const fn iman_write_value(", "fn write_mmio64_lo_hi("]
    if patched:
        functions.append("fn imod_interval_ticks(")
    functions += ["fn initialize_xhci_controller(", "pub fn bind_xhci_mmio("]
    if patched:
        functions.append("pub fn bind_xhci_mmio_with_imod_interval_ns(")
    code += "\n" + "\n\n".join(block(source, marker) for marker in functions)
    methods = ["    pub fn init("]
    if patched:
        methods.append("    fn init_with_imod_interval(")
    methods += ["    fn setup_event_ring(", "    fn read_runtime_u32(", "    fn read_runtime_u64("]
    code += "\nimpl XhciController {\n" + "\n\n".join(block(source, marker) for marker in methods) + "\n}\n"
    return code


def run(tmp, source, registers, patched):
    mode = "patched" if patched else "baseline"
    generated = paths(source, registers, patched)
    (ARTIFACT / f"{mode}-production-paths.rs").write_text(generated)
    fixture = HARNESS.read_text().replace("/* PRODUCTION_PATHS */", generated)
    rust = tmp / f"{mode}-host.rs"
    rust.write_text(fixture)
    binary = tmp / f"{mode}-host"
    command = [shutil.which("rustc"), "--edition=2024", "--test", str(rust), "-o", str(binary)]
    if patched:
        command += ["--cfg", "patched"]
    compiled = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (ARTIFACT / f"{mode}-compile.log").write_text(compiled.stdout)
    assert compiled.returncode == 0, compiled.stdout
    result = subprocess.run([str(binary), "--test-threads=1", "--nocapture"], text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10)
    (ARTIFACT / f"{mode}-tests.log").write_text(result.stdout)
    expected = 10 if patched else 3
    assert result.returncode == 0 and f"{expected} passed; 0 failed" in result.stdout, result.stdout
    trace = re.search(r"LEGACY_TRACE=(.*)\n", result.stdout).group(1)
    return {"mode": mode, "tests_passed": expected, "production_paths_sha256": sha(ARTIFACT / f"{mode}-production-paths.rs"),
            "source_sha256": hashlib.sha256(source.encode()).hexdigest(), "legacy_trace": trace,
            "compile_log": str((ARTIFACT / f"{mode}-compile.log").relative_to(ROOT)),
            "test_log": str((ARTIFACT / f"{mode}-tests.log").relative_to(ROOT))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path)
    args = parser.parse_args()
    if args.core is None:
        home = (ROOT / "projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home").resolve()
        candidates = [p for p in home.glob(f"git/checkouts/scarlet-*/{BASE[:7]}*")
                      if subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=p, text=True).strip() == BASE]
        assert len(candidates) == 1, candidates
        args.core = candidates[0]
    assert subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=args.core, text=True).strip() == BASE
    ARTIFACT.mkdir(parents=True, exist_ok=True)
    receipt = {"base_revision": BASE, "patches": {name: sha(ROOT / "patches/scarlet" / name) for name in PATCHES},
               "moderation_patch_sha256": sha(PATCH), "board_patch_sha256": sha(BOARD_PATCH),
               "harness_sha256": sha(HARNESS), "runner_sha256": sha(Path(__file__)),
               "physical_tested": False, "throughput_measured": False, "source_cache_modified": False,
               "hardware_boundaries_modeled": ["HCRST/reset", "MMIO backing", "DMA allocation/mapping/synchronization", "start/enumeration", "IRQ and registry"],
               "production_paths_executed": ["bind_xhci_mmio", "bind_xhci_mmio_with_imod_interval_ns", "imod_interval_ticks",
                                             "initialize_xhci_controller", "init", "init_with_imod_interval", "setup_event_ring",
                                             "read_runtime_u32", "read_runtime_u64", "write_mmio64_lo_hi", "iman_write_value"], "runs": []}
    cache_before = sha(args.core / RELATIVE)
    with tempfile.TemporaryDirectory(prefix="imod-validation-", dir=ARTIFACT) as directory:
        tmp = Path(directory)
        core = tmp / "core"
        subprocess.run(["git", "clone", "--quiet", "--local", "--no-hardlinks", str(args.core), str(core)], check=True)
        subprocess.run(["git", "checkout", "--quiet", "--detach", BASE], cwd=core, check=True)
        for name in PATCHES:
            subprocess.run(["git", "apply", str(ROOT / "patches/scarlet" / name)], cwd=core, check=True)
        baseline = (core / RELATIVE).read_text()
        registers = (core / "kernel/src/drivers/usb/xhci/registers.rs").read_text()
        receipt["runs"].append(run(tmp, baseline, registers, False))
        subprocess.run(["git", "apply", "--check", str(PATCH)], cwd=core, check=True)
        subprocess.run(["git", "apply", str(PATCH)], cwd=core, check=True)
        subprocess.run(["git", "diff", "--check"], cwd=core, check=True)
        patched = (core / RELATIVE).read_text()
        receipt["runs"].append(run(tmp, patched, registers, True))
        assert receipt["runs"][0]["legacy_trace"] == receipt["runs"][1]["legacy_trace"]
        assert "initialize_xhci_controller(mmio_vaddr, DmaContext::direct(), None)?" in block(patched, "fn probe_xhci(")
        # The normal pinned project continues to call the base-compatible API.
        runtime = ROOT / "drivers/usb/tegra210-xusb/src/runtime.rs"
        board_before = sha(runtime)
        board = tmp / "board"
        shutil.copytree(ROOT / "drivers/usb/tegra210-xusb", board / "drivers/usb/tegra210-xusb", ignore=shutil.ignore_patterns("target"))
        subprocess.run(["git", "init", "--quiet", str(board)], check=True)
        subprocess.run(["git", "apply", "--check", str(BOARD_PATCH)], cwd=board, check=True)
        subprocess.run(["git", "apply", str(BOARD_PATCH)], cwd=board, check=True)
        board_source = (board / "drivers/usb/tegra210-xusb/src/runtime.rs").read_text()
        assert "bind_xhci_mmio_with_imod_interval_ns(base, Some(self.xhci_irq), context, Some(40_000))?" in board_source
        assert "bind_xhci_mmio(base, Some(self.xhci_irq), context)?" in runtime.read_text()
        assert sha(runtime) == board_before
        receipt["board_runtime_original_sha256"] = board_before
        receipt["board_runtime_patched_sha256"] = sha(board / "drivers/usb/tegra210-xusb/src/runtime.rs")
        receipt["production_mod_sha256"] = sha(core / RELATIVE)
        receipt["default_trace_identical"] = True
        receipt["pci_keeps_none"] = True
        receipt["normal_pinned_board_keeps_original_api"] = True
    assert sha(args.core / RELATIVE) == cache_before
    receipt.update(result="pass", tests_passed=13, temporary_sources_and_binaries_removed=True)
    (ARTIFACT / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()

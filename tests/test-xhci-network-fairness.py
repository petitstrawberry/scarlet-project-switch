#!/usr/bin/env python3
"""Test exact production method bodies before/after the RX/TX fairness patch.

The fixture replaces MMIO, DMA mappings/rings, IRQ tokens and locks with host
models. Actual production RX and TX budget loops and their caller are extracted
unchanged from the source being tested. This is a control-flow regression test,
not a hardware, scheduler, latency or throughput benchmark.
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
HERE = ROOT / ".cache/network-perf"
BASE = "6fa4a4ac2c4a1b05034057b16f614736a44344b2"
RELATIVE = "kernel/src/drivers/usb/xhci/mod.rs"
COOPERATIVE = ROOT / "patches/scarlet/xhci-cooperative-waits.patch"
FAIRNESS = ROOT / "patches/scarlet/xhci-network-fairness.patch"
EXPECTED_COOPERATIVE = "b169f30efa94c9758d445c3a79c65b9b66d12d5dd6fa544074d6b783f22196ae"
EXPECTED_PACKAGED_MOD = "988bf4b727d244a2f5a00943e7e6a03b64a0ac42b85ec4ee060e8ff33e91391d"
EXPECTED_PACKAGED_RING = "d6397d5bf586b7b01758d838e64a56ab520861492c63fc18798d238e629e4fd7"
METHODS = (
    "take_pending_cdc_ncm_tx", "restore_cdc_ncm_tx_buffer", "process_pending_cdc_ncm_tx",
    "process_interrupt_events", "queue_interrupt_work", "process_deferred_interrupt_work",
    "complete_deferred_interrupts", "process_pending_port_change",
)
CONSTANTS = (
    "EVENT_RING_TRBS", "USBSTS_EVENT_INTERRUPT", "USBSTS_PORT_CHANGE_DETECT",
    "EP0_DCI", "XHCI_CDC_NCM_TX_WORK_BUDGET", "XHCI_CDC_NCM_TX_TRANSFER_DEPTH",
)

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def method(source, name):
    start = source.index(f"    fn {name}(")
    opening = source.index("{", start)
    # The selected bodies contain no braces in strings/comments except their
    # balanced source braces. Save exact extracted bytes in the receipt.
    depth, end = 1, opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path)
    parser.add_argument("--rustc", default=shutil.which("rustc"))
    args = parser.parse_args()
    if args.core is None:
        cargo_home = (ROOT / "projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home").resolve()
        candidates = list(cargo_home.glob(f"git/checkouts/scarlet-*/{BASE[:7]}*"))
        candidates = [p for p in candidates if subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=p, text=True).strip() == BASE]
        if len(candidates) != 1:
            parser.error(f"expected one cached core at {BASE}, found {len(candidates)}")
        args.core = candidates[0]
    if not args.rustc:
        parser.error("rustc is required")
    assert sha(COOPERATIVE) == EXPECTED_COOPERATIVE
    HERE.mkdir(parents=True, exist_ok=True)
    harness = ROOT / "tests/usb-probe-qa/xhci-network-fairness-host-tests.rs"
    result = {
        "base_revision": BASE, "cooperative_patch_sha256": sha(COOPERATIVE),
        "fairness_patch_sha256": sha(FAIRNESS), "harness_sha256": sha(harness),
        "command": ["python3", str(Path(__file__).relative_to(ROOT))],
        "methods_executed_from_production": list(METHODS),
        "host_modeled_boundaries": ["MMIO", "DMA mappings/cache synchronization", "rings", "IRQ tokens", "locks"],
        "physical_tested": False, "scheduler_tested": False, "throughput_measured": False,
        "source_cache_modified": False, "runs": [],
    }
    with tempfile.TemporaryDirectory(prefix="xhci-fairness-validation-", dir=HERE) as tmp:
        tmp = Path(tmp)
        subprocess.run(["git", "init", "--quiet", str(tmp)], check=True)
        for name in ("mod.rs", "ring.rs"):
            relative = f"kernel/src/drivers/usb/xhci/{name}"
            target = tmp / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(subprocess.check_output(["git", "show", f"{BASE}:{relative}"], cwd=args.core))
        subprocess.run(["git", "apply", "--check", str(COOPERATIVE)], cwd=tmp, check=True)
        subprocess.run(["git", "apply", str(COOPERATIVE)], cwd=tmp, check=True)
        assert sha(tmp / RELATIVE) == EXPECTED_PACKAGED_MOD
        assert sha(tmp / "kernel/src/drivers/usb/xhci/ring.rs") == EXPECTED_PACKAGED_RING
        for mode in ("packaged_baseline", "fairness_patched"):
            if mode == "fairness_patched":
                subprocess.run(["git", "apply", "--check", str(FAIRNESS)], cwd=tmp, check=True)
                subprocess.run(["git", "apply", str(FAIRNESS)], cwd=tmp, check=True)
            source = (tmp / RELATIVE).read_text()
            constants = "\n".join(re.search(rf"^const {name}:.*?;$", source, re.MULTILINE).group(0) for name in CONSTANTS)
            bodies = "\n\n".join(method(source, name) for name in METHODS)
            extracted = tmp / "xhci-network-production-methods.rs"
            extracted.write_text(constants + "\nimpl XhciController {\n" + bodies + "\n}\n")
            # Preserve the tested bodies and hashes after the temporary checkout
            # and test binary are removed.
            shutil.copyfile(extracted, HERE / f"xhci-network-{mode}-methods.rs")
            shutil.copyfile(harness, tmp / "tests.rs")
            compile_command = [str(args.rustc), "--test", "--edition=2024", str(tmp / "tests.rs"), "-o", str(tmp / "tests")]
            compiled = subprocess.run(compile_command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            assert compiled.returncode == 0, compiled.stdout
            tested = subprocess.run([str(tmp / "tests"), "--test-threads=1", "--nocapture"],
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10)
            log = HERE / f"xhci-network-{mode}-tests.log"
            log.write_text(compiled.stdout + tested.stdout)
            run = {
                "mode": mode, "source_sha256": sha(tmp / RELATIVE),
                "ring_sha256": sha(tmp / "kernel/src/drivers/usb/xhci/ring.rs"),
                "extracted_methods_sha256": sha(extracted), "returncode": tested.returncode,
                "test_log": str(log.relative_to(ROOT)),
            }
            result["runs"].append(run)
            if mode == "packaged_baseline":
                assert tested.returncode != 0 and "full RX budget starved queued TX" in tested.stdout, tested.stdout
                assert "3 passed; 5 failed" in tested.stdout, tested.stdout
                result["baseline_full_rx_tx_starvation_reproduced"] = True
            else:
                assert tested.returncode == 0 and "8 passed; 0 failed" in tested.stdout, tested.stdout
                result["patched_tests_passed"] = 8
    result.update(result="pass", temporary_source_and_test_binaries_removed=True)
    (HERE / "xhci-network-fairness-result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))

if __name__ == "__main__":
    main()

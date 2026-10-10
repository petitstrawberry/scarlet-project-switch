#!/usr/bin/env python3
"""Execute exact RX claim/completion, TX enqueue and ring methods with modeled hardware.

Only the CDC-NCM IN branch of handle_transfer_event is selected; unrelated HID,
notification, and TX-completion dispatch branches are excluded from this fixture.
No cached dependency, kernel build, or remote device is modified by this test.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BASE = "6fa4a4ac2c4a1b05034057b16f614736a44344b2"
BASELINE_MOD_SHA = "e82a54f262ba9dae98abc0a70b6d05c32eaa2e992ab978925fc653efa1371f16"
RELATIVE = "kernel/src/drivers/usb/xhci/mod.rs"
ARTIFACT = ROOT / ".cache/network-perf/rx-lock-scope"
PATCHES = ["xhci-cooperative-waits.patch", "tcp-registry-drop-order.patch", "xhci-network-fairness.patch",
           "tcp-rx-stats-lock-scope.patch", "network-stage-profile.patch", "tegra-xhci-interrupt-moderation.patch"]
PATCH = ROOT / "patches/scarlet/xhci-ncm-rx-lock-scope.patch"
HARNESS = ROOT / "tests/usb-probe-qa/xhci-ncm-rx-lock-scope-host-tests.rs"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def block(source, marker):
    start = source.index(marker)
    opening = source.index("{", start)
    depth, end = 1, opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def production_paths(source, ring, trb):
    method = block(source, "    fn handle_transfer_event(")
    prefix = method[:method.index("        if let Some(ncm)")]
    rx = block(method, "        if let Some(ncm) = slot.cdc_ncm.as_mut()\n            && ncm.bulk_in.dci == endpoint_id")
    claim = prefix + rx + "\n        false\n    }"
    structs = ["struct CdcNcmDmaBuffer {", "struct InFlightCdcNcmRx {", "struct CompletedCdcNcmRx {", "struct QueuedCdcNcmTx {"]
    functions = ["fn sync_pages_before_device_write(", "fn sync_pages_after_device_write("]
    methods = ["    fn transfer_successful(", "    fn complete_cdc_ncm_rx(", "    fn enqueue_cdc_ncm_tx("]
    return (trb[:trb.index("#[cfg(test)]")] + "\n" +
            "\n\n".join(block(source, marker) for marker in structs + functions) +
            "\n" + block(ring, "pub struct DmaTrbRing {") + "\n" + block(ring, "impl DmaTrbRing {") +
            "\nimpl XhciController {\n" + claim + "\n" + "\n\n".join(block(source, m) for m in methods) + "\n}\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path)
    args = parser.parse_args()
    if args.core is None:
        home = ROOT / "projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home"
        candidates = [p for p in home.glob(f"git/checkouts/scarlet-*/{BASE[:7]}*")
                      if subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=p, text=True).strip() == BASE]
        assert len(candidates) == 1, candidates
        args.core = candidates[0]
    assert subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=args.core, text=True).strip() == BASE
    ARTIFACT.mkdir(parents=True, exist_ok=True)
    original_hash = sha(args.core / RELATIVE)
    receipt = {"base_revision": BASE, "input_patches": {name: sha(ROOT / "patches/scarlet" / name) for name in PATCHES},
               "patch_sha256": sha(PATCH), "runner_sha256": sha(Path(__file__)), "harness_sha256": sha(HARNESS),
               "physical_tested": False, "throughput_measured": False,
               "modeled_boundaries": ["registry IRQ masking (host mutex)", "DMA mapping and allocation", "cache handoff barriers", "doorbell and device callbacks"],
               "actual_production_paths": ["handle_transfer_event initial lookup and CDC-NCM IN branch", "complete_cdc_ncm_rx",
                   "enqueue_cdc_ncm_tx", "transfer_successful", "CdcNcmDmaBuffer declaration and field drop order",
                   "sync_pages_before_device_write", "sync_pages_after_device_write", "DmaTrbRing methods including enqueue/link/wrap", "Trb constructors and decoding"],
               "profile_scope": "XhciRxRequeue still spans full cache preparation and ring publication, now including registry reacquisition wait; excludes doorbell and NCM delivery. Old/new elapsed means are not identical scopes.",
               "unchanged_limits": {"rx_depth": 8, "cache_preparation": "full allocation", "owned_ntb_copy": True},
               "lifecycle_limitations": ["existing unlocked doorbell window", "existing delivery to captured device after disconnect", "does not prove hardware coherency or general concurrent enumeration safety"]}
    with tempfile.TemporaryDirectory(prefix="rx-scope-validation-", dir=ARTIFACT) as directory:
        tmp = Path(directory); core = tmp / "core"
        subprocess.run(["git", "clone", "--quiet", "--local", "--no-hardlinks", str(args.core.resolve()), str(core)], check=True)
        subprocess.run(["git", "checkout", "--quiet", "--detach", BASE], cwd=core, check=True)
        for name in PATCHES:
            subprocess.run(["git", "apply", str(ROOT / "patches/scarlet" / name)], cwd=core, check=True)
        before = (core / RELATIVE).read_text()
        (ARTIFACT / "baseline-mod.rs").write_text(before)
        subprocess.run(["git", "apply", "--check", str(PATCH)], cwd=core, check=True)
        subprocess.run(["git", "apply", str(PATCH)], cwd=core, check=True)
        subprocess.run(["git", "diff", "--check"], cwd=core, check=True)
        after = (core / RELATIVE).read_text()
        assert after.count("struct CompletedCdcNcmRx {") == 1
        assert after.count("    fn complete_cdc_ncm_rx(") == 1
        assert hashlib.sha256(before.encode()).hexdigest() == BASELINE_MOD_SHA
        assert block(before, "    fn enqueue_cdc_ncm_tx(") == block(after, "    fn enqueue_cdc_ncm_tx(")
        assert block(before, "fn sync_pages_before_device_write(") == block(after, "fn sync_pages_before_device_write(")
        assert block(before, "fn sync_pages_after_device_write(") == block(after, "fn sync_pages_after_device_write(")
        (ARTIFACT / "patched-mod.rs").write_text(after)
        ring = (core / "kernel/src/drivers/usb/xhci/ring.rs").read_text()
        trb = (core / "kernel/src/drivers/usb/xhci/trb.rs").read_text()
        generated = production_paths(after, ring, trb)
        (ARTIFACT / "production-paths.rs").write_text(generated)
        rust = tmp / "host.rs"; binary = tmp / "host-tests"
        rust.write_text(HARNESS.read_text().replace("/* PRODUCTION_PATHS */", generated))
        command = [shutil.which("rustc"), "--edition=2024", "--test", str(rust), "-o", str(binary)]
        result = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        (ARTIFACT / "compile.log").write_text(result.stdout)
        assert result.returncode == 0, result.stdout
        result = subprocess.run([str(binary), "--test-threads=1", "--nocapture"], text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
        (ARTIFACT / "tests.log").write_text(result.stdout)
        assert result.returncode == 0 and "14 passed; 0 failed" in result.stdout, result.stdout
        receipt.update(tests_passed=14, production_mod_sha256=sha(core / RELATIVE),
                       production_paths_sha256=sha(ARTIFACT / "production-paths.rs"),
                       ring_source_sha256=hashlib.sha256(ring.encode()).hexdigest(),
                       trb_source_sha256=hashlib.sha256(trb.encode()).hexdigest(),
                       actual_tx_enqueue_unchanged=True, full_cache_helpers_unchanged=True)
    assert sha(args.core / RELATIVE) == original_hash
    receipt.update(result="pass", source_cache_unchanged=True, temporary_clone_and_binary_removed=True)
    (ARTIFACT / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()

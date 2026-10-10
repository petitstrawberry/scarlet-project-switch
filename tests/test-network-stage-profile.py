#!/usr/bin/env python3
"""Test opt-in profiling using exact production profiler/device/NCM/TX methods.

A fresh temporary source copy applies the four packaged network patches and the
profiling patch. Host models replace timer, task wakeups, diagnostic registry,
locks, MMIO and DMA/cache boundaries. Real NCM parsing/copy/enqueue and TX copy
loops execute. No dependency checkout, hardware, kernel artifact or SD is edited.
"""
import argparse
import hashlib
import runpy
from types import SimpleNamespace
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BASE = "6fa4a4ac2c4a1b05034057b16f614736a44344b2"
ARTIFACT = ROOT / ".cache/network-perf/stage-profile"
PATCHES = ["xhci-cooperative-waits.patch", "tcp-registry-drop-order.patch",
           "xhci-network-fairness.patch", "tcp-rx-stats-lock-scope.patch",
           "network-stage-profile.patch"]
FILES = ["kernel/src/drivers/special/mod.rs", "kernel/src/drivers/special/net_profile.rs",
         "kernel/src/network/profile.rs", "kernel/src/network/mod.rs",
         "kernel/src/network/tcp.rs", "kernel/src/drivers/usb/cdc_ncm.rs",
         "kernel/src/drivers/usb/xhci/mod.rs", "kernel/src/drivers/usb/xhci/ring.rs"]

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def block(source, marker):
    start = source.index(marker)
    opening = source.index("{", start)
    depth, end = 1, opening + 1
    # Selected production scopes have balanced braces in comments/strings.
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]

def run_host(tmp, name, source, expected):
    path = tmp / f"{name}.rs"
    path.write_text(source)
    binary = tmp / name
    command = [shutil.which("rustc"), "--test", "--edition=2024", str(path), "-o", str(binary)]
    compiled = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (ARTIFACT / f"{name}-compile.log").write_text(compiled.stdout)
    assert compiled.returncode == 0, compiled.stdout
    result = subprocess.run([str(binary), "--test-threads=1", "--nocapture"], text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10)
    (ARTIFACT / f"{name}-tests.log").write_text(result.stdout)
    assert result.returncode == 0 and f"{expected} passed; 0 failed" in result.stdout, result.stdout
    shutil.copyfile(path, ARTIFACT / path.name)
    return {"name": name, "tests_passed": expected, "extracted_harness_sha256": sha(path),
            "log": str((ARTIFACT / f"{name}-tests.log").relative_to(ROOT))}

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
    ARTIFACT.mkdir(parents=True, exist_ok=True)
    receipt = {"base_revision": BASE, "patches": {name: sha(ROOT / "patches/scarlet" / name) for name in PATCHES},
               "harness_sha256": sha(ROOT / "tests/usb-probe-qa/network-stage-profile-host-tests.rs"),
               "runner_sha256": sha(Path(__file__)),
               "xhci_boundary_models_sha256": sha(ROOT / "tests/usb-probe-qa/xhci-network-fairness-host-tests.rs"),
               "physical_tested": False, "scheduler_tested": False, "throughput_measured": False,
               "source_cache_modified": False, "runs": []}
    with tempfile.TemporaryDirectory(prefix="network-profile-validation-", dir=ARTIFACT) as directory:
        tmp = Path(directory)
        core = tmp / "core"
        subprocess.run(["git", "clone", "--quiet", "--local", "--no-hardlinks", str(args.core), str(core)], check=True)
        subprocess.run(["git", "checkout", "--quiet", "--detach", BASE], cwd=core, check=True)
        for name in PATCHES:
            patch = ROOT / "patches/scarlet" / name
            subprocess.run(["git", "apply", "--check", str(patch)], cwd=core, check=True)
            subprocess.run(["git", "apply", str(patch)], cwd=core, check=True)
        subprocess.run(["git", "diff", "--check"], cwd=core, check=True)
        receipt["source_hashes"] = {relative: sha(core / relative) for relative in FILES}
        profile_source = (core / "kernel/src/network/profile.rs").read_text()
        # Preserve exact production sources of new, normally untracked files.
        for relative in ["kernel/src/network/profile.rs", "kernel/src/drivers/special/net_profile.rs"]:
            shutil.copyfile(core / relative, ARTIFACT / Path(relative).name)
        # Relative generated source paths keep preserved harness hashes stable.
        (tmp / "network").mkdir()
        shutil.copyfile(core / "kernel/src/network/profile.rs", tmp / "network/profile.rs")
        module = "mod network { pub(crate) mod profile; }"
        device_source = (core / "kernel/src/drivers/special/net_profile.rs").read_text()
        device_file = tmp / "diagnostic.rs"
        device_file.write_text(device_source + '\npub fn device() -> impl crate::device::char::CharDevice { NetworkProfileDevice { snapshot: crate::sync::IrqSpinLock::new(String::new()) } }\n')
        cdc_source = (core / "kernel/src/drivers/usb/cdc_ncm.rs").read_text()
        constants = ["NTH16_SIGNATURE", "NDP16_NO_CRC_SIGNATURE", "NTH16_LENGTH", "NDP16_ONE_DATAGRAM_LENGTH",
                     "ETHERNET_HEADER_LENGTH", "NCM_RX_QUEUE_LIMIT", "NCM_MAX_DATAGRAMS_PER_NTB"]
        functions = ["parse_ntb16", "read_u16", "read_u32", "write_u16", "write_u32",
                     "drain_queued_rx_packets", "enqueue_rx_packets"]
        actual = "\n".join(re.search(rf"^const {name}:.*?;$", cdc_source, re.MULTILINE).group(0) for name in constants)
        actual += "\n" + block(cdc_source, "struct QueuedRxPacket {")
        actual += "\n" + "\n\n".join(block(cdc_source, f"fn {name}(") for name in functions)
        actual += "\nimpl CdcNcmDevice {\n" + block(cdc_source, "    pub fn handle_received_ntb(") + "\n}\n"
        (ARTIFACT / "production-ncm-paths.rs").write_text(actual)
        harness = (ROOT / "tests/usb-probe-qa/network-stage-profile-host-tests.rs").read_text()
        harness = harness.replace("/* PROFILE_MODULE */", module).replace("/* PROFILE_DEVICE */", '#[path="diagnostic.rs"] mod diagnostic;')
        harness = harness.replace("/* PRODUCTION_NCM_PATHS */", actual)
        receipt["runs"].append(run_host(tmp, "profile-ncm-host", harness, 9))

        # Reuse the durable, already reviewed xHCI boundary models, not their
        # method bodies. Extract all eight production methods from this patch.
        fairness = SimpleNamespace(**runpy.run_path(str(ROOT / "tests/test-xhci-network-fairness.py")))
        xhci_source = (core / "kernel/src/drivers/usb/xhci/mod.rs").read_text()
        constants = "\n".join(re.search(rf"^const {name}:.*?;$", xhci_source, re.MULTILINE).group(0) for name in fairness.CONSTANTS)
        methods = "\n\n".join(fairness.method(xhci_source, name) for name in fairness.METHODS)
        extracted = constants + "\nimpl XhciController {\n" + methods + "\n}\n"
        (tmp / "xhci-network-production-methods.rs").write_text(extracted)
        (ARTIFACT / "production-xhci-paths.rs").write_text(extracted)
        xhci_harness = (ROOT / "tests/usb-probe-qa/xhci-network-fairness-host-tests.rs").read_text()
        xhci_harness += '\n' + module + '\nuse crate::network::profile as net_profile;\nmod timer { pub fn get_time_ns() -> u64 { 1000 } }\n'
        xhci_harness += r'''
#[test]
fn profile_off_production_tx_has_identical_payload_and_no_counts() {
    net_profile::command(b"0").unwrap(); let a=net_profile::snapshot();
    let c=controller(0,1,8,false,true,1); assert_eq!(c.process_pending_cdc_ncm_tx(),8);
    assert_eq!(net_profile::snapshot(),a);
    for tx in &c.slot_runtime.lock()[0].cdc_ncm.as_ref().unwrap().tx_in_flight {
        assert_eq!(&tx.buffer.pages.bytes()[..64], &[0x5a;64]); assert_eq!(tx.transfer_len,64);
    }
}
#[test]
fn profile_on_production_tx_reports_actual_copy_and_entire_cache_capacity() {
    net_profile::command(b"1").unwrap(); let a=net_profile::snapshot();
    let c=controller(EVENT_RING_TRBS,1,8,false,true,1); assert!(c.process_deferred_interrupt_work());
    let b=net_profile::snapshot();
    let copy=net_profile::Stage::XhciTxCopy as usize; let clean=net_profile::Stage::XhciTxClean as usize;
    assert_eq!(b[copy].calls-a[copy].calls,8); assert_eq!(b[copy].bytes-a[copy].bytes,8*64);
    assert_eq!(b[copy].capacity-a[copy].capacity,0);
    assert_eq!(b[clean].calls-a[clean].calls,8); assert_eq!(b[clean].bytes-a[clean].bytes,8*64);
    assert_eq!(b[clean].capacity-a[clean].capacity,8*16384);
    let budget=net_profile::Stage::XhciFullEventBudget as usize;
    assert_eq!(b[budget].calls-a[budget].calls,1);
    assert_eq!(in_flight(&c),8); assert_eq!(c.completion_count.load(Ordering::Relaxed),0);
    net_profile::command(b"0").unwrap();
}
#[test]
fn profile_production_rejected_oversized_tx_does_not_count_a_copy_or_clean() {
    net_profile::command(b"1").unwrap(); let a=net_profile::snapshot();
    let c=controller(0,1,40,true,true,1);
    assert_eq!(c.process_pending_cdc_ncm_tx(),XHCI_CDC_NCM_TX_WORK_BUDGET);
    let b=net_profile::snapshot();
    for stage in [net_profile::Stage::XhciTxCopy,net_profile::Stage::XhciTxClean] {
        assert_eq!(a[stage as usize],b[stage as usize]);
    }
    assert_eq!(queued(&c),8); assert_eq!(in_flight(&c),0); net_profile::command(b"0").unwrap();
}
'''
        receipt["runs"].append(run_host(tmp, "profile-xhci-host", xhci_harness, 11))
        # Integration guards supplement the executable modeled-boundary tests.
        assert "pub mod net_profile;" in (core / "kernel/src/drivers/special/mod.rs").read_text()
        assert "pub(crate) mod profile;" in (core / "kernel/src/network/mod.rs").read_text()
        assert "profile::Stage::StackDispatch" in (core / "kernel/src/network/mod.rs").read_text()
        assert (core / "kernel/src/network/tcp.rs").read_text().count("net_profile::Stage::TcpDrain") == 2
        assert "net_profile::Stage::TcpReceive" in (core / "kernel/src/network/tcp.rs").read_text()
        assert "net_profile::Stage::XhciRxInvalidate" in xhci_source
        assert "net_profile::Stage::XhciRxCopy" in xhci_source
        assert "net_profile::Stage::XhciRxRequeue" in xhci_source
        receipt["integration_guards_passed"] = True
    receipt.update(result="pass", tests_passed=20, temporary_clone_and_test_binaries_removed=True)
    (ARTIFACT / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))

if __name__ == "__main__":
    main()

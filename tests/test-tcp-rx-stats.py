#!/usr/bin/env python3
"""Verify the production TCP RX method drops its statistics guard before callbacks.

Copy the exact pinned TCP source into a temporary directory, apply the existing
registry safety patch, extract receive_segment, and compile it in an instrumented
host harness. Four guard-lifetime checks must fail before the stats-scope patch
and all seven tests must pass afterward. No dependency checkout is modified.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


PIN = "6fa4a4ac2c4a1b05034057b16f614736a44344b2"
ROOT = Path(__file__).resolve().parents[1]
RELATIVE_SOURCE = Path("kernel/src/network/tcp.rs")

HARNESS = r'''
use std::cell::{Cell, RefCell, RefMut};
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Check { None, Lookup, Processing, Logging }

thread_local! {
    static HELD: Cell<bool> = const { Cell::new(false) };
    static CHECK: Cell<Check> = const { Cell::new(Check::None) };
    static LOGS: Cell<usize> = const { Cell::new(0) };
}

fn checkpoint(phase: Check) {
    if CHECK.with(|check| check.get() == phase) {
        assert!(!HELD.with(Cell::get), "statistics guard spans callback");
    }
}

fn logged(_args: std::fmt::Arguments<'_>) {
    checkpoint(Check::Logging);
    LOGS.with(|logs| logs.set(logs.get() + 1));
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => { $crate::logged(format_args!($($arg)*)) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stats { packets_received: u64, bytes_received: u64 }

struct StatsLock {
    data: RefCell<Stats>,
    writes: Cell<usize>,
    drops: Cell<usize>,
}

struct StatsGuard<'a> { lock: &'a StatsLock, value: RefMut<'a, Stats> }

impl StatsLock {
    fn write(&self) -> StatsGuard<'_> {
        assert!(!HELD.with(|held| held.replace(true)), "recursive stats guard");
        self.writes.set(self.writes.get() + 1);
        StatsGuard { lock: self, value: self.data.borrow_mut() }
    }
}

impl Deref for StatsGuard<'_> {
    type Target = Stats;
    fn deref(&self) -> &Stats { &self.value }
}

impl DerefMut for StatsGuard<'_> {
    fn deref_mut(&mut self) -> &mut Stats { &mut self.value }
}

impl Drop for StatsGuard<'_> {
    fn drop(&mut self) {
        HELD.with(|held| assert!(held.replace(false)));
        self.lock.drops.set(self.lock.drops.get() + 1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ipv4Address([u8; 4]);

#[derive(Clone, Copy)]
struct TcpHeader {
    src_port: u16, dst_port: u16, seq_number: u32, ack_number: u32,
}

impl TcpHeader {
    fn data_offset(&self) -> usize { 24 }
    fn flags(&self) -> u8 { 0x10 }
}

fn should_log_tcp_https(src: u16, dst: u16) -> bool { src == 443 || dst == 443 }

struct TcpLayer {
    stats: StatsLock,
    matched: bool,
    lookups: Cell<usize>,
    processed: Cell<usize>,
}

struct Socket<'a>(&'a TcpLayer);

impl Socket<'_> {
    fn process_segment(&self, src: Ipv4Address, dst: Ipv4Address, header: TcpHeader,
                       data: &[u8], mss: Option<u16>) {
        checkpoint(Check::Processing);
        assert_eq!(src, Ipv4Address([192, 168, 0, 16]));
        assert_eq!(dst, Ipv4Address([192, 168, 0, 35]));
        assert_eq!(header.dst_port, 22);
        assert_eq!(data, b"payload");
        assert_eq!(mss, Some(1460));
        self.0.processed.set(self.0.processed.get() + 1);
    }
}

impl TcpLayer {
    fn find_socket(&self, dst: u16, src_ip: Ipv4Address, src: u16) -> Option<Socket<'_>> {
        checkpoint(Check::Lookup);
        assert_eq!(dst, 22);
        assert_eq!(src_ip, Ipv4Address([192, 168, 0, 16]));
        assert!(src == 50000 || src == 443);
        self.lookups.set(self.lookups.get() + 1);
        self.matched.then_some(Socket(self))
    }

    /* EXTRACTED_PRODUCTION_RECEIVE_SEGMENT */
}

fn run(check: Check, matched: bool, logging: bool) {
    HELD.with(|held| held.set(false));
    CHECK.with(|current| current.set(check));
    LOGS.with(|logs| logs.set(0));
    let layer = TcpLayer {
        stats: StatsLock {
            data: RefCell::new(Stats { packets_received: 100, bytes_received: 4096 }),
            writes: Cell::new(0), drops: Cell::new(0),
        },
        matched, lookups: Cell::new(0), processed: Cell::new(0),
    };
    layer.receive_segment(
        Ipv4Address([192, 168, 0, 16]), Ipv4Address([192, 168, 0, 35]),
        TcpHeader { src_port: if logging { 443 } else { 50000 }, dst_port: 22,
                    seq_number: 7, ack_number: 11 },
        b"payload", Some(1460),
    );
    assert!(!HELD.with(Cell::get));
    assert_eq!(*layer.stats.data.borrow(), Stats { packets_received: 101, bytes_received: 4127 });
    assert_eq!(layer.stats.writes.get(), 1);
    assert_eq!(layer.stats.drops.get(), 1);
    assert_eq!(layer.lookups.get(), 1);
    assert_eq!(layer.processed.get(), usize::from(matched));
    assert_eq!(LOGS.with(Cell::get), usize::from(!matched && logging));
}

#[test]
fn lookup_guard_released_matched() { run(Check::Lookup, true, false); }
#[test]
fn lookup_guard_released_unmatched() { run(Check::Lookup, false, false); }
#[test]
fn processing_guard_released() { run(Check::Processing, true, false); }
#[test]
fn logging_guard_released() { run(Check::Logging, false, true); }
#[test]
fn matched_counters_increment_once() { run(Check::None, true, false); }
#[test]
fn unmatched_counters_increment_once() { run(Check::None, false, false); }
#[test]
fn unmatched_logging_counters_increment_once() { run(Check::None, false, true); }
'''


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def command(args, *, cwd=None):
    return subprocess.run(args, cwd=cwd, check=True, capture_output=True, timeout=20)


def extract_method(source):
    marker = "    pub fn receive_segment("
    if source.count(marker) != 1:
        raise RuntimeError("expected exactly one production receive_segment method")
    start = source.index(marker)
    opening = source.index("{", start)
    depth = 0
    for position in range(opening, len(source)):
        if source[position] == "{":
            depth += 1
        elif source[position] == "}":
            depth -= 1
            if depth == 0:
                return source[start:position + 1]
    raise RuntimeError("unterminated production receive_segment method")


def run_harness(temporary, output, label, source, rustc, host):
    method = extract_method(source)
    harness = HARNESS.replace("    /* EXTRACTED_PRODUCTION_RECEIVE_SEGMENT */", method)
    rust_source = temporary / (label + ".rs")
    executable = temporary / label
    rust_source.write_text(harness)
    (output / (label + "-method.rs")).write_text(method + "\n")
    (output / (label + "-harness.rs")).write_text(harness)
    compiled = command([rustc, "--edition=2024", "--target", host, "--test",
                        str(rust_source), "-o", str(executable)])
    (output / (label + "-compile.log")).write_bytes(compiled.stdout + compiled.stderr)
    tested = subprocess.run([str(executable), "--test-threads=1", "--nocapture"],
                            capture_output=True, timeout=20)
    result = tested.stdout + tested.stderr
    (output / (label + "-tests.log")).write_bytes(result)
    return {"returncode": tested.returncode, "source_sha256": sha256(source.encode()),
            "method_sha256": sha256(method.encode()), "harness_sha256": sha256(harness.encode()),
            "test_log_sha256": sha256(result)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, default=Path.home() / ".cargo/git/checkouts/scarlet-26bf9663864ed506/6fa4a4a")
    parser.add_argument("--output", type=Path, default=ROOT / ".cache/network-perf/tcp-rx-stats")
    args = parser.parse_args()
    core = args.core.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    revision = command(["git", "rev-parse", "HEAD"], cwd=core).stdout.decode().strip()
    if revision != PIN:
        raise RuntimeError(f"expected core revision {PIN}, found {revision}")
    base = command(["git", "show", "HEAD:" + RELATIVE_SOURCE.as_posix()], cwd=core).stdout
    safety = ROOT / "patches/scarlet/tcp-registry-drop-order.patch"
    scope = ROOT / "patches/scarlet/tcp-rx-stats-lock-scope.patch"
    rustc = shutil.which(os.environ.get("RUSTC", "rustc"))
    if rustc is None:
        raise RuntimeError("rustc is required for the focused host harness")
    compiler = command([rustc, "-vV"]).stdout.decode()
    host = next(line.removeprefix("host: ") for line in compiler.splitlines() if line.startswith("host: "))
    receipt = {"core_revision": revision, "base_source_sha256": sha256(base),
               "safety_patch_sha256": sha256(safety.read_bytes()),
               "stats_patch_sha256": sha256(scope.read_bytes()), "compiler": compiler,
               "host_target": host, "production_source_modified": False}
    with tempfile.TemporaryDirectory(prefix="scarlet-tcp-rx-stats-") as directory:
        temporary = Path(directory)
        receipt["temporary_directory"] = str(temporary)
        copied_source = temporary / RELATIVE_SOURCE
        copied_source.parent.mkdir(parents=True)
        copied_source.write_bytes(base)
        command(["git", "apply", "--check", str(safety)], cwd=temporary)
        command(["git", "apply", str(safety)], cwd=temporary)
        baseline_source = copied_source.read_text()
        receipt["baseline"] = run_harness(temporary, output, "baseline", baseline_source, rustc, host)
        command(["git", "apply", "--check", str(scope)], cwd=temporary)
        command(["git", "apply", str(scope)], cwd=temporary)
        patched_source = copied_source.read_text()
        receipt["patched"] = run_harness(temporary, output, "patched", patched_source, rustc, host)
        baseline_log = (output / "baseline-tests.log").read_text()
        patched_log = (output / "patched-tests.log").read_text()
        if receipt["baseline"]["returncode"] == 0 or "3 passed; 4 failed" not in baseline_log:
            raise RuntimeError("baseline did not demonstrate the four expected guard-lifetime failures")
        if receipt["patched"]["returncode"] != 0 or "7 passed; 0 failed" not in patched_log:
            raise RuntimeError("patched production method did not pass all seven focused checks")
        receipt["result"] = "PASS: baseline 3 passed/4 failed; patched 7 passed/0 failed"
    receipt["temporary_directory_removed"] = not Path(receipt["temporary_directory"]).exists()
    if not receipt["temporary_directory_removed"]:
        raise RuntimeError("temporary validation directory was not removed")
    (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()

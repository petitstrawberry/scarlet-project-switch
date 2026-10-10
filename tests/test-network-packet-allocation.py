#!/usr/bin/env python3
"""Compare extracted packet code and exercise the actual bounded xHCI worker.

Temporary pinned checkouts are removed, with results preserved separately.
Allocation counts use host System, not Talc; they establish avoided requests,
not a speedup or physical IRQ timing. No hardware or dependency cache edits.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import runpy
import shutil
import subprocess
import tempfile

ROOT=Path(__file__).resolve().parents[1]
BASE="6fa4a4ac2c4a1b05034057b16f614736a44344b2"
PATCHES=["xhci-cooperative-waits.patch","tcp-registry-drop-order.patch",
         "xhci-network-fairness.patch","tcp-rx-stats-lock-scope.patch",
         "network-stage-profile.patch","tegra-xhci-interrupt-moderation.patch",
         "tcp-bulk-receive-drain.patch","xhci-ncm-rx-lock-scope.patch"]
NEW=["network-packet-allocation.patch","xhci-worker-batching.patch"]
block=runpy.run_path(str(ROOT/"tests/test-tcp-bulk-receive-drain.py"))["block"]

def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def run(a,**kw):return subprocess.run(a,check=True,capture_output=True,text=True,timeout=60,**kw)

def extract(sources,patched):
    s=sources["protocol_stack.rs"]
    start=s.index("// Protocol-neutral inline metadata.") if patched else s.index("#[derive(Debug, Clone, Default)]\npub struct LayerContext")
    result="use alloc::{collections::BTreeMap,string::String,vec::Vec};\n"+s[start:s.index("/// Configuration for socket creation",start)]
    result+="\nconst TCP_HEADER_SIZE:usize=20;\n"
    for file,ty,methods in [("tcp.rs","TcpHeader",["calculate_checksum_with_options","to_bytes"]),("ipv4.rs","Ipv4Header",["new","calculate_checksum","to_bytes"])]:
        source=sources[file];body=block(source,f"impl {ty} {{")
        result+="#[derive(Debug,Clone,Copy)]\n#[repr(C,packed)]\n"+block(source,f"pub struct {ty} {{")+"\nimpl "+ty+" {\n"
        for name in methods+(["to_array"] if patched else []):
            marker=f"    pub fn {name}(" if f"    pub fn {name}(" in body else f"    fn {name}("
            result+=block(body,marker).replace(f"    fn {name}(",f"    pub fn {name}(")+"\n"
        result+="}\n"
    result+=block(sources["ipv4.rs"],"fn checksum_from_bytes(")
    return result

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--core",type=Path,default=ROOT/"projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home/git/checkouts/scarlet-26bf9663864ed506/6fa4a4a")
    p.add_argument("--output",type=Path,default=ROOT/".cache/network-perf/network-hotpath/validation")
    args=p.parse_args();core=args.core.resolve();out=args.output.resolve();out.mkdir(parents=True,exist_ok=True)
    assert run(["git","rev-parse","HEAD"],cwd=core).stdout.strip()==BASE
    paths=["kernel/src/network/"+n for n in ["tcp.rs","ipv4.rs","ethernet.rs","protocol_stack.rs"]]+["kernel/src/drivers/usb/xhci/mod.rs"]
    before={n:sha(core/n) for n in paths};status=run(["git","status","--porcelain"],cwd=core).stdout
    rustc=shutil.which("rustc");version=run([rustc,"-vV"]).stdout;host=re.search(r"^host: (.+)$",version,re.M).group(1)
    receipt={"base_revision":BASE,"runner_sha256":sha(Path(__file__)),"rustc":version,"patches":{n:sha(ROOT/"patches/scarlet"/n) for n in PATCHES+NEW},"runs":[],"physical_tested":False}
    with tempfile.TemporaryDirectory(prefix="network-hotpath-",dir=out) as directory:
        tmp=Path(directory);copy=tmp/"core"
        run(["git","clone","--quiet","--local","--no-hardlinks",str(core),str(copy)])
        run(["git","checkout","--quiet","--detach",BASE],cwd=copy)
        for name in PATCHES:
            run(["git","apply",str(ROOT/"patches/scarlet"/name)],cwd=copy)
        originals={Path(n).name:(copy/n).read_text() for n in paths[:-1]}
        for name in NEW:
            run(["git","apply","--check",str(ROOT/"patches/scarlet"/name)],cwd=copy)
            run(["git","apply",str(ROOT/"patches/scarlet"/name)],cwd=copy)
        run(["git","diff","--check"],cwd=copy)
        changed={Path(n).name:(copy/n).read_text() for n in paths[:-1]}
        receipt["source_sha256"]={n:sha(copy/n) for n in paths}
        for n in paths:(out/Path(n).name).write_text((copy/n).read_text())
        harness=ROOT/"tests/usb-probe-qa/network-packet-allocation-host-tests.rs"
        test=harness.read_text().replace("/* BASELINE */","mod baseline {\n"+extract(originals,False)+"\n}").replace("/* CANDIDATE */","mod candidate {\n"+extract(changed,True)+"\n}")
        worker_harness=ROOT/"tests/usb-probe-qa/xhci-worker-batching-host-tests.rs"
        source=(copy/paths[-1]).read_text()
        worker=re.search(r"^const XHCI_WORKER_PASS_BUDGET:.*;$",source,re.M).group(0)+"\n"+block(source,"fn xhci_worker_entry()")
        tests=[("packet",test,7,harness),("worker",worker_harness.read_text().replace("/* PRODUCTION_WORKER */",worker),5,worker_harness)]
        for name,text,count,harness in tests:
            path=tmp/(name+".rs");path.write_text(text);(out/path.name).write_text(text)
            compiled=subprocess.run([rustc,"--edition=2024","--target",host,"-C","opt-level=2","--test",str(path),"-o",str(tmp/name)],capture_output=True,text=True,timeout=60)
            (out/(name+"-compile.log")).write_text(compiled.stdout+compiled.stderr);compiled.check_returncode()
            tested=subprocess.run([str(tmp/name),"--test-threads=1","--nocapture"],capture_output=True,text=True,timeout=30)
            (out/(name+"-tests.log")).write_text(tested.stdout+tested.stderr);tested.check_returncode()
            assert f"{count} passed; 0 failed" in tested.stdout
            receipt["runs"].append({"name":name,"tests_passed":count,"harness_sha256":sha(harness),"extracted_sha256":sha(path)})
        # Confirm packet callers use stack serialization and final capacity.
        assert "let header_bytes = header.to_array();" in changed["tcp.rs"]
        assert "Vec::with_capacity(20 + packet.len())" in changed["ipv4.rs"]
        assert "total_size.max(ETHERNET_MIN_SIZE.saturating_sub(4))" in changed["ethernet.rs"]
    assert before=={n:sha(core/n) for n in paths}
    assert status==run(["git","status","--porcelain"],cwd=core).stdout
    receipt.update(result="pass",temporary_checkout_removed=True,source_cache_unchanged=True)
    (out/"receipt.json").write_text(json.dumps(receipt,indent=2)+"\n");print(json.dumps(receipt,indent=2))

if __name__=="__main__":main()

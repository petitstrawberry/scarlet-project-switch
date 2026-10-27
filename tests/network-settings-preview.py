#!/usr/bin/env python3
"""Render the production Settings Network page on an isolated rootfs copy."""
import importlib.util
import argparse
import os
import shutil
import json
import struct
import zlib
import tomllib
from pathlib import Path
import subprocess

ROOT=Path(__file__).resolve().parents[1]
PROJECT=ROOT/"projects/aarch64-switch-l4t-console"
spec=importlib.util.spec_from_file_location("console_preview",ROOT/"tests/qemu-console.py")
console=importlib.util.module_from_spec(spec);spec.loader.exec_module(console)
def prepare_scroll(out,reuse_image=None):
    qa=ROOT/".cache/network-scroll-qa-project"
    if qa.exists():raise ValueError("remove the previous temporary scroll project first")
    if reuse_image:
        qa.mkdir(parents=True)
        shutil.copyfile(reuse_image,qa/"Image")
    else:
        prepare_scroll_kernel(out,qa)
    env=os.environ.copy();env["CARGO_HOME"]=str(PROJECT/".scarlet/cache/cargo-home")
    env.update(CARGO_TARGET_DIR=str(ROOT/".cache/network-scroll-qa-target"),CARGO_UNSTABLE_BUILD_STD="std,panic_abort",CARGO_UNSTABLE_BUILD_STD_FEATURES="compiler-builtins-mem",CARGO_UNSTABLE_UNSTABLE_OPTIONS="true")
    with (out/"scroll-guest-build.log").open("w") as log:
        subprocess.run(["cargo","build","--manifest-path",str(ROOT/"tests/network-settings-scroll/Cargo.toml"),"--release","--target","aarch64-unknown-scarlet"],env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
def prepare_scroll_kernel(out,qa):
    shutil.copytree(PROJECT/"bsp",qa/"bsp",ignore=shutil.ignore_patterns("target","Cargo.lock"))
    scarlet=(ROOT/"../Scarlet").resolve()
    cargo=qa/"bsp/Cargo.toml"
    cargo.write_text(cargo.read_text().replace("../../../../Scarlet",str(scarlet)))
    production=tomllib.loads((PROJECT/"scarlet.toml").read_text())
    features=production["bsp"]["kernel"]["features"]
    manifest='schema_version = 2\n[project]\nname = "network-scroll-qa"\n[bsp]\npath = "bsp"\npackage = "scarlet"\n[bsp.kernel]\n'
    manifest+=f'source = {{ path = {json.dumps(str(scarlet/"kernel"))} }}\n'
    manifest+='features = { '+', '.join(f'{k} = {str(v).lower()}' for k,v in features.items())+' }\n[modules]\n'
    for name,module in production["modules"].items():
        if module.get("enabled"):
            manifest+=f'{json.dumps(name)} = {{ path = {json.dumps(str((PROJECT/module["path"]).resolve()))}, enabled = true }}\n'
    manifest+=f'"scarlet-input-qa-fixture" = {{ path = {json.dumps(str(ROOT/"tests/input-fixture"))}, enabled = true }}\n'
    (qa/"scarlet.toml").write_text(manifest)
    env=os.environ.copy();env["CARGO_HOME"]=str(PROJECT/".scarlet/cache/cargo-home")
    with (out/"scroll-kernel-build.log").open("w") as log:
        subprocess.run(["cargo","scarlet","build","--project",str(qa),"--release"],env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
    subprocess.run(["llvm-objcopy","--remove-section=.ksym","-O","binary",str(qa/"bsp/target/aarch64-switch-none-elf/release/scarlet"),str(qa/"Image")],check=True)
def top_png(case,framebuffer_format):
    rgb=console.logical_rgb((case/"top-framebuffer.bin").read_bytes(),framebuffer_format)
    def chunk(kind,payload):return struct.pack(">I",len(payload))+kind+payload+struct.pack(">I",zlib.crc32(kind+payload))
    rows=b"".join(b"\0"+rgb[y*1280*3:(y+1)*1280*3] for y in range(720))
    data=b"\x89PNG\r\n\x1a\n"+chunk(b"IHDR",struct.pack(">2I5B",1280,720,8,2,0,0,0))+chunk(b"IDAT",zlib.compress(rows))+chunk(b"IEND",b"")
    (case/"top.png").write_bytes(data)
def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--output",type=Path,default=ROOT/".cache/network-settings-preview")
    p.add_argument("--scroll",action="store_true",help="inject native touch swipes and capture top/bottom with two NICs")
    p.add_argument("--reuse-input-image",type=Path,help="reuse an unchanged, previously compiled test-only input kernel")
    args=p.parse_args();out=args.output.resolve();out.mkdir(parents=True,exist_ok=True)
    if args.scroll:prepare_scroll(out,args.reuse_input_image)
    image=out/"rootfs.ext2"
    if image.exists():raise ValueError("preserve existing preview evidence before rerunning")
    subprocess.run(["/bin/cp","-c",str(PROJECT/".scarlet/images/rootfs-switch-full.ext2"),str(image)],check=True)
    service=out/"preview.toml"
    service.write_text('[service.network-settings-preview]\nexec = "/bin/settings --network"\ndepends = ["scarlet-desktop"]\n')
    subprocess.run(["debugfs","-w","-R",f'write "{service}" /etc/stemd.d/services/98-network-settings-preview.toml',str(image)],check=True)
    if args.scroll:
        config=out/"interfaces.toml"
        config.write_text('[[interface]]\nname = "veth0"\nmethod = "static"\naddress = "10.0.2.15/24"\ngateway = "10.0.2.2"\nmetric = 100\n[[interface]]\nname = "veth1"\nmethod = "static"\naddress = "10.0.3.15/24"\ngateway = "10.0.3.2"\nmetric = 50\n')
        subprocess.run(["debugfs","-w","-R",f'write "{config}" /etc/netcfgd.d/zz-network-preview.toml',str(image)],check=True)
    try:
        result=console.run(timeout=240,settle_seconds=3,rootfs=image,output_dir=out/"capture",network_scroll_qa=args.scroll)
        assert result["passed"]
        case=out/"capture"/("network-scroll-el2" if args.scroll else "el2")
        log=(case/"serial.log").read_text(errors="replace")
        assert "[settings] Starting" in log and "name=Settings" in log
        if args.scroll:
            assert "NETWORK_SCROLL_TOP_READY" in log and "NETWORK_SCROLL_BOTTOM_READY" in log
            top_png(case,result["scanout_format"])
        print("NETWORK_SETTINGS_PREVIEW PASS: inspect",case)
    finally:
        if args.scroll:
            shutil.rmtree(ROOT/".cache/network-scroll-qa-project")
            shutil.rmtree(ROOT/".cache/network-scroll-qa-target",ignore_errors=True)
if __name__=="__main__":main()

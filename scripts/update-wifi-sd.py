#!/usr/bin/env python3
"""Update Wi-Fi OS files in the existing SD filesystem, preserving user data.

Default: read-only plan. --write updates selected files in unmounted p4 with
debugfs, checks the filesystem, then verifies each file by readback. It creates
no filesystem or boot-file backups. Other installed files are retained.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
spec = importlib.util.spec_from_file_location("rootfs_install", ROOT/"scripts/install-rootfs.py")
disk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(disk)
FILES = [
    "bin/settings", "bin/wirelessd", "bin/wifictl", "bin/wpa-supplicant",
    "bin/netcfgd", "bin/scarlet-ld",
    "etc/stemd.d/services/03-wirelessd.toml",
    "etc/netcfgd.d/50-wifi.toml",
]

def digest(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()

def quote(value):
    value = str(value)
    if any(c in value for c in '"\n\r'):
        raise ValueError("unsupported debugfs path")
    return '"'+value+'"'

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--device",required=True)
    p.add_argument("--overlay",type=Path,default=PROJECT/".scarlet/staging/rootfs")
    p.add_argument("--profile",type=Path)
    p.add_argument("--log-dir",type=Path,default=ROOT/".cache/sd-wifi-update")
    p.add_argument("--debugfs",default="debugfs")
    p.add_argument("--e2fsck",default="e2fsck")
    p.add_argument("--write",action="store_true")
    p.add_argument("--file",action="append",choices=FILES,
                   help="Update only these OS files; repeat as needed (default: full Wi-Fi integration)")
    a=p.parse_args()
    disk.validate_disk(a.device)
    pins=json.loads((PROJECT/"wifi-firmware.json").read_text())["files"]
    files={name:(a.overlay/name).resolve(strict=True) for name in (a.file or FILES)}
    for name,pin in pins.items():
        source=(a.overlay/"lib/firmware/brcm"/name).resolve(strict=True)
        if source.stat().st_size!=pin["size"] or digest(source)!=pin["sha256"]:
            raise ValueError("Wi-Fi firmware hash mismatch")
        if not a.file:
            files["lib/firmware/brcm/"+name]=source
    if a.profile:
        files["etc/wireless.conf"]=a.profile.resolve(strict=True)
    expected={name:digest(source) for name,source in files.items()}
    owner=a.overlay.stat()
    for package in ("l4t","switchvisor"):
        directory=PROJECT/".scarlet"/package
        manifest=json.loads((directory/"manifest.json").read_text())
        for name,sha in manifest["sha256"].items():
            if digest(directory/name)!=sha:raise ValueError("boot package hash mismatch")
    print("Target:",a.device+"s4","; files:",len(files),flush=True)
    print("Existing home directories and unrelated configuration are retained.",flush=True)
    for name in files:print("Update /"+name,flush=True)
    if not a.write:
        print("Read-only plan verified; --write performs the prepared update.",flush=True)
        return
    if os.geteuid()!=0:raise ValueError("administrator access required")
    logs=a.log_dir.resolve()
    logs.mkdir(parents=True,exist_ok=True)
    os.chmod(logs,0o700)
    os.chown(logs,owner.st_uid,owner.st_gid)
    raw="/dev/r"+a.device.removeprefix("/dev/")
    # Buffered block devices accept debugfs' metadata reads on macOS.
    target=a.device+"s4"
    subprocess.run(["/usr/sbin/diskutil","unmountDisk",a.device],check=True)
    if any(part.get("MountPoint") for part in disk.validate_disk(a.device)):
        raise ValueError("SD still mounted")
    with open(raw,"rb",buffering=0) as f:protected=disk.protected_samples(f)
    def fsck():
        with (logs/"filesystem-check.log").open("a") as log:
            result=subprocess.run([a.e2fsck,"-p","-f",target],stdout=log,stderr=subprocess.STDOUT)
        if result.returncode not in (0,1):raise ValueError("filesystem check failed")
    fsck()
    changed=[]
    with tempfile.TemporaryDirectory(prefix="verify-",dir=logs) as temporary, (logs/"debugfs.log").open("w") as log:
        check=Path(temporary)/"file"
        def matches(name,sha):
            check.unlink(missing_ok=True)
            subprocess.run([a.debugfs,"-R","dump "+quote("/"+name)+" "+quote(check),target],stdout=log,stderr=log,check=True)
            result=check.exists() and digest(check)==sha
            check.unlink(missing_ok=True)
            return result
        def edit(command):
            subprocess.run([a.debugfs,"-w","-R",command,target],stdout=log,stderr=log,check=True)
        def directory_exists(name):
            result=subprocess.run([a.debugfs,"-R","stat "+quote(name),target],stdout=subprocess.PIPE,stderr=log,text=True,check=True)
            return "Type: directory" in result.stdout
        for name,source in files.items():
            if matches(name,expected[name]):continue
            if digest(source)!=expected[name]:raise ValueError("input changed during preparation")
            directories=[str(d) for d in Path("/"+name).parents if str(d)!="/"]
            for directory in reversed(directories):
                if not directory_exists(directory):
                    edit("mkdir "+quote(directory))
                    if not directory_exists(directory):raise ValueError("directory creation failed: "+directory)
            edit("rm "+quote("/"+name))
            edit("write "+quote(source)+" "+quote("/"+name))
            mode="0100755" if name.startswith("bin/") else "0100600" if name=="etc/wireless.conf" else "0100644"
            edit("set_inode_field "+quote("/"+name)+" mode "+mode)
            if not matches(name,expected[name]):raise ValueError("file readback mismatch: "+name)
            changed.append(name)
            print("Updated and verified /"+name,flush=True)
    fsck()
    with open(raw,"rb",buffering=0) as f:
        if disk.protected_samples(f)!=protected:raise ValueError("protected partition sample changed")
    subprocess.run(["/usr/sbin/diskutil","mount",a.device+"s1"],check=True)
    mount=Path(disk.disk_info(a.device+"s1")["MountPoint"])
    for profile in ("--console","--switchvisor"):
        subprocess.run([os.sys.executable,str(ROOT/"scripts/install-sd.py"),profile,"--mount",str(mount),"--write"],check=True)
    receipt={"device":a.device,"verified_files":list(files),"changed_files":changed,
             "partition_table_preserved":True,"other_partition_samples_preserved":True,
             "hardware_boot_validated":False}
    (logs/"receipt.json").write_text(json.dumps(receipt,indent=2)+"\n")
    for path in logs.rglob("*"):os.chown(path,owner.st_uid,owner.st_gid)
    os.chown(logs,owner.st_uid,owner.st_gid)
    print("SD update verified.",flush=True)
if __name__=="__main__":
    main()

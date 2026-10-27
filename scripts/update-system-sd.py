#!/usr/bin/env python3
"""Update installed bundle files on the inspected, unmounted Scarlet SD.

Modified configuration and mutable data are retained. Published default
configuration can advance with the bundle. Files are staged beside their
destination and read back before replacement; no backup is created.
Default: read-only comparison. --write requires administrator access.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects/aarch64-switch-l4t-console"
spec = importlib.util.spec_from_file_location("rootfs_install", ROOT / "scripts/install-rootfs.py")
disk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(disk)
SYSTEM = {"init", "bin", "lib", "usr", "opt", "share", "applications", "systems", "etc"}
MUTABLE = {"root", "home", "tmp", "run", "dev", "proc", "sys", "mnt", "media"}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def quote(value):
    value = str(value)
    if any(c in value for c in ('"', "\\", "\n", "\r", "\0")):
        raise ValueError("unsupported debugfs path")
    return '"' + value + '"'


def candidates(overlay):
    for path in sorted(overlay.rglob("*")):
        rel = path.relative_to(overlay)
        parts = rel.parts
        if parts[0] not in SYSTEM or any(p in (".DS_Store", ".gitignore") for p in parts):
            continue
        if parts[0] == "systems" and len(parts) > 2 and parts[2] in MUTABLE:
            continue
        if path.is_symlink() or path.is_file():
            # Never install content reached through a host-side symlink.
            if any(p.is_symlink() for p in path.parents if p != overlay and overlay in p.parents):
                raise ValueError("symlink ancestor in staging: " + str(rel))
            yield rel.as_posix(), path


def configuration(name):
    parts = Path(name).parts
    return parts[0] == "etc" or (parts[0] == "systems" and len(parts) > 2 and parts[2] in {"etc", "var"})


def configuration_defaults(scarlet=None):
    """Recognize only version-controlled defaults from selected source trees."""
    defaults = {}
    roots = [(ROOT, PROJECT / "rootfs/etc")]
    if scarlet is not None:
        scarlet = scarlet.resolve(strict=True)
        roots.extend((scarlet, path) for path in (scarlet / "bundles").glob("*/fs/etc"))
    for repository, base in roots:
        for path in base.rglob("*"):
            if path.is_symlink() or not path.is_file():
                continue
            name = "etc/" + path.relative_to(base).as_posix()
            relative = path.relative_to(repository).as_posix()
            revisions = subprocess.check_output(["git", "log", "--format=%H", "HEAD", "--", relative], cwd=repository, text=True).splitlines()
            for revision in revisions:
                blob = subprocess.run(["git", "show", revision + ":" + relative], cwd=repository, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
                if blob.returncode == 0:
                    defaults.setdefault(name, set()).add(hashlib.sha256(blob.stdout).hexdigest())
    return defaults


def action(name, installed, expected, defaults):
    if installed == expected:
        return "verify"
    if installed is not None and configuration(name):
        if installed.get("sha256") not in defaults.get(name, set()):
            return "retain"
    return "update"


class Updater:
    def __init__(self, device, debugfs, temporary, log):
        self.device, self.debugfs, self.temporary, self.log = device, debugfs, temporary, log
        self.directories = {"/"}

    def run(self, command, write=False):
        args = [self.debugfs] + (["-w"] if write else []) + ["-R", command, str(self.device)]
        result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, check=True)
        self.log.write(result.stdout)
        return result.stdout

    def stat(self, name):
        output = self.run("stat " + quote(name))
        match = re.search(r"Type:\s+(\S+)\s+Mode:\s+([0-7]+)", output)
        if match:
            return match[1], int(match[2], 8), output
        if "File not found" in output:
            return None
        raise ValueError("cannot inspect " + name + ": " + output)

    def identity(self, name, info):
        if info[0] == "symlink":
            match = re.search(r'Fast link dest: "(.*)"', info[2])
            if match:
                return {"link": match[1]}
        elif info[0] != "regular":
            raise ValueError("unexpected installed file type: " + name)
        output = self.temporary / "readback"
        output.unlink(missing_ok=True)
        self.run("dump " + quote(name) + " " + quote(output))
        if not output.is_file():
            raise ValueError("readback failed: " + name)
        if info[0] == "symlink":
            value = {"link": output.read_text()}
        else:
            value = {"sha256": digest(output), "mode": info[1]}
        output.unlink()
        return value

    def parents(self, name):
        for parent in reversed(Path(name).parents):
            key = str(parent)
            if key in self.directories:
                continue
            info = self.stat(key)
            if info is None:
                self.run("mkdir " + quote(key), write=True)
                info = self.stat(key)
            if info is None or info[0] != "directory":
                raise ValueError("destination ancestor is not a directory: " + key)
            self.directories.add(key)

    def replace(self, name, source, expected):
        self.parents(name)
        staged = str(Path(name).with_name(".scarlet-bundle-update"))
        if self.stat(staged) is not None:
            raise ValueError("unfinished staged replacement: " + staged)
        if "link" in expected:
            if not source.is_symlink() or os.readlink(source) != expected["link"]:
                raise ValueError("staging source changed: " + str(source))
            self.run("symlink " + quote(staged) + " " + quote(expected["link"]), write=True)
        else:
            if source.is_symlink() or digest(source) != expected["sha256"]:
                raise ValueError("staging source changed: " + str(source))
            self.run("write " + quote(source) + " " + quote(staged), write=True)
            mode = f"0{0o100000 | expected['mode']:o}"
            self.run("set_inode_field " + quote(staged) + " mode " + mode, write=True)
        info = self.stat(staged)
        if info is None or self.identity(staged, info) != expected:
            raise ValueError("staged readback mismatch: " + name)
        if self.stat(name) is not None:
            self.run("rm " + quote(name), write=True)
        # debugfs link/unlink do not change i_links_count: the staged inode's
        # count stays one across this same-directory replacement.
        self.run("link " + quote(staged) + " " + quote(name), write=True)
        info = self.stat(name)
        if info is None or self.identity(name, info) != expected:
            raise ValueError("installed readback mismatch; staged file retained: " + name)
        self.run("unlink " + quote(staged), write=True)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--device", required=True)
    p.add_argument("--overlay", type=Path, default=PROJECT / ".scarlet/staging/rootfs")
    p.add_argument("--scarlet-source", type=Path, help="Optional pinned Scarlet checkout for recognizing previous common configuration defaults")
    p.add_argument("--log-dir", type=Path, default=ROOT / ".cache/sd-system-update")
    p.add_argument("--debugfs", default="debugfs")
    p.add_argument("--e2fsck", default="e2fsck")
    p.add_argument("--write", action="store_true")
    a = p.parse_args()
    parts = disk.validate_disk(a.device)
    if any(part.get("MountPoint") for part in parts):
        subprocess.run(["/usr/sbin/diskutil", "unmountDisk", a.device], check=True)
    if any(part.get("MountPoint") for part in disk.validate_disk(a.device)):
        raise ValueError("SD still mounted")
    overlay = a.overlay.resolve(strict=True)
    if a.write and os.geteuid() != 0:
        raise ValueError("administrator access required")
    logs = a.log_dir.resolve()
    logs.mkdir(parents=True, exist_ok=True)
    owner = overlay.stat()
    os.chmod(logs, 0o700)
    raw = "/dev/r" + a.device.removeprefix("/dev/")
    target = a.device + "s4"
    with open(raw, "rb", buffering=0) as stream:
        protected = disk.protected_samples(stream)
    def fsck(write):
        with (logs / "filesystem-check.log").open("a") as log:
            status = subprocess.run([a.e2fsck, "-p" if write else "-n", "-f", target], stdout=log, stderr=subprocess.STDOUT)
        if status.returncode not in ((0, 1) if write else (0,)):
            raise ValueError("filesystem check failed")
    fsck(a.write)
    defaults = configuration_defaults(a.scarlet_source)
    changed, retained, verified = [], [], []
    with tempfile.TemporaryDirectory(prefix="verify-", dir=logs) as tmp, (logs / "debugfs.log").open("w") as log:
        updater = Updater(target, a.debugfs, Path(tmp), log)
        for name, source in candidates(overlay):
            destination = "/" + name
            info = updater.stat(destination)
            expected = {"link": os.readlink(source)} if source.is_symlink() else {"sha256": digest(source), "mode": source.stat().st_mode & 0o7777}
            installed = updater.identity(destination, info) if info is not None else None
            choice = action(name, installed, expected, defaults)
            if choice == "retain":
                retained.append(name)
                continue
            if choice == "verify":
                verified.append(name)
                continue
            if a.write:
                updater.replace(destination, source, expected)
            changed.append(name)
            print(("Updated and verified /" if a.write else "Would update /") + name, flush=True)
    if a.write:
        fsck(True)
    with open(raw, "rb", buffering=0) as stream:
        if disk.protected_samples(stream) != protected:
            raise ValueError("protected partition sample changed")
    receipt = {"device": a.device, "written": a.write, "changed_files": changed, "unchanged_files_verified": verified, "existing_configuration_retained": retained, "partition_table_preserved": True}
    (logs / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    if os.geteuid() == 0:
        for path in logs.rglob("*"):
            os.chown(path, owner.st_uid, owner.st_gid)
        os.chown(logs, owner.st_uid, owner.st_gid)
    print(f"Bundle {'update' if a.write else 'comparison'} complete: {len(changed)} changed, {len(verified)} unchanged, {len(retained)} existing configuration files retained.", flush=True)


if __name__ == "__main__":
    main()

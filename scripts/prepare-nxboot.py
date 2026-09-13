#!/usr/bin/env python3
"""Fetch the signed macOS NXBoot CLI into this project's local cache."""
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
URL = "https://github.com/mologie/nxboot/releases/download/v0.3.2/nxboot"
SHA256 = "dbdbaccc464367abeff6ecd3792b90442b0cf17b08e1690bbfa9090b4d59560e"

def main():
    if os.uname().sysname != "Darwin":
        raise SystemExit("NXBoot CLI is macOS-only; use a Linux RCM injector on Linux")
    cache = ROOT / ".cache"
    cache.mkdir(exist_ok=True)
    binary = cache / "nxboot"
    if not binary.is_file() or hashlib.sha256(binary.read_bytes()).hexdigest() != SHA256:
        with urllib.request.urlopen(URL, timeout=30) as response:
            data = response.read()
        if hashlib.sha256(data).hexdigest() != SHA256:
            raise SystemExit("NXBoot SHA256 mismatch")
        with tempfile.NamedTemporaryFile(dir=cache, delete=False) as temp:
            temp.write(data)
            staged = Path(temp.name)
        staged.chmod(0o755)
        staged.replace(binary)
    subprocess.run(["/usr/bin/codesign", "--verify", "--strict", str(binary)], check=True)
    subprocess.run([str(binary), "--help"], check=True)
    print(f"NXBoot verified: {binary}\nSHA256: {SHA256}")

if __name__ == "__main__":
    main()

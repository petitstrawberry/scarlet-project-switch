#!/usr/bin/env python3
"""Run Scarlet's network regressions at the Switch kernel Git revision."""
import argparse
from pathlib import Path
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--core", type=Path, default=ROOT.parent / "Scarlet")
    parser.add_argument("--output", type=Path,
                        default=ROOT / ".cache/network-perf/upstream-integration/tests")
    args = parser.parse_args()
    core = args.core.resolve(strict=True)
    project = ROOT / "projects/aarch64-switch-l4t-console/scarlet.toml"
    revision = tomllib.loads(project.read_text())["bsp"]["kernel"]["source"]["rev"]
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=core,
                                     text=True).strip()
    if actual != revision:
        parser.error(f"Scarlet checkout must be at the kernel pin {revision}; got {actual}")
    runner = core / "tools/test-network-regression.py"
    subprocess.run([sys.executable, str(runner), "--output", str(args.output.resolve())],
                   cwd=core, check=True)


if __name__ == "__main__":
    main()

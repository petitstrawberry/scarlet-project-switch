#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
CARGO_HOME="$PWD/projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home" \
CARGO_TARGET_DIR="$PWD/.cache/console-qa-target" \
CARGO_UNSTABLE_BUILD_STD=std,panic_abort \
CARGO_UNSTABLE_BUILD_STD_FEATURES=compiler-builtins-mem \
CARGO_UNSTABLE_UNSTABLE_OPTIONS=true \
  cargo build --manifest-path tests/console-qa/Cargo.toml \
  --target aarch64-unknown-scarlet --release
python3 tests/qemu-console.py
python3 tests/qemu-console.py --el1
python3 tests/qemu-console.py --screen-only
python3 tests/check-isa.py --console
python3 tests/host-tools.py

#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
project=projects/aarch64-switch-console
python3 scripts/prepare-console.py
env -u CARGO_UNSTABLE_BUILD_STD -u CARGO_UNSTABLE_BUILD_STD_FEATURES \
  cargo scarlet build --project "$project" --release
elf="$PWD/$project/bsp/target/aarch64-switch-none-elf/release/scarlet"
env -u CARGO_UNSTABLE_BUILD_STD -u CARGO_UNSTABLE_BUILD_STD_FEATURES \
  cargo scarlet image --project "$project" --release --no-build --kernel-elf "$elf"

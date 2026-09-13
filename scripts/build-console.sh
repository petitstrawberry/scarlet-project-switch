#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
project=projects/aarch64-switch-console
python3 scripts/prepare-console.py
# The BSP needs only core/alloc. Native applications need the same std rebuilt
# for Cortex-A57; a target flag alone would leave LSE in precompiled std.
env -u CARGO_UNSTABLE_BUILD_STD -u CARGO_UNSTABLE_BUILD_STD_FEATURES \
  cargo scarlet build --project "$project" --release
elf="$PWD/$project/bsp/target/aarch64-switch-none-elf/release/scarlet"
CARGO_UNSTABLE_BUILD_STD=std,panic_abort \
CARGO_UNSTABLE_BUILD_STD_FEATURES=compiler-builtins-mem \
CARGO_UNSTABLE_UNSTABLE_OPTIONS=true \
  cargo scarlet image --project "$project" --release --no-build --kernel-elf "$elf"

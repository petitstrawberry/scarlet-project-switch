#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
# Run after building the production image, inside nix develop.
CARGO_HOME="$PWD/projects/aarch64-switch-l4t-console/.scarlet/cache/cargo-home" \
CARGO_TARGET_DIR="$PWD/.cache/system-power-futex/target" \
CARGO_UNSTABLE_BUILD_STD=std,panic_abort \
CARGO_UNSTABLE_BUILD_STD_FEATURES=compiler-builtins-mem \
CARGO_UNSTABLE_UNSTABLE_OPTIONS=true \
  cargo build --manifest-path tests/system-power-waiter/Cargo.toml \
  --target aarch64-unknown-scarlet --release --locked
python3 tests/qemu-system-power.py
python3 tests/qemu-system-power.py \
  --futex-waiter .cache/system-power-futex/target/aarch64-unknown-scarlet/release/system-power-waiter

for mode in waitpid waitpid-any sleep poll-empty poll-min; do
  python3 tests/qemu-system-power.py \
    --futex-waiter .cache/system-power-futex/target/aarch64-unknown-scarlet/release/system-power-waiter \
    --waiter-mode "$mode"
done

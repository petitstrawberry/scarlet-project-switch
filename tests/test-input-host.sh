#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
host_target=$(rustc -vV | sed -n 's/^host: //p')
python3 tests/check-input-format.py
for driver in soc/tegra210 rtc/max77620 input/touchscreen/stm-ftm4 input/joycon; do
    cargo test --manifest-path "drivers/$driver/Cargo.toml" --target "$host_target"
done
cargo test --manifest-path tests/input-host/Cargo.toml --target "$host_target"
cargo test --manifest-path ../Scarlet/user/lib/sws-protocol/Cargo.toml \
    --no-default-features --features std --target "$host_target"
cargo test --manifest-path ../scarlet-ui/Cargo.toml -p scarlet-ui-core --lib \
    --target "$host_target" -- --test-threads=1

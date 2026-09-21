#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
host_target=$(rustc -vV | sed -n 's/^host: //p')
python3 scripts/project_sources.py
scarlet_source=$(python3 scripts/project_sources.py --path scarlet)
ui_source=$(python3 scripts/project_sources.py --path scarlet-ui)
python3 tests/check-input-format.py
for driver in soc/tegra210 rtc/max77620 input/touchscreen/stm-ftm4 input/joycon; do
    cargo test --manifest-path "drivers/$driver/Cargo.toml" --target "$host_target"
done
cargo test --manifest-path tests/input-host/Cargo.toml --target "$host_target"
cargo test --config "$PWD/.cargo/config.toml" --manifest-path "$scarlet_source/user/lib/sws-protocol/Cargo.toml" \
    --no-default-features --features std --target "$host_target"
cargo test --config "$PWD/.cargo/config.toml" --manifest-path "$ui_source/Cargo.toml" -p scarlet-ui-core --lib \
    --target "$host_target" -- --test-threads=1

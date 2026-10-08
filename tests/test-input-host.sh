#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
host_target=$(rustc -vV | sed -n 's/^host: //p')
: "${SCARLET_SOURCE:?set SCARLET_SOURCE to the Scarlet checkout under test}"
: "${SCARLET_UI_SOURCE:?set SCARLET_UI_SOURCE to the ScarletUI checkout under test}"
scarlet_source=$SCARLET_SOURCE
ui_source=$SCARLET_UI_SOURCE
python3 tests/check-input-format.py
for driver in soc/tegra210 rtc/max77620 random/tegra210-se input/touchscreen/stm-ftm4 input/joycon input/switch-buttons; do
    cargo test --manifest-path "drivers/$driver/Cargo.toml" --target "$host_target"
done
cargo test --manifest-path tests/input-host/Cargo.toml --target "$host_target"
cargo test --manifest-path "$scarlet_source/user/lib/sws-protocol/Cargo.toml" \
    --no-default-features --features std --target "$host_target"
cargo test --manifest-path "$ui_source/Cargo.toml" -p scarlet-ui-core --lib \
    --target "$host_target" -- --test-threads=1

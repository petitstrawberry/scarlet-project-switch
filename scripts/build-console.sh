#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
exec cargo scarlet image --project projects/aarch64-switch-l4t-console --release "$@"

#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$script_dir/prepare-gm20b-firmware.py" --download --install-dir "$1"

#!/bin/sh
set -eu
exec python3 "${SCARLET_PROJECT_DIR:-.}/tools/package_l4t.py" --profile "${SCARLET_PROFILE:-release}"

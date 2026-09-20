#!/bin/sh
set -eu
project=${SCARLET_PROJECT_DIR:-.}
exec python3 "$project/../aarch64-switch-l4t/tools/package_l4t.py" \
  --project "$project" --profile "${SCARLET_PROFILE:-release}" \
  --boot-directory scarlet-console --entry-file L4T-scarlet-console.ini

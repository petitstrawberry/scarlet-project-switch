#!/bin/sh
set -eu
project=${SCARLET_PROJECT_DIR:-.}
python3 "$project/tools/package_l4t.py" \
  --project "$project" --profile "${SCARLET_PROFILE:-release}"
exec python3 "$project/../../scripts/package-switchvisor.py" --usb-net

#!/bin/sh
set -eu
task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if [ "${1:-}" = --help ]; then
    echo "usage: $0 [menu|console|switchvisor|ums-sd] <Hekate payload.bin>"
    exit 0
fi
mode=${1:-menu}
payload=${2:?Provide the Hekate payload path; see --help}
if [ ! -f "$payload" ]; then echo "Hekate payload not found: $payload" >&2; exit 1; fi
if [ ! -x "$task_root/.cache/nxboot" ]; then
    echo "Run python3 $task_root/scripts/prepare-nxboot.py first" >&2
    exit 1
fi
task_digest=$(shasum -a 256 "$task_root/.cache/nxboot")
if [ "${task_digest%% *}" != dbdbaccc464367abeff6ecd3792b90442b0cf17b08e1690bbfa9090b4d59560e ]; then
    echo "NXBoot SHA256 mismatch; run prepare-nxboot.py" >&2
    exit 1
fi
/usr/bin/codesign --verify --strict "$task_root/.cache/nxboot"
case "$mode" in
    menu) set -- --hekate menu ;;
    console) set -- --hekate id SCR-NXC ;;
    switchvisor) set -- --hekate id SCR-SWV ;;
    ums-sd) set -- --hekate ums sd ;;
    *) echo "Unknown mode: $mode" >&2; exit 1 ;;
esac
exec "$task_root/.cache/nxboot" "$@" "$payload"

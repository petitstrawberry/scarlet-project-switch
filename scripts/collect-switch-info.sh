#!/bin/sh
set -eu
if [ "$(uname -s)" != Linux ]; then
    echo "Run this on the Switch's Kubuntu, preferably with sudo." >&2
    exit 1
fi
task_output=${1:-switch-info}
mkdir -p "$task_output"
uname -a > "$task_output/uname.txt"
cat /proc/cpuinfo > "$task_output/cpuinfo.txt"
cat /proc/meminfo > "$task_output/meminfo.txt"
cat /proc/iomem > "$task_output/iomem.txt"
cat /proc/cmdline > "$task_output/cmdline.txt"
dmesg > "$task_output/dmesg.txt" 2>&1 || true
if [ -r /sys/firmware/fdt ]; then cp /sys/firmware/fdt "$task_output/boot.dtb"; fi
if command -v dtc >/dev/null 2>&1; then
    dtc -q -I fs -O dtb -o "$task_output/live.dtb" /sys/firmware/devicetree/base
    dtc -q -I fs -O dts -o "$task_output/live.dts" /sys/firmware/devicetree/base
fi
for file in /sys/class/graphics/fb0/name /sys/class/graphics/fb0/virtual_size /sys/class/graphics/fb0/stride /sys/class/graphics/fb0/bits_per_pixel /sys/class/graphics/fb0/modes; do
    if [ -r "$file" ]; then
        cat "$file" > "$task_output/fb0-$(basename "$file").txt"
    fi
done
tar -czf "$task_output/device-tree.tar.gz" -C /sys/firmware/devicetree base
echo "Switch information collected in $task_output"

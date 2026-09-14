# Share the console kernel/initramfs. Only diagnostic display ownership changes.
# Keep this running script outside the kernel, DTB and initramfs load buffers.
setenv prefix /switchroot/scarlet-console/
setenv scarlet_keep_bootcon 1
if load mmc ${devnum}:${distro_bootpart} 0x91000000 ${prefix}/boot.scr; then
    source 0x91000000
fi
echoe Scarlet console diagnostic script returned
sleep 3
reset

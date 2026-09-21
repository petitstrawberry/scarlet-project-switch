# Hekate boot menu

| Entry | Hekate ID | Boot directory | Purpose |
| --- | --- | --- | --- |
| `switchvisor` | `SCR-SWV` | `switchroot/scarlet-switchvisor` | USB UART/control, optional virtio-net, host-uploaded guest |
| `scarlet` | `SCR-NXC` | `switchroot/scarlet-console` | Direct SD kernel/initramfs boot |

Both use the Scarlet ext2 root on `/dev/mmcblk0p4`.
The [console package](console.md) supplies the kernel/initramfs used by
both entries. The [Switchvisor package](switchvisor-usb-debug.md) adds its
EL2 monitor, overlays and host upload bundle.

After packaging both profiles, update the mounted FAT32 partition on macOS:

```sh
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --l4t
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --l4t --write
```

The combined profile validates the known MBR layout, input hashes and SD
readback. It replaces both menu files and removes the obsolete menu files
`L4T-noble.ini`, `L4T-scarlet.ini` and `L4T-switchvisor.ini`.
Their boot/OS directories are retained. Hekate's main configuration,
Atmosphere, emuMMC, the L4T firmware and Noble boot data are checked for
unchanged hashes.

Without a profile flag, the installer updates only the `scarlet` entry;
`--console` is an explicit alias for that default. `--switchvisor` updates
only the USB entry. Existing other Scarlet boot directories are preserved
for these individual installs.

Eject the actual whole disk before disconnecting or leaving Hekate UMS.
The generated installation receipt records file transfer; hardware boot
must be checked separately.

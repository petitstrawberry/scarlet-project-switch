# Hekate boot menu

| Entry | Hekate ID | Boot directory | Purpose |
| --- | --- | --- | --- |
| `switchvisor` | `SCR-SWV` | `switchroot/scarlet-switchvisor` | Automatic U-Boot fallback; kernel/initramfs from the installed `scarlet-console` directory |
| `switchvisor-dev` | `SCR-SWD` | `switchroot/scarlet-switchvisor-dev` | Indefinite USB upload wait; no automatic SD boot |
| `scarlet` | `SCR-NXC` | `switchroot/scarlet-console` | Direct SD kernel/initramfs boot |

All three normally use the Scarlet ext2 root on `/dev/mmcblk0p4`.
The [console package](console.md) supplies the kernel/initramfs used by
all entries. The [Switchvisor package](switchvisor-usb-debug.md) generates
both monitor profiles, overlays and a host upload bundle. Both Switchvisor
entries keep USB UART/control and optional virtio-net at EL2. The normal entry
starts its packaged U-Boot after the USB preboot window (currently two seconds);
use `switchvisor-dev` for uploads so host timing cannot trigger SD boot.
`scripts/start-switchvisor.py` always selects the development entry.

After packaging both profiles, update the mounted FAT32 partition on macOS:

```sh
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --l4t
python3 scripts/install-sd.py --mount "/Volumes/SWITCH SD" --l4t --write
```

The combined profile validates the known MBR layout, input hashes and SD
readback. It replaces all three menu files and removes the obsolete menu files
`L4T-noble.ini`, `L4T-scarlet.ini` and `L4T-switchvisor.ini`.
Their boot/OS directories are retained. Hekate's main configuration,
Atmosphere, emuMMC, the L4T firmware and Noble boot data are checked for
unchanged hashes.

Without a profile flag, the installer updates only the `scarlet` entry;
`--console` is an explicit alias for that default. `--switchvisor` updates
both Switchvisor entries. A Switchvisor-only install requires matching console
kernel/initramfs files already on SD; use `--l4t` to install them together.
Existing other Scarlet boot directories are preserved for these individual installs.

Eject the actual whole disk before disconnecting or leaving Hekate UMS.
The generated installation receipt records file transfer; hardware boot
must be checked separately.

# Current Hekate L4T menu

The supported menu contains two entries:

| Entry | Hekate ID | Boot directory | Purpose |
| --- | --- | --- | --- |
| `switchvisor` | `SCR-SWV` | `switchroot/scarlet-switchvisor` | USB UART/control, host-supplied guest bundle, GDB disabled |
| `scarlet (console)` | `SCR-NXC` | `switchroot/scarlet-console` | Direct SD kernel/initramfs boot |

Both boot the Scarlet ext2 root on `/dev/mmcblk0p4`. The console packager no
longer emits the SGFX logging menu entry. `scripts/package-switchvisor.py`
uses the console package's kernel/initramfs when making its USB bundle.

After packaging both profiles, update an inspected Hekate SD UMS mount with:

```sh
python3 scripts/install-sd.py --mount '/Volumes/SWITCH SD' --l4t
python3 scripts/install-sd.py --mount '/Volumes/SWITCH SD' --l4t --write
```

The combined profile verifies the known MBR layout, input hashes and SD
readback. It replaces both menu files and removes the obsolete menu files
`L4T-noble.ini`, `L4T-scarlet.ini` and `L4T-switchvisor.ini`. Their boot/OS
directories are retained. Hekate's main configuration, Atmosphere, emuMMC,
the L4T boot stack and Noble boot data are checked for unchanged hashes.
No backup copies are created. Eject the actual whole disk before closing UMS.

## Validation, 2026-09-20

The HWDC-tested kernel and initramfs payloads were compared byte-for-byte
against the packaged SD images. All 14 installed files passed SHA-256
readback, all 29 protected files were unchanged, and exactly the two menu
entries above remained. The user selected `switchvisor` from the new menu;
USB deployment then booted Scarlet with four CPUs, the SD root and SWS.

The initial installed `uImage` SHA-256 was
`2b4358ef410370aa823592afb98eb3e972361656d06b069ab78b359379c64054`.
Its initramfs SHA-256 was
`409c6802b1115c14ad2f8ca774fd6fd70ce22fbc6998b4ae6ed2081cf13fc2af`.
Local receipt: `projects/aarch64-switch-console/.scarlet/sd-installation.json`.

### Audio and NVDEC performance refresh

The SD console images were replaced with the current workspace build including
speaker playback (`d81607b`) and NVDEC surface reuse/layout conversion
(`75b0d49`). All 14 files passed SHA-256 readback, all 29 protected files
remained unchanged, and the two entries above were the only menu files.
The rediscovered device was `disk12s1`; the whole `disk12` was safely ejected.

| Artifact | SHA-256 |
| --- | --- |
| Kernel ELF | `938544877c1c6de8444855b5f035649ec89781dca753e6b55bea8513d90195ec` |
| `Image` | `c6359331fd3b98496818f01612d85edbd55e2fe1ed20f9446e64917d2360536e` |
| `uImage` | `c6fb623adbd7494bacaa69a46a148b82af35a416585e8a2c701ea056fbca76c2` |
| `initramfs` | `67ba9ae2a1e306e867b3e6fb24f91114e0500bdce6f200a542f7d1d111bb4483` |

The installation receipt above and
`.cache/audio-bringup-20260920/sd-install.log` record the refresh.
Post-refresh boot verification is pending the user leaving UMS and selecting
`switchvisor`.

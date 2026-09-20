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
The user then selected `switchvisor` from the updated SD. USB deployment of
the images above reached `guest-running`, `cpu-mask=0xf`, with USB up and the
loader/fallback disabled. Scarlet mounted `/dev/mmcblk0p4` on the first attempt,
started SWS, and SAS configured the RT5639 speaker device at 48 kHz stereo.
This verifies the updated Switchvisor entry and the host-supplied guest bundle;
the direct `scarlet (console)` entry has readback verification only.
Evidence: `.cache/audio-bringup-20260920/deploy-final.log` and
`uart-audio-final.log`.

At the user's request, `sasctl volume 0` set and reported a 0% master volume.
Further video checks must stay silent; set it again after any SAS restart or
reboot, since SAS currently starts at its default 25% volume.

### Native NV12 refresh

The tested NV12 workspace images were installed on SD after the coordinated
source commits: Scarlet `c8e163dd`, SGFX `5ea41a0`, ScarletUI `b3c7a6d3`,
Switch `f266812`, and Chromebook compatibility `179b112`. The packaged kernel
matched the tested release ELF, and the CPIO contained the final NV12 player.
The rootfs player had already been overwritten from `/old_root` and its
SHA-256 verified on the guest.

Hekate SD UMS exposed `disk12s1` with the expected MBR layout. Installation of
the two profiles passed SHA-256 readback for all 14 files and unchanged hashes
for all 29 protected files. Only `switchvisor` and `scarlet (console)` remained
in the L4T menu. The whole `disk12` was safely ejected. No backup copies were
created. This records SD transfer verification; a boot from these newly copied
SD files has not been observed yet.

| Artifact | SHA-256 |
| --- | --- |
| Kernel ELF | `60e8fcd25f1d9ff02c4a24a865aef639cfe09b4053d24e77c263d65f5b4d8225` |
| `Image` | `69d2e6dce088679043a11e640860b942da9660bc923eefbc8d9f630214c7b0a2` |
| `uImage` | `31cd6a28f5457bdf1d4c67e73015d31b7e95a7e081759ede2de7174be9d9a976` |
| `initramfs` (legacy RAMDisk wrapper) | `080c497b436a0e907e81109615fcb46d68c224c181dc6fd1df78edf0636c2108` |
| Player in CPIO and rootfs | `47fb57e18bcb700dd9370953c15320e0804b9bb13917a83de2df4aeadc6d85e1` |

Evidence: `.cache/nv12/sd-install-dry-run.log`, `.cache/nv12/sd-install.log`
and `projects/aarch64-switch-console/.scarlet/sd-installation.json`.
The `scarlet (console)` entry loads the updated images directly from SD;
`switchvisor` continues to use the matching host-supplied USB bundle.

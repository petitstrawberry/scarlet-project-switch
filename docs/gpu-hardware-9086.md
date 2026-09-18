# IMG_9086: GPU discovery remains deferred

The user recorded `IMG_9086.MOV` after installing the diagnostic candidate
whose kernel is `f717b19911c74424451993e723e59de161502aaa`, ELF SHA-256
`d8afc6b083ae8a08b3574bf90355107d56ba6c6eddca53ba88cb1bf3da84f2db`.
The preceding successful SD installation is retained in
[gpu-sgfx-render-verification.json](gpu-sgfx-render-verification.json).

The video is 30.762 seconds long. Upright frames were inspected at 2 fps, with
an additional 12-fps reading from 9 to 14 seconds. Local derivative images
and OCR are under `.cache/video-9086/`; the OCR is unreliable for these logs,
so the observations below come from visually reading the frames.

## Observations

- Around 10 seconds, Joy-Con registration succeeds, followed by GPU register
  mappings and `[probe] deferred Standard Devices device: gpu`.
- Around 10–12 seconds, the GPU is retried repeatedly. Touchscreen registration
  succeeds, and the RTC reports a wall-clock seed. These messages do not
  establish successful input operation or RTC accuracy.
- Around 13–15 seconds, two boot framebuffer registrations appear. This does
  not establish native DC adoption; rotated simple framebuffers also allocate
  landscape shadow buffers.
- Around 23–30 seconds, the Scarlet Shell console GUI and continuing SWS log
  text are visible. A full GUI is present rather than the previously reported
  uniform screen. This does not establish hardware SGFX rendering or the
  intended opaque window-B diagnostic console. No successful native DC or
  GM20B hardware admission has been established from this recording.

## Firmware ordering

Initial device discovery and all existing deferred-probe retry passes happen
before global VFS initialization and initramfs mounting in `start_kernel`.
GM20B reads firmware during probe and returns `PROBE_DEFER` when that global
VFS does not exist. The installed kernel never retries the queue after the
mount. The repeated three MMIO mappings in the recording are consistent with
the probe reaching this firmware wait before making GPU power changes.

Chromebook avoids this firmware dependency during probe. Its Adreno probe
registers a cold backend, and `A618Backend::query_info` later invokes
`ensure_hardware_ready`, which reads GMU/SQE firmware through the global VFS.
The CoachZ project copies firmware into both its initramfs and persistent
rootfs at `/system/scarlet/lib/firmware/qcom`. The kernel's global VFS retains
the initramfs namespace even after userspace changes its root. Switch instead
packages firmware directly at `/lib/firmware/nvidia/gm20b`; all 16 pinned files
were verified at that path in its installed and new initramfs.

The correction uses Scarlet's existing deferred queue after root-filesystem
setup, without rescanning bound devices. The common change is in draft
[Scarlet PR #563](https://github.com/petitstrawberry/Scarlet/pull/563).
GM20B now loads firmware before creating register mappings and prints
`gm20b: firmware loaded; initializing hardware` once that phase succeeds.
Hardware initialization remains in probe; this does not port Chromebook's
lazy backend architecture.

The expected next sequence is:

```text
[boot] Retrying deferred devices after root filesystem initialization...
[probe] retrying deferred Standard Devices device: gpu
gm20b: firmware loaded; initializing hardware
gm20b: powering GPU; ...
```

The new candidate passed production compilation and package inspection;
[gpu-initramfs-retry-verification.json](gpu-initramfs-retry-verification.json)
records its hashes. It was copied to the FAT32 SD; all 12 file readbacks and
38 protected-file hashes matched, and the SD was ejected. Physical testing
of this retry candidate is now recorded in [IMG_9087](gpu-hardware-9087.md):
the retry, firmware decoding and GMMU checks pass; initial FIFO binding and DC
active readback fail. DC adoption and genuine GPU rendering remain unverified.

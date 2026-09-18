# IMG_9088: native DC publishes; pixels remain faulty and FIFO completion stalls

The tested FAT32 installation is retained in
[gpu-fifo-bar1-gen2-verification.json](gpu-fifo-bar1-gen2-verification.json).
It uses board source `093c11e35d7d6a040e61c4790f9576ac9e1cf644`, Scarlet kernel
`faac004cc3614ea8b192dbd7f8303cafe33a6a14`, and kernel ELF SHA-256
`a2005ed8554f922e0eb97da7ab2c68cc2b7cc912357b64752defb3ab7853e04b`.
All 12 SD file readbacks and 38 protected-file hashes matched before eject.

The video is 37.5517 seconds long, 1920x1080 at approximately 60 fps, SHA-256
`c198737e9c2b76db34e0445728cb22b339cb2b30a3ae52adbdfa4429d7ed3012`.
Full-resolution upright frames were inspected at 2 fps, with additional 60-fps
readings around the GPU submission. Local derivatives are under
`.cache/video-9088/`. Values below were read visually.

## Display observations and user report

Around 19.5 seconds the native driver completes activation/readback and reports:

```text
tegra-dc: native scanout active; 1280x720, two buffers, hardware rotation
tegra-dc: keep_bootcon; boot logs remain visible in window B
```

GraphicsManager publishes native device 9, `/dev/fb0` and `/dev/display0`.
The optional diagnostic view continues showing boot and mirrored application
logs. At the transition, there is a hard vertical division and overlapping old
text on the left; this is an observation, not a diagnosed DC mechanism.

Around 36.5 seconds the driver samples an SWS CPU frame:

```text
tegra-dc: frame=16 CPU addr=0x17eca7000 pitch=5120 rgb=0x1d2024 varied=575/576
```

The user additionally reports that input changes the screen to white/gray,
with possible garbage at the display edge. Thus native registration, accepted
register state and varied CPU buffer samples are confirmed, while correct
physical image traversal, colors and edges are not. The diagnostic window is
intentionally opaque, so this recording cannot establish a correct full GUI.
The reported physical faults must not be dismissed on that basis.

## GPU observations

Post-initramfs retry and firmware decoding continue to work. GPU power/identity
and GMMU BAR1 read/write/remap pass. Unlike IMG_9087, initial FIFO runlist
activation completes and the driver reaches its first host-method submission:

```text
gm20b: FIFO submitting put=1 sequence=0x53474631
gm20b: FIFO bind=0x00000000 reason=UNKNOWN
gm20b: FIFO context bar1=0x10000003 inst=0x80176a5f sched=0x00000000 chsw=0x00000000
gm20b: FIFO USERD get=0 ref=0xffffffff fence=0x00000000
```

The first private submission times out around 24 seconds. FIFO/PBDMA/bind and
scheduler error status are zero, but USERD GET/reference and the fence have
not advanced. Random PBDMA progress fields without a confirmed loaded channel
are not evidence of executed methods. Probe stops before GR boot, the physical
shader/copy checks or Ready SGFX admission. SWS CPU presentation therefore
does not demonstrate GPU rendering.

## Next candidate

The user prioritizes DC correctness before further GPU work. The
[portrait-pitch candidate](dc-portrait-pitch-verification.json) retains the
ordinary 1280x720 CPU rendering interface, then converts complete frames into
two private 720x1280 BGRA scanout buffers. Window A uses pitch 2880, no
SCAN_COLUMN, no inverted direction and zero offsets, matching the inspected
[Hekate portrait fetch](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/di.inl#L432).
Native activation, VBlank retirement, rollback and lifetime protection remain.

This isolates the unverified landscape traversal while leaving the actual
SWS-produced content in the experiment. Extra per-frame CPU work is expected;
the root cause is not established by this change. GPU direct landscape scanout
is a separate, still-unverified path. No pending GPU memory/runlist edits are
included in this DC image.

Bounded logs report the target address, pitch/options, cumulative A/B underflow
counts and the shared MC fault latch without acknowledging it, following
[NVIDIA underflow handling](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/dc.c)
and [Linux MC register definitions](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/memory/tegra/mc.h).
The build and package checks pass. The subsequent portrait candidate was
installed to the FAT32 SD with all 12 readbacks and 38 protected-file hashes
verified, then ejected. Physical confirmation of correct GUI colors, orientation,
input updates and all four edges remains pending.

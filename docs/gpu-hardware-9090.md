# IMG_9090: direct DC column scan underflows

The user reports that the direct hardware-rotation candidate again shows the
previous gray screen. The accompanying `IMG_9090.mov` records the diagnostic
boot, with the original portrait console visible in opaque window B.

The installed image is board `2c87a15d5bed410b1277e6d57cf167de7c981a48`,
Scarlet `faac004cc3614ea8b192dbd7f8303cafe33a6a14`, ELF SHA-256
`7bb28b2c5ddd1eabb316e6506175267130d4f31caa9836c1fb9dd3f588a27461`.
Its 12 SD readbacks and 38 protected-file checks remain recorded in
[the direct-rotation receipt](dc-direct-rotation-verification.json).

## Observed display boundary

Around 15–16 seconds, native device 9 initializes and publishes successfully.
Its CPU buffers are `0x17e8a7000` and `0x17eca7000`. Active scanout reports
pitch 5120, options `0x40000011`, offsets 5116/0 and buffer strides 0/0.
This establishes register activation, not correct physical pixels.

The first frame has RGB `0x000000`, with 53/576 sampled pixels differing.
At approximately 27.5 seconds, frame 8 has RGB `0x1d2026`, with 575/576
sampled pixels differing. The ordinary CPU-produced image is not uniform.

Window A's underflow counter increases from `0x5` at the initial native flip
to `0x2cf` at frame 8. Window B's counter remains zero and its logs remain
readable. The direct column-scan path therefore has continuing fetch underflows;
the gray GUI report cannot be explained solely by an empty producer image.
The shared MC latch reports status `0x40` and low address `0xf5a71600` in both
observations. It is not acknowledged by this driver and does not independently
establish a new fault at each observation. The wrapped error-status digits are
not transcribed as an exact value.

The preceding working portrait-pitch image, IMG_9089, has an A counter of
`0x1` at frame 8, despite its nonzero shared MC latch. The difference in ongoing
A underflows is more useful here than the repeated MC latch alone.

## Other visible boundaries

Around 21 seconds the private GPU host-method push still times out with GET 0,
reference `0xffffffff` and fence 0. No successful GR/SGFX execution is shown.
SWS continues and requests window lists later in the recording. APs 1, 2 and 3
eventually report their scheduler online and local timer ready, although earlier
scheduler-online timeout/spin-contention warnings are visible. These are
observations, not an additional SMP fix or proof of timer suspend/resume.

## Next DC-only candidate

Retain the same pitch-linear buffers, Normal-NC aliases, source cursor and
hardware-rotation geometry while adding Linux/NVIDIA's native DC fetch priority.
Hekate initializes `MEM_HIGH_PRIORITY` (`0x403`) and its timer (`0x404`) to zero;
Linux Tegra and NVIDIA T210 initialization use window thresholds `0x20` and
timers 1. The driver previously never adopted that native policy.

Only claimed A, and B for the diagnostic view, fields will be changed. Save,
active readback and rollback cover those fields. Logs will include the inherited
priority, underflow increments and read-only MC latency-allowance registers.
Whether this eliminates the underflows requires another physical boot.
No CPU rotation or block-linear upload is added, and GPU changes remain held.

Block-linear support is not established as a requirement by this recording.
Although Hekate defines a block-linear column-scan configuration, current Nyx
uses VIC rotation; its immediate predecessor used portrait pitch with a CPU
coordinate conversion. An available configuration array is not evidence that
current Nyx actually uses that path.

## Evidence and primary references

Input: `/Users/petitstrawberry/Downloads/IMG_9090.mov`, 47,363,176 bytes,
28.393333 seconds, HEVC 1920x1080 at 60000/1001 fps. SHA-256:
`fc110b42f57d94949668f131b9adb562d8d9bc21e3eacac74d0dee820d1e7471`.
Derivatives are `.cache/video-9090/metadata.json`, `contact.jpg`,
`detail/dc-03.jpg`, `detail/userspace-01.jpg`, `detail/userspace-08.jpg`
and `detail/27.5s.jpg`. Selected frames were visually read at original resolution.

- [Linux native DC initialization](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/dc.c#L2213) and [priority field definitions](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/dc.h#L316).
- [NVIDIA T210 priority initialization](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/dc.c#L5533) and [rotation-aware bandwidth/latency handling](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/video/tegra/dc/t21x/bandwidth.c#L250), fetched with `gh`.
- [Hekate bootloader priority defaults](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/di.inl#L26), [current Nyx VIC path](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/nyx/nyx_gui/frontend/gui.c#L90) and [pre-VIC consumer](https://github.com/CTCaer/hekate/blob/9d889e2c3e588c3b1b71ebd4a2f0482d28e91660/nyx/nyx_gui/frontend/gui.c#L288), fetched with `gh`.

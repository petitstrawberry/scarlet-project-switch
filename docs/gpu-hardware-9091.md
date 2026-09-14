# IMG_9091: native fetch priority does not fix column scan

The user reports that the fetch-priority candidate still shows the gray GUI.
`IMG_9091.MOV` records its diagnostic boot with readable original portrait
scanout in window B. The tested board source is
`1fd5799618e036a6c6e93e69965e21c5791c8e96`, Scarlet
`faac004cc3614ea8b192dbd7f8303cafe33a6a14`, ELF SHA-256
`1b8b091e9a1fdd56d509acfc0af7dcc6b05d8fd65d43740980d98b3ac9fce7f1`.
The [installation receipt](dc-fetch-priority-verification.json) retains its
12 boot-file hashes and 38 protected-file checks.

## Observations

At approximately 21–22 seconds, device 9 initializes and publishes. Its buffers
are `0x17e8a7000` and `0x17eca7000`. Active state still reports pitch 5120,
options `0x40000011`, offsets 5116/0 and buffer strides 0/0.
Priority reads back as `0x00202000 / 0x00010100`: the newly requested A/B
thresholds and timers are active.

Inherited underflow counters are A `0x2`, B zero. The first native flip reports
A `0x4`, B zero, delta 2/0 and interrupt status `0x104`.
Around 32–33 seconds, frame 8 reports A `0x2cd`, B zero, delta 36/0 since the
preceding sampled observation. Frame 7 had A `0x2a9`, delta 6/0.
Frame 8's CPU image has RGB `0x1d2026` and 575/576 samples differing.
The priority settings have latched, but ongoing A fetch underflows remain.
They have not fixed the physical display.

The shared MC latch reads `0x40`, error `0x20105501 / 0xf5a71320` after the
first flip and at frame 8. These repeated sticky values do not establish a
fresh MC fault at each sample. Read-only latency registers report
`la-ab=0x001e001e` and both scaled values `0x001e001e`.

The GPU still times out at the first private FIFO host-method push with GET 0,
reference `0xffffffff`, fence zero and status 4, before genuine GR/SGFX
admission. Later SWS window-list traffic and AP scheduler/local-timer messages
remain visible. This recording does not validate GPU rendering or timer resume.

## Revised display path

The user reiterates that the [IMG_9089 portrait-pitch baseline](gpu-hardware-9089.md)
displayed correct content after CPU rotation, and requests following Hekate.
Current Nyx actually rotates landscape pitch input by 270 degrees in VIC,
waits for completion, then gives DC portrait pitch output. Its DC configuration
uses pitch 2880, offsets zero and `WIN_ENABLE` without `SCAN_COLUMN`.
Implement that existing hardware path instead of treating another column-scan
or bandwidth adjustment as a validated fix. The failed column-scan candidates
remain historical evidence; the unfinished block-linear proposal remains held.

## Evidence and references

Input: `/Users/petitstrawberry/Downloads/IMG_9091.MOV`, 68,014,575 bytes,
39.018333 seconds, HEVC 1920x1080 at 60000/1001 fps. SHA-256:
`93638c9451802610db6458a2d6cf2192c705f51c813dc1c5560427fbaf46dc5a`.
Derivatives: `.cache/video-9091/metadata.json`, `contact.jpg`, and
`detail/frame015.jpg`, `frame016.jpg`, `frame024.jpg`, `frame036.jpg`,
`frame038.jpg`. Selected frames were read at original resolution.

- [Hekate Nyx consumer](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/nyx/nyx_gui/frontend/gui.c#L90), [VIC implementation](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/vic.c#L394), and [portrait-pitch DC configuration](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/di.inl#L459), fetched with `gh`.
- [Linux native priority initialization](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/dc.c#L2213). This is a register-policy reference, not proof of working 90-degree pitch column scan.

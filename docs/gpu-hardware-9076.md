# Switch boot video IMG_9076

Source: `/Users/petitstrawberry/Downloads/IMG_9076.mov`, 4.171667 seconds,
1920×1080 HEVC. SHA256:
`e384a18f6bce126ee544b289f2a6818543b4fe312c6006232db2df665f72d75b`.

Frames were extracted with ffmpeg at 10 fps into `.cache/video-9076/`.
`detail-*` frames retain the original cadence from 1.95–2.25 seconds.
The readings below are visual; timestamps are approximate. The original video
was not changed. Candidate attribution follows the last verified SD deployment
in `gpu-verification.json`; the video does not display an artifact hash.

| Frame | Visible evidence | Scope |
| --- | --- | --- |
| `frame-008` | `maxcpus=4`, `Detected 4 CPU(s)` | Selected CPU topology. |
| `frame-016` | `PLLX ready; CPUs=0xf current=1017600 kHz` | CPU frequency backend probed. |
| `frame-020` | `Right Rate -> Ready`; first HID `id=0x30`; `/dev/gamepad0` registered | Right Joy-Con received physical HID. |
| `frame-021`, `detail-007` | `Left Rate -> Ready` | Left initialization completed; its first-HID line is partly erased in `detail-008`, so its fields are not transcribed. |
| `frame-022` | `gm20b: powering GPU; rail=1000000uV ref=38400000Hz pwr=204000000Hz`; `GPU MC flush release timeout` | GPU power sequence returned successfully; initial MC acknowledgement succeeded, but the added STATUS-clear wait failed before reading GPU identity. |
| `frame-022` | `[rtc] wall clock seeded from device` | RTC seed succeeded in this boot; time accuracy and repeated boots remain unmeasured. |
| `frame-024` | `/dev/touchscreen0 ready, ten contacts` | Touch initialization completed; no physical touch interaction is shown. |
| `frame-040` | AP 1/2/3 `scheduler online; local timer ready`; `SMP schedulers online: 4/4 CPU(s)` | Four schedulers reached their online publication. |
| `frame-040` | Deferred CPU frequency transition begin `pstate=6 freq_khz=710400`, followed by complete | The driver reports applying a 710400 kHz request. No post-transition frequency query or sustained frequency/timer measurement is shown. |
| `frame-042` | Graphical background and pointer | Framebuffer session starts; video ends before Home or interactive application checks. |

GPU probe failed. `gm20b: identified` and a registered GPU endpoint were not
observed. The video reaches the release timeout only after successful rail,
CAR/clamp/reset readback and MC flush acknowledgement. This is register-level
progress, not an independent voltage or clock measurement. No rollback-error
line is visible in the sampled frames.

The previous driver interpreted MC STATUS bit 2 as a release acknowledgement.
[Linux's common MC unblock function](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/memory/tegra/mc.c)
and [Switchroot's `tegra_mc_flush_done`](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/platform/tegra/mc/mc.c)
clear the CTRL request without waiting for STATUS to clear. Both sources were
retrieved with `gh`; the mainline file matches the previously cached source.
The corrected driver retains the bounded flush-acknowledgement wait, checks
that CTRL bit 2 actually clears, and prints CTRL/STATUS before GPU identity.

The corrected package is recorded separately in `gpu-mc-verification.json`.
Its hardware result is pending. The next boot should show
`gm20b: MC flush complete ctrl=... status=...`, then `gm20b: identified`, or
the next precise failure. SGFX execution remains unimplemented. Four-core
task execution, per-core timer delivery, sleep/wake, user-operated input and
repeated CPU frequency changes still require physical verification.

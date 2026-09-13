# Switch boot video IMG_9081

Source: `/Users/petitstrawberry/Downloads/IMG_9081.mov`, 4.005 seconds,
1920×1080 HEVC, approximately 57.85 fps. SHA256:
`e9df4ce76113b495e2941bde2fa5067f9a6353a5f1d1e010a63ece773997003d`.

Frames were extracted at 10 fps into `.cache/video-9081/`; `late-*` keeps
the original cadence from 2.1–2.75 seconds. Readings are visual and times
are approximate. Candidate attribution follows the eight-file SD readback
in `gpu-gmmu-display-verification.json`: Image SHA256
`2e585abce986263cc0f5cd8ae4256d20ccf4ebe98cf6cf499ddc3d661bf21134`.
The recording itself does not display an artifact hash. The source is intact.

| Frame | Visible evidence | Scope |
| --- | --- | --- |
| `frame-001` | BSP boot probe, EL2, entry to Scarlet Linux Image bootstrap | The boot stack reached the kernel candidate. |
| `frame-010` | CPU frequency backend ready, CPU mask `0xf`; Right Joy-Con handshake progresses | Provider initialization occurred; no frequency measurement or user input is established. |
| `frame-020`, `frame-022` | GPIO6 changes `0x02` to `0x09`, ALT6 stays zero; MC flush complete; `MC_BOOT_0=0x12b000a1 read_us=15` | The previously corrected GPU power/identity path still succeeds. |
| `frame-022` | `GMMU MC smmu=0xffffffff gpu_asid=0xffffffff`, followed by `GPU is attached to an inherited Tegra SMMU domain` | The new guard treats all-ones MC reads as an enabled domain and rejects probe. BAR1 binding and its memory check are not reached. |
| `frame-022` | MC flush complete after failed GPU probe | The rollback drain path ran; this does not independently prove electrical isolation or absence of all outstanding transactions. |
| `frame-022` | RTC wall-clock seed; touch sensing enabled and touchscreen ready | Initialization succeeded in this boot, without interactive or accuracy validation. |
| `late-012`, `frame-027` | DC0 and DC1 deferred at Late Initialization; graphics manager registers ordinary framebuffer devices | No `tegra-dc: inherited` or native activation message is visible. Firmware framebuffer fallback remains in use. |
| `frame-030`, `frame-033`, `frame-037` | Init loaded; AP1 controller initialization done, followed by AP3 controller initialization done | Startup proceeds after GPU rejection. The final four-core summary and sustained timers are not established by these frames. |
| `frame-040` | Pointer plus partially replaced boot text, including a diagonal update boundary | Graphical drawing has begun. The recording ends before a stable Shell frame, further updates or a fatal diagnostic. This is insufficient to classify a hang. |

## Result

Private GMMU validation failed before publishing any GPU page-table address.
Native DC adoption was deferred before its probe body. Neither new hardware
stage is validated by this recording. The existing GPU identity read remains
good, and init/AP startup continues after the rejected GPU probe.

The Noble DTB gives DC0/DC1 `pinctrl-0` references to PMC DSI pad states.
The common pre-probe pass requires a registered PMC pin controller for these
states, which this BSP does not yet provide. The native driver is intended
to preserve those inherited pads, so its adoption binding must avoid asking
the common pass to change them. The all-ones SMMU reads need separate analysis
against the Tegra memory-controller and secure-firmware interfaces; they
must not be interpreted as an observed, valid enabled domain.

SGFX execution support remains zero. The 4-second recording does not establish
normal Shell stability, native rotation, BAR1 DMA, input operation, or timer
recovery. The common earlyfb handoff is in draft upstream PR
[#559](https://github.com/petitstrawberry/Scarlet/pull/559), stacked on #558;
its native hardware path was not reached here.

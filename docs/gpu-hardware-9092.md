# IMG_9092: VIC timeout followed by ordinary framebuffer fallback

The later visible Scarlet Shell does **not** establish successful VIC/DC
adoption. The video shows FCE initialization, first-frame composition timeout,
failed native DC probe, and then registration of the ordinary boot framebuffer
as `fb0`. Native display double buffering is not active in this boot.

## Input and installed identity

- Video: `/Users/petitstrawberry/Downloads/IMG_9092.mov`.
- Size: 61,635,385 bytes; duration: 37.068333 seconds.
- HEVC, 1920x1080, 60000/1001 frames per second.
- SHA-256: `e14c328ca037db485be9c55515be2ab09b16e7f07151c8d88cc15c660984343d`.
- Installed board source: `b71de486ed228ef144bb5a65bd71622b0d5bd098`.
- Scarlet source: `faac004cc3614ea8b192dbd7f8303cafe33a6a14`.
- ELF SHA-256: `3811e40fb7f434be69579d3a8e027e18aceb9ed71048dcb652d91ae1cdb7cd6f`.
- Artifact and SD identities: [VIC receipt](dc-vic-rotation-verification.json).

The initial user report, "でた。", was incorrectly interpreted as VIC success
before this video was inspected. The receipt now records the failed native
attempt and successful fallback separately.

## Observations

The extracted frames are in `.cache/video-9092/`; timestamps below are
approximate video times, not kernel timestamps or performance measurements.

- `detail/frame-001.jpg`, about 14.25 s: boot arguments include `maxcpus=4`
  and `keep_bootcon`; inherited framebuffer is 720x1280, pitch 2880,
  rotation 3.
- `vic-detail/frame-041.jpg` and `frame-042.jpg`, about 20.5–20.6 s:
  DC inherited state and four newly allocated buffers are logged, then
  `tegra-vic: FCE active` and initialization of graphics device 9. The first
  CPU source is `0x17e8a7000`, pitch 5120, with nonuniform sampled pixels.
- `vic-timeout/frame-006.jpg`, about 20.7 s: the frame-completion timeout
  diagnostic and failed-before-DC-flip messages appear during the console
  clear. The complete idle/FCE/parsed-state values cannot be reliably read
  from this transition; they are not transcribed as hardware facts.
- `detail/frame-014.jpg` and `frame-015.jpg`, about 20.75–21.25 s:
  `Failed to probe Late Initialization device dc@54200000: VIC
  composition/parameter timeout`, followed by initialization of graphics
  device 10, configuration 1280x720, framebuffer `0x17e8a7000`, and
  `Registered framebuffer resource: 10 -> fb0`.
- `detail/frame-045.jpg`, about 36.25 s: the ordinary Scarlet Shell artwork
  is visible, with application logs drawn over the same surface.

The fallback shadow can reuse an address released by failed native adoption.
The matching source address alone is therefore not proof that native DC
registration succeeded; the error and subsequent source device ID matter.

## Current rendering path

The board's DT describes the inherited Hekate scanout as
`simple-framebuffer`, 720x1280, pitch 2880, at `0xf5a00000`, rotation 3.
After failed native adoption, Scarlet's ordinary
`kernel/src/drivers/graphics/simple_framebuffer.rs` allocates a single
1280x720 landscape shadow and publishes `/dev/fb0` and `/dev/display0`.
Presentation rotates damaged pixels by CPU into the inherited physical
front. This driver does not expose a display swapchain; the default
`scanout_buffer_count()` is zero.

SWS CPU composition has its own backbuffer, but copying that frame into a
single physical front is not a DC front/back page flip. `keep_bootcon` also
keeps earlyfb writers active on the original surface, explaining the shared
GUI/log drawing. The native driver's separate diagnostic window B was not
successfully adopted in this attempt.

No successful VIC composition, native alternating active addresses, measured
frame rate, GM20B shader execution or SGFX admission is established by this
video. Earlier FIFO failure evidence remains in [IMG_9091](gpu-hardware-9091.md).

## Linux reference found during review

Switchroot Jammy/Noble's actual build uses theofficialgman's NVIDIA fork.
Its [rotation helper and flip override](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/ext/dev.c#L432)
use DC SCAN_COLUMN plus horizontal/vertical inversion without VIC composition.
See [the source distinction](display-bringup.md#linux-reference-distinction).
This establishes an existing Linux DC rotation path, not equivalence of
Scarlet's failed pitch-column layout, memory bandwidth or inherited state.

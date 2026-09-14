# IMG_9093: direct-pitch column adoption rejected by underflow

The DC activation and uncompressed CDE values latch, but window A continues
to underflow. Native registration is rejected by the new initialization
fetch check. The later visible Shell is ordinary simplefb fallback, not
successful native DC rotation, double buffering or SGFX rendering.

## Input and installed identity

- Video: `/Users/petitstrawberry/Downloads/IMG_9093.mov`.
- Size: 35,150,985 bytes; duration: 20.970000 seconds.
- HEVC, 1920x1080; nominal frame rate 60000/1001, variable actual rate.
- SHA-256: `1142fe14f1e2cc9fd3e2ed0a0264ea7ee8edb879e98e2a0c4fb2dd4f37732fab`.
- Installed board source: `94b6e0ea3c2d6e366215a8f93bd2550dae463984`.
- Scarlet source: `faac004cc3614ea8b192dbd7f8303cafe33a6a14`.
- ELF SHA-256: `9f45efe445e95116ce423d00c142a3fcafa370d0572299bdaf6ad55d3d4713aa`.
- Exact package and SD identities: [direct-pitch receipt](dc-linux-column-verification.json).

## Observations

Full frames are in `.cache/video-9093/`, sampled at four frames per second.
`dc-detail/` contains every source frame from the 6.05–6.50 s transition.
Times below are approximate video times, not a frame-rate benchmark.

- `frame-001.jpg`, about 0.125 s: inherited boot framebuffer is 720x1280,
  pitch 2880, rotation 3. Boot arguments retain `maxcpus=4` and
  `keep_bootcon`; the kernel detects four CPUs.
- `frame-017.jpg` through `frame-020.jpg`, about 4.1–4.9 s: right JoyCon
  reaches Ready and registers a gamepad endpoint; FTM4 reports its touch
  endpoint ready. RTC seeds the wall clock. These startup lines do not
  establish sustained input or RTC reliability.
- `dc-detail/frame-011.jpg`, about 6.23 s: inherited DC activation is
  `0x00000154`, CDE is already `0/1`, and A/B underflow counters are `2/0`.
  CPU buffers are allocated at `0x17e8a7000` and `0x17eca7000`.
- `dc-detail/frame-013.jpg` through `frame-017.jpg`, about 6.27–6.33 s:
  the first CPU source at `0x17e8a7000`, pitch 5120, has 64/576 nonuniform
  sampled pixels and sample hash `726b4d59`. Active A uses that address,
  options `0x40000011`, H/V offsets `5119/0`, kind zero, buffer/UV stride
  zero, and CDE `0/1`. Activation changes to `0x00000140`; owned A/B
  H-counter mask `0x14` is clear. Priority threshold/timer readback is
  `0x00202000/0x00010100`.
- The next initialization fetch diagnostic is exactly:

  ```text
  tegra-dc: adoption fetch A=0x3->0x4 B=0x0->0x0 delta=1/0
  ```

  Native probe then fails with `DC column scanout underflows; keeping
  firmware framebuffer`. The register checks passing did not make the
  pitched column fetch usable.
- `frame-026.jpg` through `frame-040.jpg`, about 6.4–9.9 s: ordinary
  framebuffer source device 10 becomes `fb0`, reusing released shadow
  address `0x17e8a7000`. Device 13 subsequently becomes `fb1` from another
  inherited simple-framebuffer node. Neither is the failed native device 9.
- `frame-048.jpg`, about 11.9 s: the first private GPU FIFO host push still
  times out, GET zero, reference `0xffffffff`, fence zero, status 4. GR and
  authenticated SGFX Ready admission are not reached.
- `frame-070.jpg`, about 17.4 s: the ordinary console-mode Shell artwork
  is visible with logs over the surface. SWS reports scale 1000 and a
  1280x720 shared-memory Shell frame. This is the simplefb CPU fallback.

The shared MC sticky status is zero in the relevant DC diagnostics. Its
unacknowledged error fields may be old values; they are not evidence of a
new memory-controller fault. The scaled display latency allowance remains
`0x001e001e`. No native address alternation, frame rate, GPU rendering or
suspend/resume success is established by this video.

## Next comparative image

The new [block-linear candidate](dc-block-linear-verification.json) changes
DC input storage to uncompressed Tegra 16Bx2, block-height log2 4, DC kind
`0x42`. Ordinary application buffers remain linear; the driver uploads
unchanged image coordinates into an inactive private DC buffer. DC still
performs the rotation. VIC is not linked and the genuine GPU admission gate
is unchanged.

This is a surface-layout comparison, not a claim that pitch-column rotation
is universally unsupported or that block-linear will resolve Scarlet's
underflow. Linux's rotation flags alone do not establish allocation-layout
or bandwidth equivalence. The subsequent
[IMG_9094 result](gpu-hardware-9094.md) passes native publication and measures
the first full-frame upload at 27,284 microseconds. That upload remains an
intermediate copy; see [display implementation](display-bringup.md).

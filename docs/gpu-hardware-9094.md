# IMG_9094: native block-linear DC adoption passes, storage upload is slow

The installed block-linear comparison reaches native DC publication. Window A
uses private block-linear storage, and its first active fetch diagnostic adds
no A/B underflows. The original diagnostic console remains opaque in window B,
so the clip does not establish the underlying GUI pixels. The user separately
reports apparently working output, no obvious tearing, and severe slowness.
The first storage upload alone takes 27,284 microseconds.

## Input and installed identity

- Video: `/Users/petitstrawberry/Downloads/IMG_9094.mov`.
- Size: 71,349,083 bytes; duration: 28.678333 seconds.
- HEVC, 1920x1080; nominal frame rate 60000/1001, variable actual rate.
- SHA-256: `4488a090b7df93ef935eb891413655106d61b6691b1f5295135523ad4b1d7b6d`.
- Installed board source: `eaff47fe84082040bda7d426fcefe1c788bc4386`.
- Scarlet source: `faac004cc3614ea8b192dbd7f8303cafe33a6a14`.
- ELF SHA-256: `5565a36a187528efadbdb82cd393a321dd3542e4c3c62069083fd027623068f1`.
- Exact package and SD identities: [block-linear receipt](dc-block-linear-verification.json).

## Observations

Full frames are in `.cache/video-9094/`, sampled at four frames per second.
Frame references identify evidence, not a performance benchmark.

- `frame-036.jpg`: RTC seeds the wall clock, and FTM4 reports its ten-contact
  touchscreen endpoint ready. These startup lines do not establish sustained
  input or RTC reliability.
- Inherited A uses portrait boot address `0xf5a00000`, kind zero, continuous
  720x1280 mode. A/B underflow counters are `8/0`, activation is `0x00000454`,
  and uncompressed CDE is already `0/1`.
- Ordinary linear render buffers are at `0x17e8a7000` and `0x17eca7000`,
  3,686,400 bytes each. Private block-linear DC buffers are at `0x1750c0000`
  and `0x1754c0000`, 3,932,160 bytes each.
- The first upload is exactly:

  ```text
  tegra-dc: upload=1 src=0x17e8a7000 dst=0x1750c0000 kind=0x42 block-height=16 padded=3932160 elapsed=27284us matched=576/576
  ```

  CPU source samples are varied at 75/576, hash `0383b624`. Pixel comparisons
  establish agreement with the source samples in private storage; they do not
  establish the panel's pixel content.
- Active A reads private address `0x1750c0000`, pitch 5120, options
  `0x40000011`, H/V offsets `5119/0`, kind `0x42`, CDE `0/1`. Activation is
  `0x00000440`, with the owned A/B H-counter mask `0x14` clear. A/B counters
  remain `8/0`, delta `0/0`; priority readback is `0x00202000/0x00010100`.
  Shared MC sticky status is zero, and scaled display LA is `0x001e001e`.
- `frame-037.jpg`: `keep_bootcon` retains window B. Graphics device 9
  initializes as 1280x720 and publishes linear application buffer
  `0x17eca7000`; `Registered framebuffer resource: 9 -> fb0` follows. Device 9
  is the native DC device. Unlike IMG_9093, this is successful native
  registration rather than address reuse by simplefb after failed adoption.
  The later initialization fetch check must have passed to reach this line;
  its separate successful transcript is not legible across the console clear.
- `frame-045.jpg`: native DC is successfully probed and ordinary display/fb
  endpoints are present. Inherited simple-framebuffer probes do not replace
  the native source-device registration.

Window B covers A in the diagnostic video. The clip therefore does not prove
correct visible GUI orientation, active private-address alternation, sustained
underflow freedom, or tearing behavior. The user's subsequent report provides
separate visible-output evidence; it is not an instrumented result. The
conversion measurement is one initial upload, not total presentation latency
or an FPS estimate. No new GPU Ready admission is established.

## Checkpoint and next path

Board commit `c8591773802c481c34452eb0f540e7c84d491782` separately makes ordinary
linear render aliases Normal cached while leaving private DC storage Normal-NC.
Its production kernel/package build passed (ELF SHA-256
`333f49913dc6b60aba4fdf01727fccddc5f610da18e27e191e13f5c6d0f13c32`), but it
was not installed or physically tested. It must not be confused with the
installed source and receipt above.

At the user's request, further CPU storage-conversion tuning stops here.
The next target is for DC to fetch the actual completed render image directly,
without a per-present intermediate upload. Pitch-column admission still needs
its missing Linux memory-bandwidth/latency policy investigated; direct
GPU-produced compatible storage additionally needs resource-layout integration
and genuine GPU Ready admission. See [display implementation](display-bringup.md).

# Tegra210 hardware H.264 decode

## Implementation

`scarlet-driver-tegra210-nvdec` registers the common Scarlet `/dev/video0`
backend. It boots NVIDIA's unmodified NVDEC2 firmware, submits stateless
H.264 picture parameters, and returns tightly packed NV12 output.

- One open session and one in-flight picture; distinct stream IDs on reopen.
- Progressive 8-bit 4:2:0, coded size up to 1920 × 1088, POC types 0 and 2.
- Stable picture and DPB slots, P/B references, multiple slices, SPS cropping.
- Private noncacheable DMA buffers; host1x OP_DONE syncpoint retirement before
  reusing backing. Falcon IDLESTATE stays `0x801` after successful jobs and is
  only suitable for the initial firmware boot check.
- One-second decode timeout; reset and MC drain affect NVDEC alone. Backing
  is retained if isolation cannot be proven. A failed session must be reopened.
- Completion is polled by the existing video client. No decode test runs at boot.

The present path converts NVDEC block-linear surfaces to linear NV12 on the
CPU. It does not yet expose decoder surfaces directly to SGFX. Other codecs,
interlacing, POC type 1, slice groups, and custom SPS/PPS scaling matrices are
not supported by this path. Speaker playback is covered separately in
[audio bring-up](audio-bringup.md).

Scarlet commit `1eddc988` fixes POC type 2, initializes absent scaling
matrices to 16, rejects unsupported SPS matrices rather than silently discarding
them, and advertises session commands when the concurrency limit is one.

## Build and integration

The console project enables the driver. `bundles/nvdec-player.toml` selects
`h264-stateless-hw` and `mp4-aac` for `/bin/video-player` and restores its
application catalog entry in the filtered console image. The disk root image
includes the same bundle after the full distribution bundle. `cargo scarlet
update` resolved both player layers with exactly those two features.

```sh
nix develop --command cargo scarlet build \
  --project projects/aarch64-switch-console --release
nix develop --command cargo test --manifest-path tests/nvdec-qa/Cargo.toml
nix develop --command cargo test --manifest-path drivers/video/tegra210-nvdec/Cargo.toml
```

`tests/nvdec-qa` is a manually launched device test; its README and generator
describe the encoded fixtures and independent software decode references.

## Device evidence (2026-09-20)

Nintendo Switch, SCR-SWV with GDB disabled, four CPUs, SD ext2 root:

- `nvdec-qa`: **72/72** complete NV12 frame hashes matched FFmpeg software
  decode. Baseline 160 × 90 and High 320 × 180, two sessions each; cropping,
  CABAC, P/B references, three slices, and reopen all passed.
- `video-player`: the 1280 × 720 MP4 test reached `finished: 144 frames` through
  `tegra210-nvdec`, with the SGFX Maxwell renderer. The deployed binary also
  completed ten 144-frame loops; the user confirmed normal on-screen output.
- The deployed `/bin/video-player` is 2,502,168 bytes, SHA-256
  `1801b652a4f3520b70263696ad9a045553986a83e47f446408e87723b6bd5a36`.
  It was copied directly from `/old_root/bin/video-player-nvdec` and hashed
  again on the device. No backup copy was created.
- This validation uses a USB-loaded kernel; SD boot files were not updated.

Local evidence: `.cache/nvdec-linux-audit-20260920/uart-session.log`,
`qa-host-tests.log`, `driver-host-tests.log`, `kernel-build.log`,
`video-player-build.log`, and `bundle/sha256.json`.

## 1080p performance follow-up (2026-09-20)

The user reported nearly frozen video after AAC speaker playback became
functional. NVDEC was active at 408 MHz and CPU schedutil reached 1,017.6 MHz.
Sparse driver timing samples separate submission preparation, time until the
client observes decoder completion, and conversion to tightly packed NV12.
The completion measurement includes polling and scheduling latency; it is not
the decoder hardware's execution time alone.

First 128 pictures of the same 1920 × 1080 MP4, average milliseconds:

| Stage | Initial path | Reused surfaces / sector reads |
| --- | ---: | ---: |
| Submission preparation | 9.686 | 0.605 |
| Completion observation | 6.879 | 6.928 |
| CPU layout conversion | 34.116 | 8.061 |

Preparation previously allocated, zeroed, retagged and released a roughly
3 MiB DMA image every picture. Retired pictures now return their backing to
a session-local pool, bounded by the existing 17 picture slots. References
remain owned until the prior decode completes and the next DPB no longer
names them. Session teardown/resolution changes release the pool under the
same existing isolation rules.

Layout conversion now reads an aligned 64-byte sector for two rows using
integer loads. Odd/cropped/unaligned cases keep the generic implementation.
Host tests compare complete planes with byte-wise addressing and check guard
bytes. Device `nvdec-qa` again passed **72/72** full-frame hashes and reopen.
The actual MP4 completed 2,488 frames. The user reported that video advanced
more than before but remained choppy. These timings exclude userspace NV12
copying, RGB conversion, scaling, UI upload/compositing and scanout. **60 fps
presentation is not achieved or claimed.**

One audio full-ring overrun was observed under this video/UI load, followed
by automatic PCM restart; see [audio bring-up](audio-bringup.md).
Evidence: `.cache/audio-bringup-20260920/uart-audio-8.log` (initial timing),
`uart-audio-9.log` (optimized path and pixel QA), `nvdec-host-tests.log`.

## Native NV12 presentation direction (not implemented)

NVDEC already produces block-linear NV12. The CPU layout conversion exists
to satisfy the current linear-NV12 video client API. A native surface path
should retain Y/UV offsets, pitches, coded and visible extents, crop, layout,
color encoding/range and producer completion alongside an owned frame lease.
The decoder must not recycle a leased picture while display or GPU work reads it.

Tegra DC has semi-planar YUV format/CSC support in
[Linux's plane implementation](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/gpu/drm/tegra/plane.c).
[NVIDIA's window programming](https://github.com/theofficialgman/switch-l4t-kernel-nvidia/blob/7d95822acda1f6dab3f3d1d099b43ff9e0d0e626/drivers/video/tegra/dc/window.c)
covers separate Y/UV addresses, scaling, SCAN_COLUMN rotation and block-linear
surface kind. Scarlet's current DC driver already rotates and directly scans
RGB GPU buffers, but its common pixel format and admission path are RGB-only.
NVDEC's two-GOB layout differs from the current RGB H4 layout; the exact NV12
modifier, chroma alignment, crop/scaling and rotated scanout still need device QA.

The preferred full-screen path is a leased NV12 surface presented by the
compositor to a suitable DC plane, retiring the old lease at display completion.
Windowed/occluded playback needs GPU Y/UV sampling and color conversion before
normal UI composition. SGFX has R8 but currently lacks RG8/multiplanar NV12;
ScarletUI's existing external shared image adapter accepts BGRA8 only. Extend
those resource/format and completion contracts, then replace video-player's CPU
`CanvasView` path. A pixel-format enum alone does not establish plane ownership
or synchronize decoder, renderer and scanout. This work must coordinate with
the separate touch task rather than altering its input handling.

## References

- Linux v6.12, `adc218676eef25575469234709c2d87185ca223a`: Tegra Falcon/NVDEC
  boot, host1x v5 syncpoints, CAR MBIST sequence, and MC client reset.
- [NVIDIA picture layout](https://github.com/NVIDIA/open-gpu-doc/blob/9fdf5c4062007929d9f4e6cbad9c9771fe61b880/classes/video/nvdec_drv.h)
  and the adjacent `clc5b0.h` method definitions.
- [FFmpeg nvtegra H.264 implementation](https://github.com/averne/FFmpeg/blob/caeec83b791be08ed43468a2ef426d6901d51c78/libavcodec/nvtegra_h264.c)
  and its `nvtegra_decode.c` / `libavutil/nvtegra.c` support.
- Firmware provenance, unchanged image hash, and redistribution text are in
  `drivers/video/tegra210-nvdec/firmware/README.md` and `LICENSE.nvidia`.

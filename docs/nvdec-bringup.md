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
not supported by this path. Audio remains a separate bring-up task.

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

## References

- Linux v6.12, `adc218676eef25575469234709c2d87185ca223a`: Tegra Falcon/NVDEC
  boot, host1x v5 syncpoints, CAR MBIST sequence, and MC client reset.
- [NVIDIA picture layout](https://github.com/NVIDIA/open-gpu-doc/blob/9fdf5c4062007929d9f4e6cbad9c9771fe61b880/classes/video/nvdec_drv.h)
  and the adjacent `clc5b0.h` method definitions.
- [FFmpeg nvtegra H.264 implementation](https://github.com/averne/FFmpeg/blob/caeec83b791be08ed43468a2ef426d6901d51c78/libavcodec/nvtegra_h264.c)
  and its `nvtegra_decode.c` / `libavutil/nvtegra.c` support.
- Firmware provenance, unchanged image hash, and redistribution text are in
  `drivers/video/tegra210-nvdec/firmware/README.md` and `LICENSE.nvidia`.

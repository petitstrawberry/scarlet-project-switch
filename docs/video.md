# Tegra210 hardware H.264 decode

## Implementation

`scarlet-driver-tegra210-nvdec` registers the common Scarlet `/dev/video0`
backend. It boots NVIDIA's unmodified NVDEC2 firmware, submits stateless
H.264 picture parameters, and returns either tightly packed NV12 or an
explicitly negotiated immutable native NV12 image lease.

- One open session and one in-flight picture; distinct stream IDs on reopen.
- Progressive 8-bit 4:2:0, coded size up to 1920 × 1088, POC types 0 and 2.
- Stable picture and DPB slots, P/B references, multiple slices, SPS cropping.
- Private noncacheable DMA buffers; host1x OP_DONE syncpoint retirement before
  reusing backing. Falcon IDLESTATE stays `0x801` after successful jobs and is
  only suitable for the initial firmware boot check.
- One-second decode timeout; reset and MC drain affect NVDEC alone. Backing
  is retained if isolation cannot be proven. A failed session must be reopened.
- Completion is polled by the existing video client. No decode test runs at boot.

Mapped-output clients convert NVDEC block-linear surfaces to linear NV12 on
the CPU. The player now negotiates native images and samples both planes in
SGFX without CPU detiling, RGB conversion or full-frame canvas upload. Other codecs,
interlacing, POC type 1, slice groups, and custom SPS/PPS scaling matrices are
not supported by this path. Speaker playback is covered separately in
[speaker audio](audio.md).

## Build and integration

The console project enables the driver. `bundles/nvdec-player.toml` selects
`h264-stateless-hw` and `mp4-aac` for `/bin/video-player` and restores its
application catalog entry in the filtered console image. The disk root image
includes the same bundle after the full distribution bundle. `cargo scarlet
update` resolved both player layers with exactly those two features.

```sh
nix develop --command cargo scarlet build \
  --project projects/aarch64-switch-l4t-console --release
nix develop --command cargo test --manifest-path tests/nvdec-qa/Cargo.toml
nix develop --command cargo test --manifest-path drivers/video/tegra210-nvdec/Cargo.toml
```

See [NVDEC QA](../tests/nvdec-qa/README.md) for manually launched device tests,
encoded fixtures and independent software decode references.

## Surface ownership and presentation

Retired decode surfaces return to a session-local pool. DPB references remain
owned until decode completes and the next picture no longer names them.
Mapped output uses an aligned sector-read conversion path where possible,
with a generic fallback for cropped or unaligned cases.

The producer-independent contract is in `scarlet-abi::shared_image`: FourCC,
modifier, coded extent, visible crop, per-plane pitches/offsets/buffer indices,
color metadata and an immutable backing lease. `VIDEO_SET_OUTPUT_MODE` selects
native output before submission, and `VIDEO_DEQUEUE_IMAGE` returns an owning
capability only after the NVDEC completion fence. Mapped clients keep the old
ABI. Session teardown cannot invalidate retained frames. The pool reuses a
surface only when its Arc lease is unique and caps the total at 40 surfaces.

`GPU_IMPORT_SHARED_IMAGE` imports the lease into GM20B without a pixel copy.
It supports linear NV12 and NVIDIA uncompressed kind `0xfe`, two-GOB NV12
(modifier `0x03000000000fe011`), including separate plane buffers. SGFX's
sampled-only NV12 texture applies explicit BT.601/BT.709, full/limited range,
chroma siting, crop and scaling during the ordinary window composition pass.
The video client reads H.264 VUI color metadata; unsupported HDR/conversions
are rejected rather than silently treated as BT.601.

ScarletUI reuses its external image paint path and recycles retired imported
texture slots after GPU completion. Video and small control/debug overlays
are painted in order into the existing BGRA window target. The compositor and
Tegra DC continue normal BGRA presentation; no direct YUV scanout was added.
The same image contract can support such a display consumer later.

The Switch project's userspace Cargo configuration patches the coordinated
Scarlet, SGFX, ScarletUI and Chromebook libraries to local checkouts.
Keep their shared-image interfaces compatible when updating dependencies.

## Limitations

Hardware decode completion does not establish presentation frame rate.
Scheduling, composition and audio servicing can still limit playback under
load. Check colors, cropping, seeking, overlays and audio separately from
decoder frame hashes.

## References

- Linux v6.12, `adc218676eef25575469234709c2d87185ca223a`: Tegra Falcon/NVDEC
  boot, host1x v5 syncpoints, CAR MBIST sequence, and MC client reset.
- [NVIDIA picture layout](https://github.com/NVIDIA/open-gpu-doc/blob/9fdf5c4062007929d9f4e6cbad9c9771fe61b880/classes/video/nvdec_drv.h)
  and the adjacent `clc5b0.h` method definitions.
- [FFmpeg nvtegra H.264 implementation](https://github.com/averne/FFmpeg/blob/caeec83b791be08ed43468a2ef426d6901d51c78/libavcodec/nvtegra_h264.c)
  and its `nvtegra_decode.c` / `libavutil/nvtegra.c` support.
- Firmware provenance, unchanged image hash, and redistribution text are in
  [the firmware README](../drivers/video/tegra210-nvdec/firmware/README.md)
  and its adjacent `LICENSE.nvidia`.

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
The optimized driver and speaker audio build were subsequently installed on
SD; the [boot menu record](boot-menu.md) identifies the exact deployed images.

## Native NV12 presentation (2026-09-20)

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

### Validation

- GPU initialization passed **12 real color/crop readbacks** using CPU-produced
  linear and block-linear NV12, both native shader variants, BT.601/BT.709 and
  full/limited range. Padding is poisoned, and colors have non-neutral chroma.
- The canonical compiler tests native sampling followed by an RGB overlay,
  retention of both plane ranges, and rejection of a truncated UV plane.
- ScarletUI reused one logical texture slot for 2,048 distinct frames and
  released every prior source. All 45 renderer tests passed.
- The actual 1,920 × 1,080 MP4 completed 2,488 access units with
  `output=shared-image`. The user confirmed normal colors, controls and debug
  overlay, and reported visually about 24 fps. This is not an independently
  measured compositor presentation rate. Audio volume remained 0.
- Bring-up fixed a stale codegen tile-mode whitelist, a one-entry TIC limit
  that prevented UV sampling (green output), and the overlay buffer height
  conflicting with the existing minimum-size check.

Final player: 2,525,952 bytes, SHA-256
`47fb57e18bcb700dd9370953c15320e0804b9bb13917a83de2df4aeadc6d85e1`.
Copied directly from `/old_root/bin/video-player-nvdec` to `/bin/video-player`
and verified the matching SHA-256 with `storage-check hash` on the guest.
Evidence and builds are under `.cache/nv12/`: `uart-3.log`,
`uart-final.log`, `kernel-build-final.log`, `player-build-final.log`,
`codegen-tests-final.log`, `codec-tests-final.log`, `ui-tests-clean.log`,
`host-backends-check-clean.log`. The USB kernel/bundle and SD Hekate boot
images are refreshed. The SD installation passed all 14 file readbacks and
29 protected-file checks, then `disk12` was safely ejected. See the
[NV12 SD deployment record](boot-menu.md#native-nv12-refresh).

The Switch project's userspace Cargo configuration contains local source
patches for the coordinated Scarlet/SGFX/ScarletUI/Chromebook changes.
Git dependency pins must be advanced together when publishing these changes.

## References

- Linux v6.12, `adc218676eef25575469234709c2d87185ca223a`: Tegra Falcon/NVDEC
  boot, host1x v5 syncpoints, CAR MBIST sequence, and MC client reset.
- [NVIDIA picture layout](https://github.com/NVIDIA/open-gpu-doc/blob/9fdf5c4062007929d9f4e6cbad9c9771fe61b880/classes/video/nvdec_drv.h)
  and the adjacent `clc5b0.h` method definitions.
- [FFmpeg nvtegra H.264 implementation](https://github.com/averne/FFmpeg/blob/caeec83b791be08ed43468a2ef426d6901d51c78/libavcodec/nvtegra_h264.c)
  and its `nvtegra_decode.c` / `libavutil/nvtegra.c` support.
- Firmware provenance, unchanged image hash, and redistribution text are in
  `drivers/video/tegra210-nvdec/firmware/README.md` and `LICENSE.nvidia`.

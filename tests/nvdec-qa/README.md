# NVDEC correctness test

Run `nvdec-qa` manually on Scarlet with the Tegra210 NVDEC module enabled.
This test is never launched during normal boot. It compares all NV12 bytes
through a 64-bit FNV-1a hash against FFmpeg software decode, then closes and
reopens the decoder and repeats the test.

The procedural test clip contains 12 progressive H.264 Baseline frames at
160 × 90 (coded height 96), POC type 2, and two reference frames. A second
clip contains 24 High Profile frames at 320 × 180, POC type 0, CABAC,
B pictures and three slices per picture. Together they exercise cropping,
P/B references, timestamps, resolution changes between sessions, and reopen.
The hardware run checks 72 frames across four sessions.

Regenerate both fixtures and hashes with `python3 generate.py` (FFmpeg with
libx264 is required). For example, the Baseline fixture is generated with:

```sh
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=160x90:rate=12 \
  -frames:v 12 -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
  -x264-params 'keyint=12:min-keyint=12:scenecut=0:bframes=0:ref=2:aud=1:slices=1' \
  -f h264 baseline.h264
ffmpeg -hide_banner -loglevel error -i baseline.h264 \
  -fps_mode passthrough -pix_fmt nv12 -f rawvideo baseline.nv12
```

The reference output contains 12 frames of 21,600 bytes. Hash each frame
with FNV-1a (offset basis `0xcbf29ce484222325`, multiplier `0x100000001b3`)
and update `src/fixtures.rs` together with the encoded fixture. The generator
uses FFmpeg's frame packet positions to convert display-order reference
hashes into decode order for B pictures. Encoder
versions can produce different encoded clips, so the committed clip and
its committed hashes must always be used together. The fixture contains
only generated test patterns, with no third-party footage.

Host `cargo test` checks access-unit splitting and the stateless H.264
parser's dimensions, POC progression, and reference timestamps. The device
run checks the actual hardware output; host tests cannot substitute for it.

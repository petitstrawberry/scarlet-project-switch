# IMG_9105: ELPG and BAR1 pass; the first PFIFO completion still stalls

The installed ELPG follow-up clears the admission failure seen in IMG_9095.
GPU initialization now reaches the first private host-method submission after
rootfs setup. BAR1 backing, writes, remapping and all private input checks pass.
GET, reference and fence do not advance before timeout. GR and SGFX Ready
are not reached.

## Installed identity

- Video: `/Users/petitstrawberry/Downloads/IMG_9105.mov`.
- Size: 86,244,189 bytes; duration: 26.043333 seconds.
- HEVC 1920x1080; nominal 60000/1001, actual 49800/841 frames per second.
- SHA-256: `1ac6ac90987a30fdbcef7f2ad0d8c75e0eb69884216638fd4234a0d4645105c0`.
- Board source: `01b9112b5df49382870d026de40c4571e92d6eec`.
- Scarlet: `faac004cc3614ea8b192dbd7f8303cafe33a6a14`.
- ELF SHA-256: `56877e9167141f33d5d41c0831ecc54b4c904c05202d41c3e343c8b730b58b3b`.
- Image SHA-256: `b41e330eb6eb71235cbd3c447ccf079624436df403ba452e19e7958949b85c6b`.
- Package and verified SD installation: [receipt](gpu-elpg-9095-verification.json).

## Video evidence

Full frames are in `.cache/video-9105/`, sampled at four frames per second.
The fast GPU transition is also decoded without input seeking into
`gpu-*.jpg`, using `trim=start=11.85:end=12.4`. Frame names locate evidence;
the camera clip is not a frame-rate or latency measurement.

- `frame-048.jpg`: native DC adoption fetch A is 3 to 3, B is 0 to 0,
  delta 0/0. DC90 block-linear kind 0x42 publishes graphics device 9 and
  ordinary linear framebuffer `0x17eca7000`, with cached render aliases and
  Normal-NC private scanout. Opaque boot-console B remains above the GUI.
- `gpu-012.jpg` through `gpu-015.jpg`: root initramfs is mounted, the deferred
  GPU probe retries, firmware decoding/loading completes and GPU power is
  enabled. MC_BOOT_0 is `0x12b000a1`, with a 15-microsecond read.
- The legible memory/BAR1/initial submission transcript is:

  ```text
  gm20b: memory elpg=0x20301004->0x20301004 missing=0x00000000
  gm20b: GMMU BAR1 read A=0x53474131 B=0x53474232
  gm20b: GMMU BAR1 write=0x53475733 remap=0x53474232
  gm20b: GMMU BAR1 read/write/remap passed; channels pending
  gm20b: FIFO private USERD/ring/push inputs visible through BAR1
  gm20b: FIFO runlist ready; scheduler=0x00000000 pbdma-context=0x108e0130
  gm20b: FIFO submitting put=1 sequence=0x53474631
  ```

  The source checks 22 private-input words before channel binding. This
  proves their visibility through BAR1, not PBDMA execution. PBDMA context
  bits 13:15 are zero at the runlist-ready snapshot: no loaded context.
- `frame-050.jpg`, `frame-052.jpg` and `frame-064.jpg` retain the failure:

  ```text
  gm20b: FIFO USERD get=0 put=1 ref=0xffffffff fence=0x00000000
  gm20b: MC flush complete ctrl=0x00000000 status=0x00000004
  Failed to probe Standard Devices device gpu: FIFO host-method completion timeout
  ```

  Raw PBDMA pointers are not meaningful execution evidence while its context
  is unloaded. Several long diagnostic lines straddle a console clear in
  the fast transition; their obscured fields are not transcribed here.
  There is no observed first completion, second submission or authenticated
  GR initialization.
- `frame-067.jpg` shows AP3 scheduler online/local timer ready after earlier
  AP1/AP2 scheduler-online timeouts. The clip does not establish sustained
  four-core readiness.
- `frame-072.jpg`: ordinary SWS window creation and a sampled DC storage
  upload appear. Upload 5 reports 9,841 microseconds and 576/576 matches;
  this is one upload measurement, not FPS or end-to-end frame latency.
  Diagnostic B covers the GUI, so these logs do not prove its underlying
  pixels or sustained private-buffer alternation.

## Next GPU correction

Linux initializes GPU-wide clock/PRIV ring state before memory and channels.
The installed driver configured bypass clocks only inside FIFO initialization
and omitted PRIV ring reset/start entirely. NVIDIA's FIFO reset path also loads
its SLCG/BLCG settings before channel use; those settings were absent.

The follow-up adds these prerequisites and measures GPCCLK with NVIDIA's real
hardware counter. It shortens FIFO failure lines and reports unloaded context
explicitly, retaining engine status and every real completion requirement.
These source omissions are established; this video does not establish that
they cause the stall or that the follow-up resolves it. See
[follow-up implementation](gpu-prerequisites-9105.md).

# IMG_9106: PRIV ring starts, but the PFIFO context remains unloaded

The installed clock/PRIV ring correction starts the ring, reads the vendor FIFO
gating settings and measures GPCCLK. ELPG, BAR1 backing/write/remap and private
input visibility continue to pass. The first host push still times out, with
PBDMA context unloaded and GET/reference/fence unchanged. GR and SGFX Ready
are not reached.

## Installed identity

- Video: `/Users/petitstrawberry/Downloads/IMG_9106.mov`.
- Size: 100,556,856 bytes; duration: 31.22 seconds.
- HEVC 1920x1080; nominal 60000/1001, actual 93550/1561 frames per second.
- SHA-256: `36c245597b6f6c9c64edc3c497d53ad897f2eb46ffce22c51256caa0e5756c5c`.
- Board source: `8a2dcdc7c26470e113b5413598ef6632beddcb23`.
- Scarlet: `faac004cc3614ea8b192dbd7f8303cafe33a6a14`.
- ELF SHA-256: `510a1d5a3679c7ae9f26ad2ce2aaceb2c5f086b93df5356630e6e6272567f69d`.
- Image SHA-256: `a3fd2db68aa02a6c911c94c659c8656f5120cf50561e3437f6dcf9d525d41f53`.
- Build and verified SD installation: [receipt](gpu-prerequisites-9105-verification.json).

## Video evidence

`.cache/video-9106/frame-*.jpg` decodes the complete clip at four frames per
second. `gpu-*.jpg` additionally decodes every source frame with
`trim=start=16.1:end=16.8`, without input seeking. Metadata is restricted to
stream dimensions/rates, duration, size and file identity.

- `frame-052.jpg`: native DC adoption reports A 1 to 1, B 0 to 0, delta 0/0.
  DC90 block-linear kind 0x42 publishes device 9 and the ordinary linear
  framebuffer at `0x17eca7000`. Render aliases are Normal cached, scanout
  Normal-NC. Diagnostic console B remains above the GUI.
- `frame-066.jpg` and `gpu-015.jpg` through `gpu-017.jpg`: initramfs is mounted
  before the deferred GPU probe retries. Firmware decoding/loading, power and
  MC_BOOT_0 `0x12b000a1` complete. The legible GPU transcript includes:

  ```text
  gm20b: PRIV ring cmd=0x00000000 decode=0x00000002 intr=0x00000000/0x00000000
  gm20b: memory elpg=0x20301004->0x20301004 missing=0x00000000
  gm20b: GMMU BAR1 read A=0x53474131 B=0x53474232
  gm20b: GMMU BAR1 write=0x53475733 remap=0x53474232
  gm20b: GMMU BAR1 read/write/remap passed; channels pending
  gm20b: FIFO private USERD/ring/push inputs visible through BAR1
  gm20b: FIFO gating slcg=0x0001fffe blcg=0x00000000
  gm20b: GPCCLK measured=19200000Hz count=0x00000190
  gm20b: FIFO runlist ready; scheduler=0x00000000 pbdma-context=0x108e0130
  gm20b: FIFO submitting put=1 sequence=0x53474631
  ```

  The clock counter establishes the measured clock at this snapshot. It does
  not establish PBDMA execution or graphics performance. The source validates
  22 private input words through BAR1; RAMFC/PDB and runlist pages are not part
  of that installed visibility check.
- `gpu-018.jpg` and `gpu-019.jpg`: the leading failure diagnostics overlap a
  fast console clear and the init task's loader output. Partially obscured
  raw channel, bind and engine fields are not reconstructed from suffixes.
- `frame-067.jpg`, `frame-068.jpg` and `frame-072.jpg` retain the end of failure:

  ```text
  gm20b: FIFO PBDMA0 context=0x108e0130 state=0
  gm20b: FIFO PBDMA0 has no loaded context; pointers are not execution
  gm20b: FIFO USERD get=0 put=1 ref=0xffffffff fence=0x00000000
  gm20b: MC flush complete ctrl=0x00000000 status=0x00000004
  Failed to probe Standard Devices device gpu: FIFO host-method completion timeout
  ```

  There is no observed first host completion, second push, signed GR boot or
  SGFX Ready. This remains a host-channel admission failure; the clip does
  not determine whether its cause is snooping, memory initialization, channel
  configuration or another prerequisite.
- `frame-093.jpg`: SWS window activity and DC upload 8 appear; that upload
  reports 9,838 microseconds, 576/576 matches and underflow delta 0/0. This is
  one storage-upload measurement, not FPS, input latency or GPU rendering.
  Opaque diagnostic B prevents judging the underlying GUI pixels.
- Later AP/local-timer readiness messages appear, but the clip does not
  establish sustained four-core scheduling. No SMP change is part of this
  GPU follow-up.

## Follow-up

NVIDIA's MM reset path applies FB/LTC gating and FS state before BAR1. Its
GM20B LTC/FB setup distributes the PRIV ring's actual active-LTC count and
configures the physical MMU policy only on non-priv-secure hardware. Those
steps are absent from the installed correction. Nouveau also enables local
PFIFO/PBDMA error routing; the installed driver masks every child source as
well as MC's CPU interrupt outputs. These are source-backed differences;
their causal role in this physical stall is not established.

The follow-up fills those settings, checks RAMFC/PDB/runlist visibility through
BAR1 and saves live failure registers. After GPU isolation and MC drain it
reports that saved state together with actual CPU backing, so reset state is
not mistaken for the failure. The diagnostic boot holds two compact copies
for 500 milliseconds each; ordinary boot adds no hold. Real fence, retirement,
signed firmware and all graphics admission requirements remain mandatory.
See [implementation](gpu-memory-host-9106.md).

# Tegra210 / RT5639 speaker playback

`scarlet-driver-tegra210-audio` implements the common Scarlet playback device
at `/dev/audio0`. The Switch board driver matches the Icosa RT5639 wiring:
I2C1 address 0x1c, LDO enable PZ4, and speaker EQ profile 0. It owns the fixed
ADMA0 → ADMAIF1 → AHUB I2S1 → RT5639 AIF1 → stereo speaker path.

## Hardware setup

- APE partition 27, APB2APE, AHUB, PLLA and EXTERN1/MCLK sequencing live in the
  shared Tegra210 SoC module under the existing CAR lock.
- PLLA is 368.64 MHz, PLLA_OUT0 36.864 MHz, MCLK 12.288 MHz, and I2S BCLK
  1.536 MHz. A 32-bit stereo frame therefore runs at 48 kHz.
- AHUB's I2S1 at 0x702d1000 is CAR's **I2S0 / clock ID 30**, with clock source
  offset 0x1d8. CAR I2S1 / ID 11 belongs to the next AHUB controller.
- Configure the master clock before the playback RX software reset. Reset and
  PLL waits are bounded and report the failing register on timeout.
- RT5639 ID 0x6231 is checked. Speaker power sequencing keeps the output muted
  during clock and path changes. The Icosa speaker EQ/limiter and over-current
  shutdown follow Switchroot; the initial DAC level is -15 dB.
- Apply standby bias control, start I2S/DMA with the amplifier muted, then write
  and read back the EQ coefficients before enabling speaker output. Programming
  EQ before its DAC clock is running is not sufficient. Repeat this sequence
  after every stop/start.

The backend supports 48 kHz S16LE stereo, 240–2,048 frames per period. SAS
provides software mixing, master volume/mute and resampling, including 44.1 kHz
AAC playback. Headphone routing, jack detection, microphone capture, HDMI
audio and suspend/resume are not implemented by this driver.

## DMA ownership and completion

An early allocation reserves 64 KiB of private, noncacheable, below-4-GiB DMA
memory. Submitted userspace periods are copied into eight hardware slots;
hardware never retains a userspace mapping. Four periods may be in flight.
Consumed slots are cleared so an underrun produces silence.
Successful stop clears all private slots, and each restart resets/reconfigures
the ADMAIF and I2S playback FIFOs so old audio cannot leak into a short new stream.

A sleeping worker samples the 16-bit hardware period count every 2 ms while
running and every 20 ms when idle. Completion retires only submitted periods
through Scarlet's deferred callback. Counter wrapping is handled modulo 2^16.
A missed whole ring or 200 ms without progress stops the stream and rejects
new submissions. Close retains the private DMA allocation even after a stop
timeout; a successful halt is required before reconfiguration can reuse it.

## Build and device checks

The console project enables the audio module. Build normally:

```sh
nix develop --command cargo scarlet build \
  --project projects/aarch64-switch-console --release
```

Use [audio-qa](../tests/audio-qa/README.md) for the manual stereo, resampling
and reopen test. No diagnostic tone starts automatically.

Device evidence on 2026-09-20, Switchvisor without GDB, four CPUs, SD ext2 root:

- RT5639 ID and PLL lock verified; SAS configured and mapped `/dev/audio0`.
- Final `audio-qa`: 192,000 frames at 48 kHz in 3.838864 s, followed by 176,400
  frames at 44.1 kHz in 3.833767 s. No SAS XRUN or ADMA error was logged.
- A second SAS process could not acquire the device while it was open. After
  terminating the owner, a new SAS instance configured and mapped it again.
- An MP4 with 44.1 kHz stereo AAC ran for approximately 100 seconds alongside
  NVDEC; 2,358 video frames completed without a decoder error before the test
  was manually stopped. This was not a complete-file playback test.
- The user reported loud high-frequency noise over otherwise recognizable
  audio, reproduced with the test tones at SAS volume 5%. Playback was stopped.
  The subsequent bias/EQ sequencing fix passed a two-second digital-silence
  check and both rate/channel tests: the user confirmed normal left/right
  sound without the noise. This first audible confirmation used -30 dB analog
  attenuation. The final build restored 0 dB analog gain, retained the -15 dB
  DAC level, and passed both rate/channel tests at SAS volume 25%; the user
  again confirmed normal sound without the noise.
- The user also confirmed normal AAC sound without the noise in the MP4.
  The video decoder retired 4,178 pictures across playback and seeking in that
  session with `last_error=none`. The user reported very slow video presentation;
  this is a remaining video performance issue, not a claim of smooth playback.
  The final audio build and subsequent NVDEC performance changes were installed
  on SD with verified readback; see the [deployment hashes](boot-menu.md).
- After reducing NVDEC's CPU overhead, the 1080p video/UI run logged one
  full-ring overrun (`count=1508 delta=9`) and the common PCM frontend restarted
  output automatically. This represents at least 180 ms between observations
  with 20 ms periods. Load-related service latency is still unresolved; normal
  tone/AAC confirmation does not establish uninterrupted playback under all
  graphics loads. See the [video follow-up](nvdec-bringup.md).

Local logs are in `.cache/audio-bringup-20260920/`, particularly
`uart-audio-4.log` (initial noisy output), `uart-audio-5.log` (attenuated test),
`uart-audio-6.log` (final gain), `build.log`, `package.log` and `deploy-6.log`.

## Primary source references

- [Switchroot RT5640/RT5639 codec and Icosa EQ](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/sound/soc/codecs/rt5640.c)
  and adjacent `rt5640.h`, GPL-2.0.
- [Tegra210 ADMA](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/dma/tegra210-adma.c),
  channel layout, cyclic period count and clock-gating workaround.
- [Tegra clock sources](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/clk/tegra/clk-tegra-periph.c)
  and adjacent `clk-tegra210.c`, `clk-tegra-audio.c`, `clk-tegra-pmc.c`.
- [NVIDIA I2S playback sequencing](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/sound/soc/tegra-alt/tegra210_i2s_alt.c)
  and adjacent ADMAIF/XBAR drivers, GPL-2.0.
- [Linux APE MBIST workaround](https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/drivers/clk/tegra/clk-tegra210.c).

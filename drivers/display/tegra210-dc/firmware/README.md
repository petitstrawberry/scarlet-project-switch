# VIC4 Fetch Control Engine microcode

`vic-fce.bin` is the exact 964-byte `vic_fce_ucode` array from
[Hekate vic.c](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/display/vic.c#L278),
revision `e487de8fdd6ca9c3f608d1d18c097a86355912b9` (v6.5.3).
The source identifies it as dumped from L4T r33.
Copyright (c) 2018–2024 CTCaer; the containing source is GPL-2.0-only.
The repository's `LICENSE` supplies that license text.

SHA-256: `ac41b5ea512918e2f1b1ce3478f9a373cb5660fa1ff8e5a8fba96bb8546b6f3c`.

It is embedded in the DC driver and loaded through VIC's private register
aperture. It is separate from GM20B signed graphics firmware and does not
claim SGFX shader execution. No full VIC Falcon firmware is booted by this
Hekate-compatible direct FCE path.

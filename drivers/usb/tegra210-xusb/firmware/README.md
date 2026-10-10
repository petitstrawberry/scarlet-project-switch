# Tegra210 XUSB firmware

`xusb.bin` is the unmodified `nvidia/tegra210/xusb.bin` from
[linux-firmware revision afabaf773c4c2e2c841429933a6a084a5af4d14d](https://gitlab.com/kernel-firmware/linux-firmware/-/blob/afabaf773c4c2e2c841429933a6a084a5af4d14d/nvidia/tegra210/xusb.bin).
The accompanying `LICENCE.nvidia` is the unmodified
[LICENSES/LICENCE.nvidia](https://gitlab.com/kernel-firmware/linux-firmware/-/blob/afabaf773c4c2e2c841429933a6a084a5af4d14d/LICENSES/LICENCE.nvidia)
from the same revision. NVIDIA retains its firmware copyright and license.

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| `xusb.bin` | 126,464 | `941873a6a70993b5c40a608cedc4608c281458c11949092d4bce125b96a92025` |
| `LICENCE.nvidia` | 6,212 | `bc5225a57f49c5249dcf238e4ae6437811677a2a8a7f579c3d839e058653ee44` |

The driver embeds the exact binary and copies it into private, noncacheable
DMA memory for the Tegra210 Falcon boot ROM. It validates the config table
and fetch bounds without modifying the firmware. It does not execute this
firmware on the development host.

The license's Open Source Exception permits redistribution for operating
systems under an OSI-approved open-source license with the binary unchanged
and a copy of the license supplied. The console's
`bundles/usb-firmware-license.toml` installs that complete license into both
initramfs and rootfs at `/usr/share/licenses/tegra210-xusb/LICENCE.nvidia`.
Keep the license with any distribution containing the embedded firmware.

Verify the checked-in files from the repository root:

```sh
shasum -a 256 drivers/usb/tegra210-xusb/firmware/xusb.bin \
  drivers/usb/tegra210-xusb/firmware/LICENCE.nvidia
```

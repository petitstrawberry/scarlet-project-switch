# Tegra210 NVDEC firmware

`nvdec.bin` is the unmodified `lib/firmware/tegra21x/nvhost_nvdec020_ns.fw`
from NVIDIA's `nvidia-l4t-firmware` 32.7.6-20241104234540 package.

- [Original package](https://repo.download.nvidia.com/jetson/t210/pool/main/n/nvidia-l4t-firmware/nvidia-l4t-firmware_32.7.6-20241104234540_arm64.deb)
- Package SHA-256: `0404d7ddea8eda64c492f2c4e329c1f92a346d927eaa6926ec47233e70069eff`
- Firmware size: 128,000 bytes
- Firmware SHA-256: `7b5ac5ad66dad47e77f2a056991e422dd2630fa17e0a57a5874af492efa0f909`

The accompanying `LICENSE.nvidia` is the package's complete copyright/license
file. The firmware is embedded unchanged and used only by the Tegra210 driver.
This is NVIDIA's firmware variant that boots without a separate bootloader;
the driver does not modify signed firmware or security carveouts.

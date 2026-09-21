# Switch drivers

The console manifest links these external driver modules into Scarlet:

| Directory | Responsibility |
| --- | --- |
| `soc/tegra210` | CAR, pinmux, GPIO, PMC, I2C/UART, interrupt routing and thermal resources |
| `rtc/max77620` | PMIC supplies and RTC |
| `input/` | Attached Joy-Con and FTM4 touchscreen |
| `cpufreq/tegra210` | Shared Cortex-A57 PLLX/MAX77621 frequency policy |
| `gpu/nvidia-gm20b` | GPU power, firmware, memory, validated execution and SGFX resources |
| `display/tegra210-dc` | Inherited panel-mode adoption and scanout |
| `mmc/tegra210-sdhci` | Removable SD card host |
| `video/tegra210-nvdec` | Hardware H.264 decoding |
| `audio/tegra210` | RT5639 speaker playback |
| `power/switch-power` | Battery and input-power telemetry |

Board drivers provide hardware mechanisms through Scarlet's common device
interfaces. The kernel owns scheduling and policy; SWS and ScarletUI use
their normal display, image and input APIs.

See the [driver documentation](../docs/README.md#architecture-and-drivers)
for interfaces, limits and source references, and
[development checks](../docs/testing.md) for validation commands.

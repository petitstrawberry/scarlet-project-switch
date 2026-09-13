# Switch drivers

Tegra-specific driver crates belong here, following scarlet-project-chromebook.
The console manifest links Tegra210 I2C/UART/GPIO/clock/pinmux transport,
MAX77620 RTC and touch supply, STM FTM4 touchscreen, and attached Joy-Con
crates. The external Tegra210 cpufreq module adds a shared A57 PLLX/MAX77621
policy through the normal governors and `/dev/cpufreq`; see
[CPU frequency bring-up](../docs/cpufreq-bringup.md).
Transport operations are limited to the declared supported instances;
there is no complete generic Tegra clock, reset, regulator or GPIO IRQ API.
See [input bring-up](../docs/input-bringup.md) for scope, provenance and tests.
The BSP retains firmware's existing scanout surface. Tegra SDHCI, interrupt
controller routing changes and a display-controller driver remain unimplemented.

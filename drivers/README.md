# Switch drivers

Tegra-specific driver crates belong here, following scarlet-project-chromebook.
No Tegra SDHCI, clock, reset, power, GIC-routing, or display-controller driver is
claimed as implemented. The first BSP uses firmware's existing scanout surface
and, only when requested, its initialized UART for arrival diagnostics.

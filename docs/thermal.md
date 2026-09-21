# Thermal and GPU frequency policy

The Tegra210 SoC driver registers the TMP451 sensor and PWM fan through its
normal driver initcall. GM20B waits for the fan thermal zone before powering
on. The fan setup preserves the backlight's shared clock and keeps the shared
A5 5-V rail enabled.

## Sensors and fan

I2C1 runs at the device tree's 100 kHz. TMP451 setup verifies the manufacturer
ID, sets the DT's remote/local THERM limits and conversion rate, preserves the
inherited range bit and resumes conversion if necessary. Reads check the
open-diode flag.

The `switch-skin` zone uses the ODIN Console skin estimate:

```text
max((SoC_mC * 6182 + 112480000) / 10000 + 500,
    (board_mC * 6396 + 119440000) / 10000 + 500)
```

The common thermal policy smooths the estimate and interpolates these PWM
points, sampled once per second:

| Estimated skin temperature | Fan duty, out of 255 |
| --- | ---: |
| 36°C | 0 |
| 40°C | 51 |
| 43°C | 51 |
| 53°C | 153 |
| 58°C | 255 |

Zero duty uses Tegra's absolute-off encoding. Sensor failure requests full
duty. The first synchronous temperature sample selects duty before GPU power
is enabled; failed thermal registration leaves GM20B deferred.

The kernel owns policy and drivers provide sensor/cooling callbacks.
Each cooling device has one zone owner. Shared-cooler arbitration and a
complete platform thermal-shutdown implementation are not provided.
PWM changes apply directly; Switchroot's stepped PWM ramp is not implemented.

## GPU clock and thermal cap

GM20B exposes 76,800, 153,600, 230,400 and 307,200 kHz operating points through
`/dev/devfreq`. These keep a fixed 1.0-V rail and 1.8432-GHz PLL VCO and
change the post-divider under bypass. A transition serializes with GPU work,
requires idle hardware, measures the resulting GPCCLK and restores or
isolates the GPU on failure.

The driver selects `simple_ondemand` after a valid PMU busy/total sample,
with per-device thresholds 45/5. Unavailable counters leave the initial
307,200-kHz userspace setting active. Telemetry continues under manual
governors so finite-width PMU counters retain a usable baseline.

The separate `switch-gpu` zone uses SOCTHERM. It caps the GPU at 153.6 MHz
at 90.5°C and 76.8 MHz at 100°C, with 2°C release hysteresis. Sensor failure
uses the lowest operating point. These caps adapt the vendor thresholds to
Scarlet's fixed-rail operating points; they do not implement voltage scaling
or NVIDIA's full balanced cooler.

## Inspecting policy

Task Manager exposes Thermal and GPU frequency information. The same
snapshots and controls are available from the guest shell:

```sh
cat /dev/thermal
cat /dev/devfreq
echo 'device gm20b frequency 153600' > /dev/devfreq
echo 'device gm20b thresholds 45 5' > /dev/devfreq
echo 'device gm20b governor simple_ondemand' > /dev/devfreq
```

`switch-pwm-fan` reports commanded duty, not measured RPM. Record clock,
governor and workload when comparing performance. A short successful run
does not establish sustained thermal behavior. For input-power and battery
telemetry, see [power](power.md).

## Primary references

- [Switchroot 5.1.2 ODIN Console device tree](https://github.com/CTCaer/switch-l4t-platform-t210-nx/blob/cf785c4c176499b301170d79fe57b77f365b73cd/kernel-dts/nx-platforms/tegra210-odin-common.dtsi):
  skin estimator coefficients, Console fan curve, and separate GPU trips.
- [Switchroot skin estimator](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/a4cc21186653434c0362323b12354e6e713ad5af/drivers/misc/therm_tskin_fan_est.c),
  [continuous governor](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/a4cc21186653434c0362323b12354e6e713ad5af/drivers/thermal/continuous_thermal_gov.c),
  and [PWM fan driver](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/a4cc21186653434c0362323b12354e6e713ad5af/drivers/thermal/pwm_fan.c):
  estimate formula, STA/LTA smoothing, inverted zero duty and PWM ramping.
- [TI TMP451 datasheet](https://www.ti.com/lit/ds/symlink/tmp451.pdf):
  register addresses, high-byte/low-byte latch order, 0.0625°C resolution,
  RANGE and shutdown bits, manufacturer ID and conversion rates.
- [Linux TMP451 / LM90 driver](https://github.com/torvalds/linux/blob/master/drivers/hwmon/lm90.c)
  and [Switchroot NVIDIA NCT1008 driver](https://github.com/CTCaer/switch-l4t-kernel-nvidia/blob/76e6d48970b451c242c20f298b8d63027836bb0b/drivers/misc/nct1008.c):
  sensor channels, range offset, critical limits and DT binding.
- [Hekate fan driver](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/thermal/fan.c),
  [I2C clocks](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/soc/clock.c),
  and [pinmux](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/soc/pinmux.c).
- [Linux thermal framework](https://docs.kernel.org/driver-api/thermal/sysfs-api.html):
  platform-independent sensor, zone and cooling-device separation.
- [Linux Tegra peripheral clock definitions](https://github.com/torvalds/linux/blob/40288c9206c17eb66a603262e06a58d300d0f279/drivers/clk/tegra/clk-tegra-periph.c):
  TSENSOR's `MUX` is shifted by 30; SOCTHERM's `MUX8` is shifted by 29.
- [Linux Tegra210 SOCTHERM sensor groups](https://github.com/torvalds/linux/blob/40288c9206c17eb66a603262e06a58d300d0f279/drivers/thermal/tegra/tegra210-soctherm.c)
  and [common thermal-zone readback](https://github.com/torvalds/linux/blob/40288c9206c17eb66a603262e06a58d300d0f279/drivers/thermal/tegra/soctherm.c):
  GPU group mapping, `SENSOR_TEMP1` readback and fallback offset.
- [Switchroot Tegra210 GPU fixed-PLL CVB points](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/soc/tegra/tegra210-dvfs.c):
  307.2-MHz operating point and speedo-dependent voltage table.
- [Switchroot GM20B clock transitions](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gm20b/clk_gm20b.c):
  post-divider change under bypass and throttle-state restoration.
- [Switchroot nvgpu PMU counter wiring](https://github.com/CTCaer/switch-l4t-kernel-nvgpu/blob/1ae0167d360287ca78f5a2572f0de42594140312/drivers/gpu/nvgpu/gk20a/pmu_gk20a.c)
  and [Linux simple-ondemand governor](https://github.com/torvalds/linux/blob/master/drivers/devfreq/governor_simpleondemand.c):
  GR/CE2 busy and total cycle counters, utilization thresholds and OPP selection.

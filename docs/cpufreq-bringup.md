# Erista CPU frequency control

The console image links the external `scarlet-driver-tegra210-cpufreq` module
into Scarlet's existing CPU frequency backend, shared policy and governors.
The four PSCI logical CPUs receive the same `performance-domains` binding.
The standard `/dev/cpufreq` interface and `cpufreqctl` utility also work with
other registered backends; the shell and SWS need no Switch-specific controls.
`/dev/cpuinfo` already supplies the hardware frequency to Task Manager.

The initial driver owns PLLX and the CPU MAX77621 at I2C5 address `0x1b`.
It uses the enabled ODIN CPU frequency ladder up to its normal 1020 MHz entry,
the oscillator/reference divider read from CAR, NVIDIA's PLLX fixed-M and
quasi-linear post-divider rules, and the SKU `0x83` CPU speedo fuse conversion.
PLL voltage requirements use `CPU_PLL_CVB_TABLE_ODN`, its 950 mV minimum and
the Linux PLL margin of 30%. They are rounded upward to MAX77621's 6.25 mV
selector steps. The lower DFLL voltage table is not used for PLLX.

Each transition checks both enabled VOUT/DVS banks, raises them before raising
the CPU clock, and waits for ramp completion. All CCLKG burst-state selectors
move to the locked 408 MHz PLLP output while PLLX stops and its M/N/P changes.
After a bounded 300 us lock wait, the mux returns to PLLX and its rate is read
back from the registers. Only then may voltage decrease. A PLLX lock failure
leaves the cluster on PLLP with its higher voltage; a failed voltage reduction
does not undo a successfully applied frequency. PLLP's rate, peripheral clocks,
EMC, architected counter frequency and per-core timer configuration stay intact.
The common governor's worker performs transitions outside interrupt context.

With the inspected 38.4 MHz PLL reference the registered hardware rates are:

```text
101120 204000 303360 408000 505600 608000 710400 816000 912000 1017600 kHz
```

These are PLLX's rounded rates, rather than the nominal 102 MHz ladder labels.
`current_khz` is calculated from the active hardware parent/M/N/P. A manual
request selects the first available rate at or above it, within policy limits.

```sh
cpufreqctl get
cpufreqctl set 408000
cpufreqctl set 816000
cpufreqctl set 1017600
cpufreqctl governor schedutil
cpufreqctl governor performance
cpufreqctl governor powersave
```

`set` applies a fixed frequency and attaches `userspace` atomically after the
hardware transition succeeds. An optional final CPU ID chooses a policy;
on Switch every CPU selects the same cluster. The image starts with `schedutil`.
`get` lists the CPU mask, governor, target, actual frequency and available rates.
SWS output scale is 1.0; the framebuffer remains the ordinary graphics surface.

Physical CPU frequency switching is pending. The previous SMP/Joy-Con image
received the user's `動いた` report; that does not validate this new clock driver.
For this candidate observe `tegra210-cpufreq: PLLX ready; CPUs=0xf`, run the
commands above, and check actual frequency, normal shell/input operation and
continued clock/sleep wakeups under both low and high CPU load. Repeated boot,
per-core timer delivery and sustained frequency stability require the Switch.
The initial scale 1.5 package and SD readback are recorded in
`cpufreq-verification.json`. The standalone scale 1.0 rebuild, which was not
installed, is recorded in `scale1-verification.json`. The combined GM20B
power/identity candidate and its SD readback are in `gpu-verification.json`.
No additional host or QEMU tests are used as hardware evidence.

DFLL initialization, higher frequencies, thermal policy and EMC scaling are
outside this first driver. Unsupported inherited DFLL/divider states, fuses or
regulator wiring fail probe with a diagnostic instead of claiming a frequency.

## Primary source provenance

Fetched with `gh`, from Switchroot Linux 5.1.2 commit
`2d0059fd3167a8df756de2aa0489d4aa70a9fc15`:

- [PLLX/PLLP limits, defaults and divider map](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/clk/tegra/clk-tegra210.c)
- [CPU clock mux](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/clk/tegra/clk-super.c)
- [Oscillator and PLL reference divider](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/clk/tegra/clk-tegra-fixed.c)
- [CPU PLL CVB table and margin](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/soc/tegra/tegra210-dvfs.c)
- [CVB rounding](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/soc/tegra/cvb.c)
- [Speedo fuse conversion](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/soc/tegra/fuse/speedo-tegra210.c)
- [Read-only fuse shadow layout](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/soc/tegra/fuse/fuse-tegra30.c)

Secondary MAX77621 register/selector and ramp reference:
[Hekate max7762x.c](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/power/max7762x.c).

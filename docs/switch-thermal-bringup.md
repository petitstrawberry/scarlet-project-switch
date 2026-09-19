# Switch thermal bring-up — 2026-09-19

## Current candidate: Switchroot Console fan, GPU cap and automatic devfreq

The current source uses Switchroot 5.1.2's ODIN Console skin estimate:
`max((SoC_mC * 6182 + 112480000) / 10000 + 500,
(board_mC * 6396 + 119440000) / 10000 + 500)`. The kernel smooths this
estimate with the vendor continuous governor's STA/LTA parameters, then
interpolates the Console PWM points 36°C/0, 40°C/51, 43°C/51, 53°C/153 and
58°C/255. The zone is sampled every second, versus Switchroot's 1100 ms.
The fan remains off below the estimated 36°C turn-on threshold and goes to
255/255 if the TMP451 sample fails. The fan driver now writes Tegra's
`0x100 << 16` absolute-off encoding at zero duty; the previous `236 << 16`
value was not off. The common A5 5-V rail remains enabled because it is
shared with other Switch hardware. Switchroot also ramps PWM changes in
100-ms steps and enables/disables the fan regulator; Scarlet currently applies
the requested PWM directly.

PWM initializes off while GM20B remains deferred. TMP451 configuration and
the first synchronous skin-temperature sample select the actual duty before
the GPU is allowed to power on. A registration failure commands full duty and
leaves GM20B deferred. The kernel's separate `switch-gpu` zone uses the
SOCTHERM GPU sensor. Switchroot starts passive GPU balancing at 90.5°C and
heavy throttling at 100°C; Scarlet translates these points to its
fixed-rail OPP caps of 153.6 and 76.8 MHz, with 2°C release hysteresis and
76.8 MHz on sensor failure. The cap starts at the existing 307.2 MHz, so
zone registration does not force an untested down/up transition during boot.
This is an adaptation of Switchroot's temperature thresholds, not a port of
its `gpu-balanced` cooler or its hardware thermal shutdown. GPU voltage
remains fixed at 1.0 V. GM20B selects the kernel's `simple_ondemand` frequency
governor after a valid PMU counter sample; unavailable counters leave the
validated 307.2-MHz userspace setting active.

The earlier fan/GPU-cap candidate built, packaged and booted on hardware. The previous hardware
power-offs happened with a low battery and without a charge-current or
reset-reason capture. The incident notes and hashes below describe **earlier
images**, not the current automatic-devfreq candidate. The earlier fan/GPU-cap image is preserved
at `.cache/gm20b-linux-audit/switchroot-console-fan-gpu-cap-candidate/bundle.json`;
its uImage SHA-256 is
`80fa4389d043cfb4d01fc20efbb75ef1a1da337291c358585b59628989838eb6`.

### Hardware result

On 2026-09-19 the candidate was uploaded from RCM through Hekate and
Switchvisor. It reached the guest shell with four CPUs online and both CDC
ports present. PWM1 initialized at the absolute-off register value
`0x81000000`. The first TMP451 reading was 44.187°C SoC and 41.5°C board;
the estimated skin temperature was 39.064°C, so the kernel requested 39/255
fan duty before powering GM20B. Both `switch-skin` and `switch-gpu` zones
registered, and GM20B's shader/readback bring-up passed. Later the filtered
skin temperature reached 40.006°C and requested 51/255, while the separate
GPU sensor reported about 42.5°C. These logs prove the PWM register command
and kernel policy transitions, not the physical fan RPM or acoustic level.

`/dev/devfreq` reported 307200 kHz at boot. From the UART shell, the device
was set to 230400, 153600 and 76800 kHz, then restored to 307200 kHz. The
driver measured GPCCLK at 230400000, 153600000, 76800000 and 307200000 Hz
respectively, and `/dev/devfreq` read each current rate back. The shell and
Switchvisor USB control remained responsive after the transitions. No
sustained SGFX load or high-temperature cap activation was tested in this
run. Evidence: `guest-uart-switchroot-console-fan-gpu-cap.log` and
`deploy-switchroot-console-fan-gpu-cap.log` under `.cache/gm20b-linux-audit/`.

### Automatic devfreq hardware result

The current bundle is
`.cache/gm20b-linux-audit/devfreq-auto-fixed-candidate/bundle.json` (uImage
SHA-256 `3187990fcfa60ce83c73a2c2c5435b52f7ae34d9d9560ff63b4336252fd32310`).
On 2026-09-19 it booted through Switchvisor with four CPUs, a responsive UART
shell and `simple_ondemand` selected. GM20B's PMU counter #1 counted GR/CE2
busy cycles and #2 counted total cycles. `/dev/devfreq` reported 76,800 kHz
at idle, 716 valid samples and zero failures. A short `ui-sgfx-showcase` run
showed measured 76,800→307,200-kHz boosts and returns to 76,800 kHz; the
shell and USB control remained responsive. After stopping the showcase,
`/dev/devfreq` reported 8,827 samples, zero failures and 76,800 kHz.
The GPU thermal sensor reported about 45–46°C during this run. These logs
verify automatic transitions and a short SGFX workload, not long-duration
thermal-cap activation or physical fan RPM. Evidence:
`guest-uart-devfreq-auto-fixed.log`, `guest-uart-devfreq-auto-fixed-stop.log`
and `deploy-devfreq-auto-fixed.log` in `.cache/gm20b-linux-audit/`.

The first auto image halted just after the shell prompt: its 25-ms worker
kept the policy-list `IrqSpinLock` while acquiring a sleepable policy mutex.
The final worker clones the policy reference and releases the spin lock
before polling. A diagnostic image with the worker disabled confirmed that
the PMU setup alone did not stop UART input.

## Earlier hardware observations

The fixed-duty fan experiment did not stop the whole-device power-off. With
the fan visibly spinning at 153/236 duty and the SGFX showcase running, the
guest shell still answered after six minutes, then both Switchvisor CDC ports
disappeared at 03:38:49 UTC. No panic preceded the loss in the UART capture.
The earlier fan-off candidate lost the same ports after a shorter run, but
the workloads and initial temperatures differ. **This does not establish a
thermal cause or prove that the fan changed survival time.** The incident
captures are `.cache/gm20b-linux-audit/guest-uart-fan-only.log` and
`usb-presence-fan-only.log`.

## Kernel ownership

`Scarlet/kernel/src/device/thermal.rs` is the common thermal policy layer.
Hardware drivers implement temperature and cooling-state callbacks; a kernel
worker per zone polls sensors, applies ordered trips or continuous interpolation,
and drives cooling to a fail-safe state when any zone sensor fails. One zone
exclusively owns each cooling device, avoiding competing state requests. This
is still less than Linux's complete thermal framework: it does not yet offer
shared-cooler arbitration or hardware thermal-shutdown programming.

The kernel now exposes read-only `/dev/thermal` snapshots for every registered
zone. Each snapshot contains the last sampled temperature, sample/failure
counts and applied cooler state. Task Manager's Thermal tab displays these
values alongside its GPU device-frequency tab. `switch-pwm-fan` is the duty
command (`0..255`), not a measured fan RPM. This Task Manager image has been
built and packaged, but its new thermal readout has not yet been checked on
hardware.

The Tegra210 driver registers the ODIN fan and TMP451 through its **existing
driver initcall**. It has no new `force_link` path. The GPU driver waits for
the completed fan thermal zone before powering GM20B and does not write the
fan itself. The PWM0 backlight's shared clock is checked and never reset by
fan setup. The shared A5 5-V rail stays enabled.

I2C1 is enabled at the DT's 100 kHz using Hekate's oscillator divider and
GEN1_I2C pins. The TMP451 driver verifies the TI manufacturer ID, programs
the DT's 96°C remote and 120°C local hardware THERM limits, selects the DT's
four-conversion-per-second rate, and resumes conversions if the bootloader
left the sensor in shutdown mode. It preserves the *actual inherited* range
bit and decodes measurements accordingly; this first hardware pass does not
change measurement range while altering critical protection. Each read checks
the open-diode flag. The kernel logs both SoC diode and board temperatures at
startup and every 30 samples, along with skin estimates, cooling-state
changes and sensor failures.

The first hardware run used a 153/236 fan floor. That was audibly excessive
at about 43°C, so a 51/236 floor was tried next; that run lost both USB
ports after roughly two minutes of SGFX load. The later provisional policy
restores 153/236, increases to 192 at 60°C and 236 at 70°C, and fails safe
at 236 if a sensor cannot be read. The policy is **not** a direct port of
Linux's Console skin-temperature estimator: that estimator mixes board and
diode measurements, so applying its skin-trip numbers to a raw sensor would
be incorrect. GPU remains at the verified 307.2-MHz fixed PLL. Dynamic GPU
clock transitions previously produced an ambiguous black-screen candidate
and are deliberately not mixed into this temperature-measurement run.

In Switchroot Linux, the generic thermal core registers zones and coolers;
the Switch-specific DT provides the Console profile and estimator coefficients,
the `therm_tskin_fan_est` driver calculates an estimated temperature, the
`continuous_therm_gov` governor smooths and interpolates a target, and the
`pwm_fan` driver applies it. The Console profile's estimated-temperature
points include 36°C/0, 40°C/51, 43°C/51, and 53°C/153 on a 0–255 PWM scale.
At the first hardware run's final raw readings (44.75°C SoC, 41.312°C board),
the DT estimator's instantaneous calculation is about 39.5°C; filtering
history means this is not necessarily the governor's current temperature.
That earlier candidate still used provisional board-specific raw-temperature
trips. The current candidate above uses the Console estimate and curve.

## Hardware check

The first thermal-manager image was deployed through Switchvisor on
2026-09-19. UART confirmed I2C1 at 100 kHz, TMP451 continuous conversion,
fan initialization at fail-safe duty, a registered thermal zone, GPU probe,
and a working shell. `ui-sgfx-showcase &` started at 04:02:11 UTC. At
04:13:17 UTC the guest was still running with both CDC ports available;
the latest sample was SoC 44.75°C and board 41.312°C, and the shell answered
`THERM_11M_ALIVE`. This passed the prior nine-minute power-off interval, but
does not establish why the earlier candidate powered off. Captures:
`.cache/gm20b-linux-audit/guest-uart-thermal-manager.log` and
`usb-presence-thermal-manager.log`.

The 51/236 floor was then deployed and `ui-sgfx-showcase &` started around
04:23:30 UTC. At the last UART temperature snapshot, TMP451 reported SoC
46.437°C and board 42.75°C. Both Switchvisor CDC ports disappeared at
04:25:39 UTC, roughly two minutes after starting the showcase; the user
confirmed the unit fell over. The UART tail has no panic or GPU fault.
These readings do not include Tegra's dedicated GPU thermal sensor, so the
cause cannot be identified from the TMP451 numbers. Captures:
`.cache/gm20b-linux-audit/guest-uart-thermal-quieter.log` and
`usb-presence-thermal-quieter.log`.

The quieter fan policy builds and packages to the Switchvisor USB bundle;
its uImage SHA-256 is
`0316fc8fdf1667713e72f026f5ce988fc20f44c6d1168987a2bf291d8a1578b8`.
After the first test, `switchvisorctl reboot-rcm` returned success, but the
subsequent nxboot run initially left the unit enumerated as APX without a
Switchvisor CDC device. Switchvisor later returned to preboot and the quieter
bundle was deployed; do not classify that planned reboot as a power-off.

Linux's ODIN DT has a separate `GPU-therm` zone fed by Tegra210 SOCTHERM,
with a passive trip at 90.5°C, hot at 100°C and critical at 103°C. The new
driver configures all eight TSENSORs using Tegra210 fuse calibration. The
first hardware run accidentally selected PLLC for TSENSOR: Linux declares
TSENSOR as a four-parent `MUX`, with source bits 31:30, while SOCTHERM is an
eight-parent `MUX8`, with source bits 31:29. The mistaken `(2 << 29)` TSENSOR
source left both the individual and grouped temperature registers at zero.
After correcting TSENSOR to `(2 << 30)`, the grouped register changed to
`0x25002580`: CPU 37°C and GPU 37.5°C. The individual GPU status remained
zero, and the first driver still rejected the probe. These runs are recorded
in `guest-uart-soctherm-eight-sensors.log` and
`guest-uart-soctherm-mux-fix.log` under `.cache/gm20b-linux-audit/`.

Linux's thermal zone reads the low GPU half of `SENSOR_TEMP1` at offset
`0x1c8`, not the individual GPU `SENSOR_STATUS1` at offset `0x190`. The next
candidate uses that grouped path, retains a validity/range check, and logs
when the individual sensor first becomes valid. While the GPU rail is gated,
the grouped path can reflect the PLLX hotspot offset. No SGFX showcase was
run on either failed SOCTHERM candidate; both were intentionally returned to
RCM to stop the 236/236 fail-safe fan. The group-read uImage SHA-256 is
`0f5edae84dc0dcf804f01f4d01acfb5020f997971c93b274b58145db0b3a95ac`.

The group-read image was deployed on hardware through Switchvisor. Probe
reported GPU 37.5°C from `SENSOR_TEMP1`, and the direct GPU sensor became
valid after GM20B powered on (`0x80002500`). GPU shader draw/readback passed,
the thermal zone started with a 153/236 fan duty, and the console shell
responded during `ui-sgfx-showcase &`. At the last captured sample, SoC was
45.625°C, board 42.187°C, and grouped GPU 40.0°C. All Switchvisor CDC ports
were present at 05:30:05 UTC and gone by 05:30:10 UTC, about four minutes
after starting the showcase. No reset command was sent and no panic or GPU
fault preceded the disconnect in the UART capture. The user confirmed the
unit powered off. Grouped GPU temperature does not alone prove that its
individual sensor stayed valid for the entire run; that validity was logged
only at its first transition. The observed temperatures are below the
SOCTHERM GPU critical trip, so do not claim a proven GPU thermal shutdown.
The user subsequently reported 9.6% battery remaining and that the only
external connection was the Mac USB cable. Battery voltage and charge current
under GPU load, PMIC status, and reset reason were not captured. Low battery
is a plausible cause of the power-off, not a confirmed one; repeat this exact
153/236 image after charging before attributing the loss to thermal or GPU
software. The image and its three payloads are preserved as
`.cache/gm20b-linux-audit/control-thermal153/bundle.json`. Evidence:
`.cache/gm20b-linux-audit/guest-uart-soctherm-group-read.log`,
`usb-presence-soctherm-group-read.log`, and `deploy-soctherm-group-read.log`.

The audible fan speed comes from the provisional 153/236 floor, irrespective
of the measured 40°C GPU temperature. GPU frequency is fixed at 307.2 MHz;
the Switchroot Tegra210 fixed-PLL CVB table includes that point as the fourth
of twelve frequencies from 76.8 to 921.6 MHz. This is not evidence that
307.2 MHz is overclocked or the cause of the power-off. The rail is held at
1.0 V, while the runtime fuse/CVB check reported 0.95 V required for this
device at 307.2 MHz. A lower validated voltage or frequency may reduce power
but needs a separate hardware A/B run.

A quieter follow-up image uses a 51/236 baseline, then 76 at the hottest raw
sensor's 47°C, 102 at 52°C, 153 at 60°C, 192 at 65°C, and 236 at 70°C.
It keeps 236/236 on any sensor failure. These thresholds are provisional raw
SoC/GPU/board-temperature policy, **not** Linux's Console skin-temperature
estimator or its continuous governor. The charged-battery control run should
use the preserved 153/236 bundle first; the quieter bundle can then be tested
without conflating the battery change with the fan change. The quiet image
builds and packages; its uImage SHA-256 is
`8117c24a437d35d376ad069c1ecba1c0277a630d9b56d65fb40ff55399d6e85a`.

Switchvisor owns the Switch USB-C port as a USB 2.0 device and exposes CDC
ports to the Mac host. It does not currently initialize the BM92T PD
controller or BQ2419x charger, and its USB configuration advertises
`self-powered` with `bMaxPower=1` (2 mA). This is not a measurement of actual
charger current, but means the Mac USB link cannot be assumed to sustain the
system under GPU load. A conventional USB-C PD pass-through hub charges its
upstream host (the Mac in this topology); only a powered downstream port
that supplies both USB data and sufficient power to the Switch might help,
and that combination has not been validated with Switchvisor. Charge the
Switch independently for the controlled repeat.

## Kernel GPU frequency policy candidate

The kernel now has a common `devfreq` policy for non-CPU devices with exact operating points,
`performance`, `powersave`, `userspace` and `simple_ondemand` governors, plus a kernel-only
thermal maximum hook. The GM20B backend registers after its graphics proof
passes. The shared `/dev/devfreq` control node names its target device in each
command; GM20B is currently its only registered policy. It reports the
configured PLL rate and accepts, for
example, `echo 'device gm20b frequency 153600' > /dev/devfreq`; the available
rates are 76800, 153600, 230400 and 307200 kHz. Registration starts at the
previously proven 307200 kHz; GM20B selects `simple_ondemand` once its PMU
counters pass validation. `/dev/devfreq` also reports the latest utilization,
sample count and failures. On a cool boot, thermal zone registration does not
force a runtime clock transition.

This first ladder keeps the existing 1.0-V GPU rail and 1.8432-GHz PLL VCO.
Only GM20B's linear PL divider changes, under bypass as in Switchroot's
`clk_change_pldiv_under_bypass`. The driver serializes a request with every
GPU submission, requires a retired FIFO channel and idle GR, then measures
the new GPCCLK. A failed readback attempts to restore the prior divider; an
unrecoverable mismatch isolates the GPU. The kernel policy updates its target
only after the driver confirms success. Voltage scaling is not enabled. The
generic governor samples device utilization every 25 ms, applies Linux's
90%/85% simple-ondemand thresholds and waits four low samples before reducing
the rate. The separate GPU thermal zone uses the kernel cap
for SOCTHERM readings at 90.5°C and 100°C. These thermal limits have not been
reached in hardware testing. The earlier manual devfreq-only candidate's immutable bundle is
`.cache/gm20b-linux-audit/devfreq-fixedrail-candidate/bundle.json` and its
uImage SHA-256 is
`49ceec2ed4ca80bf85f063f5b043e770db0a19c7ed7e765b3d8f4c81e2959162`.

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

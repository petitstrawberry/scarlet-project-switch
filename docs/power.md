# Switch battery and input-power telemetry

The console BSP enables `scarlet-driver-switch-power`. It registers
`switch-battery` and, when detected, `switch-usb-input` with Scarlet's common
power-supply API. `power-info` reads the same API used by Scarlet Shell.

The driver binds the existing `maxim,max17050` node on Tegra I2C1, verifies the
gauge device ID and uses `maxim,rsns-microohm` from the selected device tree.
The adjacent BQ24193 is identified at address 0x6b. Failure to identify that
charger does not hide the independently working gauge.

Reads set only I2C register pointers. The driver does not write gauge models,
calibration, charge limits, watchdog configuration or USB-PD negotiation state.
The firmware's setup remains in effect. The latched fault register is not read.

- Gauge: presence, reported SOC, cell voltage, average signed battery current
  and temperature. A gauge POR suppresses SOC until its model is initialized.
- Charger: power-good excluding OTG output, charge phase and input-current limit.
  Negative average battery current is reported as discharging even on external
  power (battery supplementation). A plugged-in source alone is not charging.
- An I2C failure returns unavailable data; it never becomes a synthetic 0%.

## Sources

- [MAX17047/MAX17050 data sheet](https://www.analog.com/media/en/technical-documentation/data-sheets/MAX17047-MAX17050.pdf).
- [TI BQ24193 SLUSBG7A](https://www.ti.com/lit/ds/symlink/bq24193.pdf), registers 00, 08 and 0A.
- [Hekate MAX17050 board conversions](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/power/max17050.c).
  The selected Switch DT supplies 10,000 microohms, accounting for the board's
  physical shunt and inherited current gain.

## Inspecting power state

Run `power-info` in the guest or open Scarlet Shell's Control Center.
The shell shows capacity details there; the status icon distinguishes
charging, external input without charging, and battery-only operation.

Allow one five-second polling interval after connecting or disconnecting
input power. A USB connection may supply less power than the running system
consumes, so external input and negative battery current can appear together.

Host checks cover signed conversions, fractional SOC, POR, battery absence,
bus errors, input limits, charge termination, OTG and battery supplementation.
For the general test workflow, see [development checks](testing.md).

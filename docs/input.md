# Input and RTC

The console project links the SoC, RTC, touchscreen and Joy-Con modules. The common kernel
provides native input metadata, I2C registration and timekeeping. SWS and
ScarletUI use board-independent gamepad events and optional menu navigation.
The image remains the normal console distribution and service stack.
PIO transports explicitly manage their own CAR resets and omit automatic
DMA/IOMMU resolution through the common `PlatformProbeOptions` API. The
firmware properties remain intact; existing drivers keep every dependency hook.

| Module | Device or provider | Current implementation |
| --- | --- | --- |
| `scarlet-driver-tegra210` | CAR, pinmux, GPIO, PMC, I2C controllers, UARTB/C | Declared FDT resources/phandles, bounded polling and bus recovery |
| `scarlet-driver-max77620` | PMIC on I2C5, RTC at address 0x68 | RTC read latch and wall-clock seed; LDO6 touch supply |
| `scarlet-driver-stm-ftm4` | STM FTM4 on I2C3 at 0x49 | Ten-contact type-B `/dev/touchscreenN` stream |
| `scarlet-driver-joycon` | Official attached left/right Joy-Con rails | Combined native `/dev/gamepadN`, buttons, both sticks and hat |

## Controls and scope

The Switch configuration selects East (Nintendo A) to confirm, South
(Nintendo B) to cancel, left stick or directional buttons to navigate, and
HOME to invoke the existing shell home action. Input is not translated into
keyboard events by the Joy-Con driver. SWS owns optional menu conversion;
applications can also consume native buttons and normalized axes.
See the [SWS input contract](https://github.com/petitstrawberry/Scarlet/blob/e027956300006bd8a41c42691c6925c63039fd43/docs/graphics/gamepad-input.md)
and [ScarletUI API](https://github.com/petitstrawberry/scarlet-ui/blob/6ef3e3c4da42898e8077b3698f08a95f9f718b8c/docs/GAMEPAD_INPUT.md).

Touch coordinates use the FDT's landscape logical range and the ordinary SWS
touch path; no Switch-specific UI or extra rotation is introduced. All slots
are reported in each active frame so a consumer can recover after input loss.
ScarletUI is pinned to the published `feature/refactor-input` revision in
`source-pins.toml`: it subscribes to SWS native touch frames and routes finger
drags to `ScrollView`, including momentum after release. The older mouse
compatibility path supports taps but does not provide touch scrolling. This
requires rebuilding the userspace applications in the rootfs, not just the kernel.
Controller reset, I2C failure and Joy-Con detach/stale input release state.
All configuration, FIFO and connection waits have finite deadlines. Input
workers run after normal device probing and failures do not park kernel boot.

RTC initialization requests only the read latch; it does not write calendar
fields, alarm state or reboot reason. The raw hardware calendar is interpreted
as UTC. Horizon's separate user-time offset is not available here, so the raw
RTC may differ from the time displayed by Horizon. This is a wall-clock seed;
the kernel's monotonic timer and resume behavior remain the existing timer path.

Joy-Con runtime RX drains a bounded interrupt-fed UART ring. Its worker waits
for input or the next protocol deadline and publishes each HID transition.
The ISR performs no packet parsing, allocation or inter-byte waiting.

The driver targets Erista/ODIN SKU 0 and official attached Joy-Con. Sticks
use nominal 12-bit range/center rather than factory/user calibration.
Bluetooth input, rumble, Joy-Con battery reporting, Hori controllers,
Switch Lite and Tegra USB input are outside this implementation.

## Checks

See [development checks](testing.md#console-and-input) for transport, packet
parser and synthetic SWS/ScarletUI input tests.

On the Switch, check both Joy-Con halves, directional/A/B/HOME operation,
detach/reconnect, touch selection and the Clock app. Transport errors should
release input state while allowing the normal shell to remain usable.

## Reference priority

Use the Switchroot L4T Linux drivers as the primary reference for device
initialization, power sequencing and live input handling. The installed
Kubuntu Noble release is L4T 5.1.2; pin source comparisons to that release
where available. Hekate remains a supplementary reference for the firmware
state inherited at boot, board pin wiring and the bootstrap protocol.
Keep source-derived expectations separate from observed Scarlet hardware
results.

## Source provenance

The primary device reference is Switchroot's Linux 5.1.2 release, pinned to `CTCaer/switch-l4t-kernel-4.9` commit
`2d0059fd3167a8df756de2aa0489d4aa70a9fc15`:

- [Linux attached Joy-Con](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/input/joystick/joycon-serdev.c)
- [Linux STM FTM4 touchscreen](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/input/touchscreen/stm/ftm4_ts.c)
- [Linux Tegra I2C](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/i2c/busses/i2c-tegra.c)
- [Linux Tegra UART](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/tty/serial/serial-tegra.c)
- [Linux MAX77620 RTC](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/rtc/rtc-max77686.c)

Register sequences and wire commands were checked against Hekate v6.5.3,
commit `e487de8fdd6ca9c3f608d1d18c097a86355912b9`, by naehrwert and CTCaer.
The external driver crates retain **GPL-2.0-only** licensing for the adapted
sequences and protocol data. Firmware is imported from the pinned installed
Noble bootstack rather than added to source control.

- [Hekate Tegra I2C](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/soc/i2c.c)
- [Hekate attached Joy-Con](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/input/joycon.c)
- [Hekate touchscreen](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/input/touch.c)
- [Hekate RTC](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/power/max77620-rtc.c)
- [Hekate shared 5 V rail](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/power/regulator_5v.c)
- [Linux Tegra bus-clear status/STOP definition](https://github.com/torvalds/linux/blob/2779759c090ea0e78109a0cad0a81d869adfb459/drivers/i2c/busses/i2c-tegra.c)

The actual ODIN DTB and Noble U-Boot sources used for address, phandle,
polarity and pin-range verification are described in the [boot contract](boot-architecture.md).


## Software keyboard

The rootfs starts `soft-keyboard` as an SWS input-panel provider. Focusing a text
editor on a touch device opens it; **Hide** dismisses it and touching an editor
opens it again. Terminal requests the same panel when its terminal view is
touched. The landscape layout follows the iPad key grid: Tab/Delete flank the
Q row, Ctrl/Return flank the A row, Shift flanks the Z row, and the bottom row
uses language, layer, Alt, Space, layer, and Hide keys. It also provides numbers,
one-shot Shift, fast double-tap Shift lock, cursor keys, and the **あ / A** IME toggle.
Mozc/SKK remain the selected conversion engine. The panel keeps keyboard focus
in the editor; focused and maximized windows resize above it. Touch capture
keeps small finger motion and the visual inter-key gaps inside the nearest key.

The SWS/TextInput extension and ScarletUI touch activation are carried by the
recorded patches in `source-pins.toml`, so published builds do not require sibling
checkouts. Rebuild the rootfs to update the server, clients, keyboard, and service
configuration together. `tests/test-input.py --panel` exercises the installed
keyboard's touch-to-key path and editor focus in QEMU.

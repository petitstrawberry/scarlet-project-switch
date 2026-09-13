# Switch input and RTC bring-up

The console project links four ordinary external driver modules, using the
same manifest/module layout as the Chromebook projects. The common kernel
provides native input metadata, I2C registration and timekeeping. SWS and
ScarletUI use board-independent gamepad events and optional menu navigation.
The image remains the normal console distribution and service stack.
PIO transports explicitly manage their own CAR resets and omit automatic
DMA/IOMMU resolution through the common `PlatformProbeOptions` API. The
firmware properties remain intact; existing drivers keep every dependency hook.

| Module | Device or provider | Current implementation |
| --- | --- | --- |
| `scarlet-driver-tegra210` | CAR, pinmux, GPIO, PMC, I2C3/5, UARTB/C | Declared FDT resources/phandles, bounded polling and bus recovery |
| `scarlet-driver-max77620` | PMIC on I2C5, RTC at address 0x68 | RTC read latch and wall-clock seed; LDO6 touch supply |
| `scarlet-driver-stm-ftm4` | STM FTM4 on I2C3 at 0x49 | Ten-contact type-B `/dev/touchscreenN` stream |
| `scarlet-driver-joycon` | Official attached left/right Joy-Con rails | Combined native `/dev/gamepadN`, buttons, both sticks and hat |

## Controls and scope

The Switch configuration selects East (Nintendo A) to confirm, South
(Nintendo B) to cancel, left stick or directional buttons to navigate, and
HOME to invoke the existing shell home action. Input is not translated into
keyboard events by the Joy-Con driver. SWS owns optional menu conversion;
applications can also consume native buttons and normalized axes.
See the sibling [SWS input contract](../../Scarlet/docs/graphics/gamepad-input.md)
and [ScarletUI API](../../scarlet-ui/docs/GAMEPAD_INPUT.md).

Touch coordinates use the FDT's landscape logical range and the ordinary SWS
touch path; no Switch-specific UI or extra rotation is introduced. All slots
are reported in each active frame so a consumer can recover after input loss.
Controller reset, I2C failure and Joy-Con detach/stale input release state.
All configuration, FIFO and connection waits have finite deadlines. Input
workers run after normal device probing and failures do not park kernel boot.

RTC initialization requests only the read latch; it does not write calendar
fields, alarm state or reboot reason. The raw hardware calendar is interpreted
as UTC. Horizon's separate user-time offset is not available here, so the raw
RTC may differ from the time displayed by Horizon. This is a wall-clock seed;
the kernel's monotonic timer and resume behavior remain the existing timer path.

Initial limitations: Erista/ODIN SKU 0 only, attached official Joy-Con only,
polling instead of GPIO interrupt demultiplexing, and nominal 12-bit stick
range/center rather than factory/user calibration. Bluetooth, rumble, battery
reporting, Hori controllers, Switch Lite, SD storage and Tegra USB input are
not implemented. CPU0-only operation remains a separate bring-up item.

## Validation

```sh
nix develop --accept-flake-config
scripts/build-console.sh
sh tests/test-input-host.sh
tests/test-console.sh
python3 tests/test-input.py

python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD"
python3 scripts/install-sd.py --console --mount "/Volumes/SWITCH SD" --write
diskutil eject "/Volumes/SWITCH SD"
```

Host tests cover actual packet construction, NACK/arbitration/FIFO deadlines,
bus-clear deadline, Gregorian/BCD/12-hour calendar decoding, FTM4 contact
packing/tracking and reset, incremental rail packets, initialization ACK
ordering, backoff, stick/button ownership and per-half detach. Production SWS
policies cover normalization, hysteresis, A/B configuration, source-scoped key
ownership, focus resets, multi-device backlog recovery and dropped frames.
ScarletUI tests cover focused event dispatch and button identity.

`test-console.sh` boots the packaged production Image at EL1 and EL2, checks
the actual SWS/console shell rendering, file/catalog access and timer wakeups,
and checks the unchanged RAMDisk without UART. It also checks the Cortex-A57
instruction set and SD tooling.

`test-input.py` builds a separate kernel under `.cache/input-qa-project` with
the **test-only** `scarlet,input-qa` fixture. A native guest writes input
records into that fixture's control device and observes the normal
EventDevice → SWS → sws-client → ScarletUI path. It checks menu keys, raw
mode without menu keys, focus-loss reset, `SYN_DROPPED` recovery and a real
`Application::on_gamepad` callback with axes/buttons/release. The fixture also
declares unavailable reset/DMA/IOMMU providers: the PIO implementation must
probe, while three default-policy drivers must defer before their callbacks.
The guest then operates the real `scarlet-shell --mode console` Home: Right
selects Files, Nintendo A launches its installed application, HOME returns to
Home, and Nintendo B restores the workspace. Asynchronous subscription changes
use an IPC barrier, and input observations have bounded deadlines.
No test fixture is linked into the production Image or installed to SD.
QEMU does not emulate
Tegra peripherals; these results cannot establish physical driver success.

On hardware, select **More Configs → Scarlet Switch Console** (`SCR-NXC`).
Check Home colors first, then directional/A/B/HOME operation with both Joy-Con
attached, each side's detach/reconnect, touch selection, and the Clock app.
If input fails, the image must still reach the normal shell. The current
hardware state is explicitly recorded in `input-verification.json`.

The first driver image (`Image` SHA-256
`995f0419e95673d865eb7edef18b0937ab9e9d16ee1317dd4edf5aa9dc864d97`)
booted to the shell, but the user reported Joy-Con, touch and RTC all failing.
`IMG_9061.mov` confirms successful CAR/pinmux/GPIO/PMC probes followed by
deferred UART and I2C transports and their children. The generic reset hook
required an unregistered reset provider before the transports could run their
own CAR sequence; unused DMA/IOMMU hooks would also defer PIO transports.
The UART check additionally used `invert-txd`/`invert-rts`, whereas the actual
ODIN firmware properties are `nvidia,invert-txd`/`nvidia,invert-rts`.
The second driver image (`Image` SHA-256
`fbf555b4c2629c5c8d061ecf37cb0d3f5e00663203a59a1f74d4e172f1afc83e`)
also reaches the normal shell. `IMG_9063.mov` shows both rails attached and
a successful RTC wall-clock seed in that recording. The user reports RTC
intermittent, Joy-Con unable to operate console Home, and touch completely
unresponsive. Physical input remains unresolved.

The scale-only installation retained the second hardware attempt's kernel
and changed SWS output scale to `2.0`.

The third hardware attempt (`Image` SHA-256
`eb50c1944fec35a1279a678b374e10fa93df8c2fcd9f84abbcfb940af5d32016`)
retains scale `2.0` and adds short STOP-terminated command-register I2C
transfers, LDO6 FPS detachment and power-good checks, bounded RTC latch
retries, synchronous touch initialization with background recovery, and a
single UART write for the Joy-Con wake/handshake sequence. Rate-limited
transport and connection-stage logs identify failures for the next hardware
boot. All eight SD files passed readback checks, all 38 protected files
retained their hashes, and the SD was ejected. `IMG_9064.mov` reaches the
normal shell, but the user reports input still unresponsive. I2C3 fails
during probe and touch remains deferred. RTC reports `invalid RTC BCD digit`
on all three initial attempts. Both Joy-Con reach the Rate stage; the
recording does not establish a later Ready stage or a first HID report.

For that third attempt, the production build and normal console boot checks
passed. Its
synthetic input run reached SWS delivery and the ScarletUI callback but
failed its assertion that Home navigation launched Files. That failure is
recorded in `input-verification.json`; it is not a passing Home result.
Further synthetic-test investigation is deferred in favor of hardware
bring-up, as requested by the user.

The fourth hardware attempt (`Image` SHA-256
`84cc0a7e1507fe55fb5b3b8aac16465f26fb55597baa9d57cb2351284eebd450`)
retains the same scale `2.0` initramfs. Linux 5.1.2 comparisons exposed the
reversed RTC mode flag: `BCD_EN=0` means binary, not BCD. I2C probe now
registers the controller without requiring bus-clear success before its
children can power up; transfer-error recovery remains enabled. The exact
I2C3 error suffix was not readable in the video, so this explanation is a
source-based hypothesis to check on hardware. Touch now follows Linux's
direct ready-event reads and power/reset/sense delays. Joy-Con ACKs are
matched by command/subcommand, packet framing uses the outer length, and
ready connections do not temporarily switch the active TX pin to GPIO for
attachment detection. Bounded initialization during ordinary probe logs
the first HID report or the stopping stage before the shell starts.

This candidate was formatted and built, and all eight installed files passed
SD readback checks with all 38 protected files unchanged. The SD was ejected.
No additional host or QEMU tests were run, as requested. `IMG_9066.mov`
shows touchscreen0 ready with sensing enabled, and the user confirms touch
works. The RTC wall-clock seed succeeds in this boot; accuracy and repeated
boot stability remain unverified. Joy-Con still does not operate the shell.
Both rails reach Rate; Right stops in Backoff with only 68 received bytes,
while Left reports a combined UART framing/overflow error. No first HID
report is established by the recording.

The fifth hardware attempt (`Image` SHA-256
`ac5e1b5e7db813a70c0907bfac91140fdc86b895d987d0464d7db06eb14fea26`)
changes only the UART transport and rail diagnostics. It adopts Linux's
hardware CTS/RTS flow control, TX FIFO-full status, PIO FIFO configuration,
and T210 FIFO-reset sequencing with posted-write and clock-period waits.
TX has a bounded 100 ms deadline to accommodate CTS pauses. RX errors now
identify the per-character LSR bits and received-byte sample, and failures
print UART line/modem/clock registers. The worker logs its first HID report
as well as the initial probe. These are source-based corrections; working
physical Joy-Con input is still unverified. Formatting and the production
build passed, with the unchanged scale `2.0` initramfs. All eight installed
files passed SD readback checks, all 38 protected files retained their hashes,
and the SD was ejected. `IMG_9067.mov` shows Left advancing Rate → Ready
and a real `first HID id=0x30` report. The user confirms Left works, and
the Home selection moves Clock → Files → Notepad. Right still stops after
Connect → Rate with 68 received bytes and no first HID; the user reports
its A and HOME apparently unresponsive. Right's UART snapshot is
`LSR=260 MCR=60 MSR=5b LCR=7 clock=100000f` (hex). The RTC seed and touch
registration succeed again; absolute clock accuracy remains unverified.

The sixth candidate (`Image` SHA-256
`29031b734b4b53a59c2a976487a52f0d8c8387d5fd883391b5f4790f57c5bd16`)
adds a bounded HID-input verification phase after a missing rate ACK.
The connection must already have been acknowledged. After sending the
interval command and waiting 20 ms, it queries actual input up to ten times
at 15 ms intervals. A recognized HID report advances to Ready and is
published normally; a delayed rate ACK also retains the existing path.
Silence or UART errors still fail initialization. This is a hardware-driven
robustness hypothesis; Linux itself treats a missing rate ACK as a handshake
failure. Detection switches TX to GPIO only while detached or in backoff,
keeping ongoing initialization and verification in UART mode. First-HID
logs include the preceding stage, so `via=VerifyInput` identifies success
without the interval ACK. Nintendo A → East and HOME → Mode → the normal
SWS home chord remain the common input bindings.

This candidate was formatted and built, with the same scale `2.0` initramfs.
No new host or QEMU tests were run. All eight installed files passed SD
readback checks, all 38 protected files retained their hashes, and the SD
was ejected. `IMG_9070.mov` still shows Right returning VerifyInput → Backoff with
68 received bytes and no first HID. Left receives a delayed rate ACK, then
logs a real HID report; the user confirms only Left works.

The next candidate (`Image` SHA-256
`7ded7baf9d496d7cf5b49e666d882efd7ef3f3a31f6085ad24736037cbe13ab9`) adds Linux Image PSCI SMP with console `maxcpus=4`; see
[cpu bring-up](cpu-bringup.md). Joy-Con initialization now follows Linux's
one-second ACK waits with one retry, and a 100 ms handshake deadline.
HID verification follows both Rate attempts, rather than beginning after
20 ms. Ordinary probe has a bounded 12-second total deadline covering those
stages. Left's delayed rate ACK in `IMG_9070.mov` confirms that the previous
20 ms wait was shorter than an observed reply; working Right input remains
unverified. Formatting, production build and entry disassembly passed.
No new host or QEMU tests were run. The scale `2.0` initramfs is unchanged.
All eight files passed SD readback checks, all 38 protected files retained
their hashes, and the SD was ejected. Physical SMP and Right input validation
are pending this candidate’s next Switch boot.

## Reference priority

Use the Switchroot L4T Linux drivers as the primary reference for device
initialization, power sequencing and live input handling. The installed
Kubuntu Noble release is L4T 5.1.2; pin source comparisons to that release
where available. Hekate remains a supplementary reference for the firmware
state inherited at boot, board pin wiring and the bootstrap protocol.
Keep source-derived expectations separate from observed Scarlet hardware
results.

## Source provenance

The primary device reference is Switchroot's Linux 5.1.2 release, obtained
with `gh` and pinned to `CTCaer/switch-l4t-kernel-4.9` commit
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
polarity and pin-range verification are described in `boot-architecture.md`.

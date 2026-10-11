# Shutdown and reboot

`shutdown` (or `shutdown -P now`) requests power removal. `reboot` and
`shutdown -r now` request the firmware's default restart behavior. ScarletShell's
power menu uses the same stemd request path. The commands acknowledge acceptance;
they cannot acknowledge completion after power has been removed.

stemd stops accepting new application launches, sends TERM to user processes,
and allows two seconds for exit. It then sends KILL and allows one more second.
All init threads and kernel workers remain runnable. If user processes remain,
stemd cancels the hardware request and permits launches again. Ordinary process
exit closes user handles; this is not a global filesystem sync/unmount barrier
and does not flush every mapping or storage-device write cache.

On Switch, the MAX77620 driver registers the board poweroff operation. It clears
reset/alarm wake in ONOFFCNFG2, verifies the readback, then requests the PMIC
software-reset power-down sequence in ONOFFCNFG1. I2C failure or a rejected wake
configuration prevents the final write. A returned poweroff request is reported
as a failure. This follows the regulator-reset shutdown used by
[Switchroot Linux](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/power/reset/max77620-poweroff.c)
and [Hekate POWER_OFF_RESET](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/utils/util.c).

Reboot uses PSCI SYSTEM_RESET through the Device Tree's `method` (`smc` on
Switch). It does not change boot-selection scratch registers or force RCM.
The destination is selected by the installed firmware. Boards without a
registered poweroff operation use PSCI SYSTEM_OFF. The
[referenced Switchroot T210 ATF implementation](https://gitlab.com/switchroot/bootstack/switch-atf/-/blob/8902c539ce83b15637c5f47fc8aff5d14d95b992/plat/nvidia/tegra/soc/t210/plat_psci_handlers.c)
does not implement SYSTEM_OFF, hence the board PMIC operation.

## Validation

Inside `nix develop`, build the production project, then run:

```sh
cargo scarlet image --project projects/aarch64-switch-l4t-console --release
cargo test --manifest-path drivers/rtc/max77620/Cargo.toml
python3 tests/qemu-system-power.py
```

The QEMU test boots the production kernel, init, stemd, shutdown and reboot in
an isolated RAM filesystem. It replaces the service list with a live sbusd daemon
and one power command, and checks that QEMU actually exits for both SMC/HVC and
both power operations. The result records whether stemd reaped sbusd after TERM.
It also exercises timer wakeups on a GICv2 without Security Extensions. The
fixture runs one active CPU; it does not emulate Tegra peripherals or the PMIC.
Logs and binary hashes are saved under `.cache/system-power/qemu`.

Scarlet's `tools/test-system-power.py` tests conduit selection, protocol/CLI
parsing, process exclusion, TERM/KILL escalation, new descendants, and refusal
to power off with a stuck process. The PMIC host tests check write ordering,
preserved settings and I2C/readback failures.

Physical validation remains necessary: verify that shutdown removes power and
that reboot reaches the firmware's configured destination on the installed
Switch firmware. QEMU success is not evidence of PMIC power removal.

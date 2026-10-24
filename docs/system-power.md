# Shutdown and reboot

`shutdown` (or `shutdown -P now`) requests power removal. `reboot` and
`shutdown -r now` request normal board restart without forcing RCM. ScarletShell's
power menu uses the same stemd request path. The commands acknowledge acceptance;
they cannot acknowledge completion after power has been removed.

stemd stops accepting new application launches, sends TERM to user processes,
and allows two seconds for exit. It then sends KILL and allows one more second.
All init threads and kernel workers remain runnable. If user processes remain,
stemd cancels the hardware request and permits launches again. Ordinary process
exit closes user handles; this is not a global filesystem sync/unmount barrier
and does not flush every mapping or storage-device write cache.

The kernel's `system_power` module owns the final transition API. One board
controller implements both `PowerOff` and `Restart`. Peripheral power domains
and reset lines remain independent. Registration finishes before userspace;
callbacks run without a registry lock and retain interrupt-driven I/O workers.
A board failure is reported and parked, never retried through another mechanism.
Without a board controller, AArch64 uses DT-selected PSCI and RISC-V uses SBI.

The Switch board policy in `switch-power` registers only for the root DT
compatible `nintendo,icosa`. QEMU therefore retains the firmware default.
Registration is independent of battery telemetry probing; missing PMIC or PMC
resources produce explicit errors when the operation is requested.

For poweroff, the board calls the MAX77620 driver's operation. It clears
reset/alarm wake in ONOFFCNFG2, verifies the readback, then requests the PMIC
software-reset power-down sequence in ONOFFCNFG1. I2C failure or a rejected wake
configuration prevents the final write. A returned poweroff request is reported
as a failure. This follows the regulator-reset shutdown used by
[Switchroot Linux](https://github.com/CTCaer/switch-l4t-kernel-4.9/blob/2d0059fd3167a8df756de2aa0489d4aa70a9fc15/drivers/power/reset/max77620-poweroff.c)
and [Hekate POWER_OFF_RESET](https://github.com/CTCaer/hekate/blob/e487de8fdd6ca9c3f608d1d18c097a86355912b9/bdk/utils/util.c).

For restart, the board calls the Tegra210 PMC driver. Following
[Switchvisor's normal reset](https://github.com/petitstrawberry/switchvisor/blob/8d2bc1db0dc858fb0b253540397e8105f137cba5/crates/switchvisor/src/usb.rs#L550),
it preserves PMC_CNTRL and sets MAIN_RST (bit 4), with memory barriers before
and after. It does not alter SCRATCH0 or force RCM. This avoids the BL31 reset
handler, whose [referenced implementation](https://gitlab.com/switchroot/bootstack/switch-atf/-/blob/8902c539ce83b15637c5f47fc8aff5d14d95b992/plat/nvidia/tegra/soc/t210/plat_psci_handlers.c)
can enter reboot-to-payload before resetting. That source does not establish
the exact firmware installed on the device or the location of the reported
hang. A returned PMC request is reported as a failure after one second.

## Validation

Inside `nix develop`, build the production project, then run:

```sh
cargo scarlet image --project projects/aarch64-switch-l4t-console --release
cargo test --manifest-path drivers/rtc/max77620/Cargo.toml
cargo test --manifest-path drivers/soc/tegra210/Cargo.toml
tests/test-system-power.sh
```

The QEMU test boots the production kernel, init, stemd, shutdown and reboot in
an isolated RAM filesystem. It replaces the service list with a live sbusd daemon
and one power command, and checks that QEMU actually exits for both SMC/HVC and
both power operations. The result records whether stemd reaped sbusd after TERM.
It also exercises timer wakeups on a GICv2 without Security Extensions. The
fixture runs one active CPU; it does not emulate Tegra peripherals or the PMIC.
Logs and binary hashes are saved under `.cache/system-power/qemu`.

A second four-case run uses a native process sleeping in `FutexWait` with no
timeout or userspace waker. Before the interruptible-futex fix, both TERM and
KILL remained queued, stemd cancelled the transition after three seconds, and
QEMU stayed running. The regression requires the sleeper to exit and the firmware transition to
complete through the production process-drain path. Zombie reaping may race
the final transition and is recorded without being required. Logs are saved under
`.cache/system-power-futex/qemu`. This proves the futex failure path, not that
the physical Switch's reported hang has the same cause.

Scarlet's `tools/test-system-power.py` tests board ownership of both operations,
firmware selection only when no board is registered, conduit selection, protocol/CLI
parsing, process exclusion, TERM/KILL escalation, new descendants, and refusal
to power off with a stuck process. The PMIC host tests check write ordering,
preserved settings and I2C/readback failures. The PMC host test verifies ordered
barriers and a single preserved-bit control write with no scratch-register access.

Physical validation remains necessary: verify that shutdown removes power and
that reboot reaches the firmware's configured destination on the installed
Switch firmware. QEMU success is not evidence of PMIC power removal.

## Switchvisor investigation (2026-10-11)

The `237dec6596c16e43558ff6a60d73b82a21296fcc` kernel still hung after the
physical desktop Restart request. Captured UART output reached
`stemd: Initiating Reboot; waiting for user processes to exit`; Switchvisor
continued answering control requests with four guest CPUs active. The ordinary
kernel console did not select the DT's ns16550 UART, and the `/dev/kmsg` reader
was itself a shutdown target, so this capture does not establish whether the
final hardware syscall ran. Switchvisor's own `reboot-rcm` subsequently worked.

Diagnostic revision `701060519ff67455209a6d0c267436762e3dbc59` activates the
DT-selected, byte-wide runtime 16550 for direct kernel output. The project
enabled `sync-debug` to report init task snapshots, signal delivery,
and lock contention without a userspace log reader. This is an investigation
configuration, not evidence that physical Restart or shutdown is fixed.
Evidence is retained in `.cache/system-power-switchvisor/`.
The normal desktop image now disables this tracing to avoid its per-lock
runtime cost; enable it explicitly for another diagnostic capture.

The next capture showed service exits while kernel thermal workers remained
alive. Revision `24e9d7f2f6c0534b24e0c8f0564c80a1e7d2b6d1` also moved the
emergency emitter from the boot framebuffer to the selected UART (ordinary
UART registration had previously lost to the first-registration rule).
With that instrumentation, an SSH reboot through the same stemd IPC path
stopped after TERM delivery. `top --debug` showed login PID 25 executing native
waitpid on CPU 2, with the CPU timer IRQ count fixed at 38965 across samples
at 99, 111 and 132 seconds; the reboot handler TID 183 was Ready on CPU 2.
The shell remained usable on another CPU. Runtime stemd and scarlet-shell
readbacks matched the staged SHA-256 values exactly.

Native waitpid discarded `WaitResult::Interrupted` in both its specific-child
and wait-any loops. A pending fatal signal made subsequent waits return
immediately, preventing return to the ABI signal-delivery hook. Revision
`13e4414c855ac315c81b139f7407f161ca2ac0e8` returns the native syscall error
instead. The regression adds a parent blocked in each waitpid mode and a
TERM-masked child (still killable by KILL). Both modes timed out on the old
kernel before firmware transition, using byte-identical fixtures and userspace.
All 16 QEMU cases pass on the fixed kernel: normal drain, native futex,
specific-child waitpid and wait-any, each with SMC/HVC reboot and shutdown.
The physical recheck of this revision reaped login, then stalled in a different
wait: files PID 72 was in native Sleep on CPU 2, where the timer IRQ counter
stayed at 3471, while init's power handler TID 135 remained Ready. Files sleeps
for one hour in service mode. Native Sleep reused the full-duration kernel
sleep loop, which kept retrying after TERM instead of returning to ABI delivery.
Revision `b1f4f37472f5edbeff68825430facc86cec4f8c1` gives native Sleep a private
interruptible timeout and returns the native error on interruption, retaining
kernel/device delay semantics. The new regression first checks an ordinary
20 ms sleep, then requests shutdown while sleeping for one hour. It times out
on the previous kernel. Evidence is in `.cache/system-power-sleep/`.
All 20 QEMU cases passed on b1f4f374, including the added Sleep cases; the
baseline uses exactly the same userspace/fixture hashes. On hardware, drain
progressed and then cancelled after its three-second deadline. Remaining groups
at KILL escalation were 133, 57 (soft-keyboard) and 66 (scarlet-skk); the capture
also records a timer stall on CPU 3 while mozc_server executed Linux madvise.
That does not yet establish which condition kept the last tasks alive.
Diagnostic revision `d24aa54245bf865601fd4be5301a585868fd5512` samples live
non-init user tasks during init enumeration every 500 ms, including CPU, state,
active syscall and pending process control. With that revision, a physical
reboot reached `Restart via Switch PMIC/PMC` and `requesting normal system reset`,
then USB changed from Switchvisor to APX (0955:7321). A subsequent shutdown
reached `[SHUTDOWN] Powering off...` and both Switchvisor and APX disappeared
from USB; confirmation of the physical screen/fan power state is pending.
Captures are `physical-drain.log` and `physical-shutdown.log` under the Sleep
evidence directory. This establishes one reset, not repeated reliability:
the preceding three-second drain cancellation remains an open issue. The
current revision has only been uploaded through Switchvisor, not installed
to SD yet.

The static follow-up also identified that native Poll with no handles still
uses the full-duration kernel sleep path (including an infinite timeout).
Both new QEMU fixtures time out on d24aa542 before firmware transition.
Revision `72e68549b679f5ff471b79847675bbb5ec3dc73a` separates interruptible
userspace sleep from full-duration device delays, and routes native Sleep,
empty Poll and selectable minimum waits through the interruptible path. The
expanded regression adds an infinite empty Poll and an unreadable pipe with a
one-hour minimum Poll wait. The final image and boot-probe build pass, as do all 28 QEMU cases and 11
power-policy host tests. Both Poll regressions use byte-identical fixtures and
userspace on the baseline and fixed kernels. On the physical Switch, the final
revision reached `Restart via Switch PMIC/PMC` and USB changed to APX again;
see `physical-final-reboot.log`. The final `uImage` SHA-256 is
`0f08ca72df32cb0983eee10ed34e5b8e76a48ca4cae3b435883cb0c080119ce9`.
The eight regular Scarlet Console FAT boot files were installed through Hekate
UMS, read back against their hashes, and the disk was ejected. The installer
also verified 47 protected files were unchanged and did not access raw devices.
No rootfs update was needed; userspace hashes are unchanged. The SD-installed
normal boot path has not yet been physically retested. One successful reset on
the final revision does not resolve the earlier intermittent drain cancellation
by itself; retain the diagnostic captures for further failures. Final records:
`.cache/system-power-sleep/final-result.json` and `sd-installation.json`.

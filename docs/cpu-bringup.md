# Linux boot CPU limitation

The current L4T path starts only the boot CPU. In
`Scarlet/kernel/src/arch/aarch64/boot/linux.rs`, `BootInfo::new` receives
`cpu_count = 1` and `start_secondary_cpus_hook = None`. The Switch BSP's
`_entry_ap` also parks instead of entering the common AP startup path.
Consequently, the kernel's “Detected 1 CPU(s)” message describes the current
boot implementation, not the physical CPU topology.
The inspected ODIN DTB contains four enabled Cortex-A57 CPU nodes, all with
`enable-method = "psci"`; `/psci` advertises `arm,psci-1.0` and `method = "smc"`.
The boot script also currently supplies `maxcpus=1`.

The Limine path already has bootloader-assisted AP bootstrap and a
`start_secondary_cpus` hook. The Linux Image path cannot use Limine responses;
it needs its own firmware CPU-start mechanism, per-CPU entry/stack setup,
exception-level and MMU transitions, topology registration, and a hook to
release APs after global initialization. Reporting four CPUs before those
CPUs can enter the scheduler would conceal the missing work.

The existing QEMU fixture launches four emulated CPUs but describes only CPU0
in its DTB and uses `maxcpus=1`. Its fourteen passing cases verify single-CPU
boot and timer wake, not SMP. The hardware success record also explicitly
records `single_cpu = true`.

The next SMP check must independently observe each CPU online in the
scheduler, run tasks on every CPU, and retain sleep/wake coverage.

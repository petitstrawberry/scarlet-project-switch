# Bounded first-Home trace boot

This is an opt-in runtime diagnostic profile, outside the normal image layers.
Use an upstream Scarlet revision with `SCARLET_HOME_TRACE` instrumentation and
explicit forwarding of that flag from `scarlet-desktop` to its shell child.
The normal supervisor, desktop mode and tablet posture are retained.

Copy these files to a deployment staging directory before using the network
transfer helper, which writes a sibling `.serial.log`. Stage and read back the
two scripts for `/etc/sws/` and the service file for
`/etc/stemd.d/services/01-sws.toml`, then use the verified same-directory rename
procedure in [storage](../../docs/storage.md#runtime-updates). Install the scripts
before replacing the service file. Reboot through Switchvisor USB deployment.

SWS readiness uses the service name and accepts notification from the child
started by the wrapper. The shell exports the diagnostic flag explicitly;
stemd clears the environment and has no per-service environment table.
Capture stdout in `/tmp/home-trace-sws.log` and `/tmp/home-trace-shell.log` to
avoid synchronous UART logging. Do not enable the unbounded UI/SWS profilers.

After a fresh boot, open Home directly once without launching another app or
entering Overview. Save captures after 2, 4 and 15 seconds, the two trace files,
and any later capture with its elapsed time and human interactions recorded.
Treat RFB and physical-panel results separately.

After collecting the logs, restore the normal service file from
`projects/aarch64-switch-l4t-console/rootfs/etc/stemd.d/services/01-sws.toml`
with the same verified staging procedure, then reboot. The normal desktop
startup script is not modified. Remove the two diagnostic scripts after the
normal service file has been restored. No SD backup or UMS session is needed.

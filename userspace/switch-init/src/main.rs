#![no_std]
#![no_main]

extern crate scarlet_std as std;

use core::fmt::{self, Write};

// PID 1 has no inherited stdio handles. Use the native diagnostic console so
// arrival can be observed before devfs/TTY policy is configured.
fn boot_log(message: &str) {
    for byte in message.bytes() {
        // SAFETY: Putchar accepts one scalar byte and no userspace pointer.
        let _ = unsafe { std::syscall::syscall1(std::syscall::Syscall::Putchar, byte as usize) };
    }
}

struct BootConsole;

impl Write for BootConsole {
    fn write_str(&mut self, message: &str) -> fmt::Result {
        boot_log(message);
        Ok(())
    }
}

fn monotonic_time_ns() -> u64 {
    // SAFETY: This AArch64 clock query returns a scalar u64, without pointers.
    unsafe { std::syscall::syscall0(std::syscall::Syscall::MonotonicTime) as u64 }
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    boot_log("SCARLET SWITCH USERSPACE REACHED\n");
    boot_log("initramfs /init; no persistent storage mounted\n");
    for _ in 0..2 {
        for milliseconds in [20, 100, 1000] {
            let requested_ns = milliseconds * 1_000_000;
            let before = monotonic_time_ns();
            let result = std::thread::sleep(std::time::Duration::from_millis(milliseconds));
            let after = monotonic_time_ns();
            let elapsed_ns = after
                .checked_sub(before)
                .expect("monotonic clock went backwards");
            let _ = writeln!(
                BootConsole,
                "TIMER_CHECK requested_ns={requested_ns} elapsed_ns={elapsed_ns} result={result}"
            );
            assert_eq!(result, 0, "native Sleep failed");
            assert!(
                elapsed_ns >= requested_ns,
                "native Sleep returned before its deadline"
            );
        }
    }
    boot_log("SCARLET SWITCH TIMER WAKE REACHED\n");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

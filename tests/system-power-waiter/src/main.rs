//! Native blocking syscall fixtures for the production shutdown drain.
use core::sync::atomic::AtomicU32;
use scarlet_sys::{Syscall, syscall3};
use std::time::Duration;

static WORD: AtomicU32 = AtomicU32::new(0);

fn main() {
    let mode = std::env::args().nth(1).expect("wait, shutdown, or reboot");
    if mode == "waitpid-child" {
        // Block TERM so killing the child first cannot hide a stuck waitpid.
        // KILL remains unmaskable and lets the production drain finish.
        assert_eq!(unsafe { syscall3(Syscall::EventMask, 0, 0, 0) }, 0);
        std::fs::write("/tmp/power-waitpid-ready", b"ready").unwrap();
        loop { core::hint::spin_loop(); }
    }
    if matches!(mode.as_str(), "waitpid" | "waitpid-any") {
        let child = std::process::Command::new("/bin/system-power-waiter")
            .arg("waitpid-child").spawn().expect("spawn wait target");
        while !std::path::Path::new("/tmp/power-waitpid-ready").exists() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let pid = if mode == "waitpid-any" { usize::MAX } else { child.id() as usize };
        println!("[power-waiter] entering native {mode} with TERM-blocked child");
        let mut status = 0i32;
        // SAFETY: The output status remains writable throughout this wait.
        let result = unsafe { syscall3(Syscall::Waitpid, pid, &mut status as *mut i32 as usize, 0) };
        panic!("waitpid target unexpectedly returned: {result}");
    }
    if matches!(mode.as_str(), "poll-empty" | "poll-min") {
        #[repr(C)]
        struct Options { timeout_ns: i64, minimum_ns: u64 }
        #[repr(C)]
        struct Handle { handle: u32, events: u16, revents: u16 }
        let options = Options { timeout_ns: -1, minimum_ns: if mode == "poll-min" { 3_600_000_000_000 } else { 0 } };
        let mut handles = [0u32; 2];
        let mut entry = Handle { handle: 0, events: 1, revents: 0 };
        let count = if mode == "poll-min" {
            // Keep both pipe endpoints open, with no readable data.
            assert_eq!(unsafe { syscall3(Syscall::Pipe, handles.as_mut_ptr() as usize, 0, 0) }, 0);
            entry.handle = handles[0];
            1
        } else { 0 };
        println!("[power-waiter] entering native {mode}");
        let result = unsafe { syscall3(Syscall::Poll, &mut entry as *mut Handle as usize, count, &options as *const Options as usize) };
        panic!("poll target unexpectedly returned: {result}");
    }
    if mode == "sleep" {
        // Check ordinary sleep still meets its deadline before the fatal case.
        let start = std::time::Instant::now();
        assert_eq!(scarlet_sys::sleep_ns(20_000_000), 0);
        assert!(start.elapsed() >= Duration::from_millis(20));
        println!("[power-waiter] entering native sleep for one hour");
        let result = scarlet_sys::sleep_ns(3_600_000_000_000);
        panic!("sleep target unexpectedly returned: {result}");
    }
    if mode == "wait" {
        println!("[power-waiter] entering native futex without a timeout");
        loop {
            // SAFETY: Static, aligned atomic word remains mapped for this wait.
            let result = unsafe {
                syscall3(Syscall::FutexWait, &WORD as *const AtomicU32 as usize, 0, usize::MAX)
            };
            assert_ne!(result, usize::MAX, "native futex wait failed");
        }
    }
    assert!(matches!(mode.as_str(), "shutdown" | "reboot"));
    // Let the earlier service enter its wait before launching the production CLI.
    std::thread::sleep(Duration::from_millis(250));
    let status = std::process::Command::new(format!("/bin/{mode}"))
        .status().expect("launch power command");
    assert!(status.success(), "power request rejected");
}

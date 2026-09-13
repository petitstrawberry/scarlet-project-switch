use sbus_client::{Argument, Connection};
use scarlet_desktop_config::{
    DESKTOP_STEMD_BUS_NAME, DESKTOP_STEMD_INTERFACE,
    DESKTOP_STEMD_LIST_APPLICATIONS_WITH_ARTWORK_METHOD, DESKTOP_STEMD_OBJECT_PATH,
};
use std::time::Duration;

fn main() {
    let content = b"console QA: ordinary writable initramfs Environment\n";
    std::fs::write("/tmp/console-qa.txt", content).expect("write to normal tmpfs backing");
    assert_eq!(std::fs::read("/tmp/console-qa.txt").unwrap(), content);
    println!("CONSOLE_FILE_IO_PASS");
    let mut bus = Connection::connect().expect("connect to the normal sbus service");
    let catalog = bus
        .call_method_timeout(
            DESKTOP_STEMD_BUS_NAME,
            DESKTOP_STEMD_OBJECT_PATH,
            DESKTOP_STEMD_INTERFACE,
            DESKTOP_STEMD_LIST_APPLICATIONS_WITH_ARTWORK_METHOD,
            vec![],
            5000,
        )
        .expect("query the real stemd application catalog");
    let names: Vec<_> = catalog
        .chunks_exact(5)
        .filter_map(|fields| {
            if let Argument::String(name) = &fields[1] {
                Some(name.as_str())
            } else {
                None
            }
        })
        .collect();
    for expected in [
        "Clock",
        "Files",
        "Notepad",
        "Settings",
        "Task Manager",
        "Terminal",
    ] {
        assert!(
            names.contains(&expected),
            "missing installed application: {expected}"
        );
    }
    println!("CONSOLE_CATALOG_PASS applications={}", names.len());
    for api in ["std", "native"] {
        for _ in 0..2 {
            for milliseconds in [20, 100, 1000] {
                let requested_ns = milliseconds * 1_000_000;
                let before = scarlet_os::time::monotonic_time_ns();
                let result = if api == "std" {
                    std::thread::sleep(Duration::from_millis(milliseconds));
                    0
                } else {
                    scarlet_sys::sleep_ns(requested_ns) as isize
                };
                let elapsed_ns = scarlet_os::time::monotonic_time_ns()
                    .checked_sub(before)
                    .unwrap();
                println!(
                    "CONSOLE_TIMER api={api} requested_ns={requested_ns} elapsed_ns={elapsed_ns} result={result}"
                );
                assert_eq!(result, 0);
                assert!(elapsed_ns >= requested_ns && elapsed_ns < requested_ns + 2_000_000_000);
            }
        }
    }
    println!("CONSOLE_FUNCTIONAL_QA_PASS");
}

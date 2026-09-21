use sbus_client::{Argument, Connection};
use scarlet_desktop_config::{
    DESKTOP_STEMD_BUS_NAME, DESKTOP_STEMD_INTERFACE,
    DESKTOP_STEMD_LIST_APPLICATIONS_WITH_ARTWORK_METHOD, DESKTOP_STEMD_OBJECT_PATH,
};
use std::time::{Duration, Instant};

fn main() {
    let content = b"console QA: writable ext2 root and temporary storage\n";
    for path in ["/tmp/console-qa.txt", "/root/console-qa.txt"] {
        std::fs::write(path, content).expect("write to normal Environment backing");
        assert_eq!(std::fs::read(path).unwrap(), content);
        std::fs::remove_file(path).unwrap();
    }
    println!("CONSOLE_FILE_IO_PASS");
    let catalog_deadline = Instant::now() + Duration::from_secs(30);
    let catalog = loop {
        let result = Connection::connect().and_then(|mut bus| {
            bus.call_method_timeout(
                DESKTOP_STEMD_BUS_NAME,
                DESKTOP_STEMD_OBJECT_PATH,
                DESKTOP_STEMD_INTERFACE,
                DESKTOP_STEMD_LIST_APPLICATIONS_WITH_ARTWORK_METHOD,
                vec![],
                1_000,
            )
        });
        match result {
            Ok(catalog) => break catalog,
            Err(error) => {
                assert!(
                    Instant::now() < catalog_deadline,
                    "query the real stemd application catalog: {error:?}"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
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

//! Drive the real Settings page through the test-only native touchscreen.
use scarlet_os::handle::Handle;
use std::time::{Duration, Instant};
use sws_client::Connection;
fn inject(events: &[(u16, u16, i32)]) {
    let handle = Handle::open("/dev/touch-qa-control", 1).unwrap();
    let mut bytes = Vec::new();
    for &(kind, code, value) in events {
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&code.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(
        handle.as_stream().unwrap().write(&bytes).unwrap(),
        bytes.len()
    );
}
fn swipe() {
    inject(&[
        (3, 0x2f, 0),
        (3, 0x39, 1),
        (3, 0x35, 1150),
        (3, 0x36, 650),
        (1, 0x14a, 1),
        (0, 0, 0),
    ]);
    for y in (150..650).rev().step_by(25) {
        inject(&[(3, 0x2f, 0), (3, 0x36, y), (0, 0, 0)]);
        std::thread::sleep(Duration::from_millis(35));
    }
    inject(&[(3, 0x2f, 0), (3, 0x39, -1), (1, 0x14a, 0), (0, 0, 0)]);
    std::thread::sleep(Duration::from_secs(1));
}
fn main() {
    let connection = Connection::connect_default().unwrap();
    // Keep this layout test independent of automatic tablet/focused policy.
    // The injected device still delivers real native touch events to Settings.
    connection.set_tablet_mode_override(Some(false)).unwrap();
    connection
        .set_windowing_mode_override(Some(sws_protocol::WindowingMode::Freeform))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let id = loop {
        if let Some(window) = connection
            .get_window_list()
            .unwrap()
            .into_iter()
            .find(|w| w.app_id == "org.scarlet-os.desktop.settings")
        {
            break window.window_id;
        }
        assert!(Instant::now() < deadline, "Settings did not open");
        std::thread::sleep(Duration::from_millis(100));
    };
    std::thread::sleep(Duration::from_secs(25));
    connection
        .send_message(
            sws_protocol::client_msg::MAXIMIZE_WINDOW,
            &sws_protocol::payload_maximize_window(id),
        )
        .unwrap();
    connection.focus_window_any(id).unwrap();
    let visible_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let windows = connection.get_window_list().unwrap();
        if windows
            .iter()
            .any(|w| w.window_id == id && w.visible && w.focused)
        {
            break;
        }
        assert!(
            Instant::now() < visible_deadline,
            "Settings did not present: {windows:?}"
        );
        connection.focus_window_any(id).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(5));
    println!("NETWORK_SCROLL_TOP_READY");
    // Allow the host to capture route selection before the first gesture.
    std::thread::sleep(Duration::from_secs(8));
    for _ in 0..4 {
        swipe();
    }
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        connection
            .get_window_list()
            .unwrap()
            .iter()
            .any(|w| w.window_id == id && w.visible)
    );
    println!("NETWORK_SCROLL_BOTTOM_READY");
}

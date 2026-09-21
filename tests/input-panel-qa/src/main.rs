//! Exercise the installed keyboard and IME through real SWS and synthetic device input.
use scarlet_os::handle::Handle;
use std::time::{Duration, Instant};
use sws_client::{Connection, Event, EventReceiver};
fn inject(path: &str, events: &[(u16, u16, i32)]) {
    let mut bytes = Vec::new();
    for &(kind, code, value) in events {
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&code.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let handle = Handle::open(path, 1).unwrap();
    assert_eq!(
        handle.as_stream().unwrap().write(&bytes).unwrap(),
        bytes.len()
    );
}
fn tap(x: i32, y: i32) {
    press(x, y, Duration::from_millis(80));
}
fn press(x: i32, y: i32, duration: Duration) {
    inject(
        "/dev/touch-qa-control",
        &[
            (3, 0x2f, 0),
            (3, 0x39, 1),
            (3, 0x35, x),
            (3, 0x36, y),
            (1, 0x14a, 1),
            (0, 0, 0),
        ],
    );
    std::thread::sleep(duration);
    inject(
        "/dev/touch-qa-control",
        &[(3, 0x2f, 0), (3, 0x39, -1), (1, 0x14a, 0), (0, 0, 0)],
    );
}
fn expect_key(connection: &Connection, receiver: &EventReceiver, code: u16, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed = Vec::new();
    loop {
        connection.dispatch().unwrap();
        let events = receiver.drain_events();
        observed.extend(events.iter().cloned());
        if events.iter().any(|event| match event {
            Event::Input(input) => input.type_ == 1 && input.code == code && input.value == 1,
            Event::TextInputCommit { text, .. } | Event::TextInputPreedit { text, .. } => {
                text.ends_with(label)
            }
            _ => false,
        }) {
            println!("INPUT_PANEL_HIT_PASS key={label} code={code}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "touching {label} delivered no matching text/key: {observed:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn expect_repeat(connection: &Connection, receiver: &EventReceiver, code: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut keys = 0;
    let mut preedits = Vec::new();
    loop {
        connection.dispatch().unwrap();
        for event in receiver.drain_events() {
            match event {
                Event::Input(input)
                    if input.type_ == 1 && input.code == code && input.value == 1 =>
                {
                    keys += 1
                }
                Event::TextInputPreedit { text, .. } => {
                    if preedits.last() != Some(&text) {
                        preedits.push(text);
                    }
                }
                _ => {}
            }
        }
        if keys >= 3 || preedits.len() >= 3 {
            println!("INPUT_PANEL_REPEAT_PASS code={code} keys={keys} preedits={preedits:?}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "held key did not repeat: code={code} keys={keys} preedits={preedits:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn main() {
    let connection = Connection::connect_default().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    // Home is asynchronous; don't let its first surface steal our editor's focus.
    let startup = connection
        .create_surface(
            "org.scarlet-os.input-panel-qa",
            "Panel startup",
            "",
            400,
            240,
        )
        .unwrap();
    connection.commit(startup).unwrap();
    inject("/dev/input-qa-control", &[(1, 0x13c, 1), (0, 0, 0)]);
    inject("/dev/input-qa-control", &[(1, 0x13c, 0), (0, 0, 0)]);
    loop {
        connection.dispatch().unwrap();
        if connection.drain_events().iter().any(|event|matches!(event,Event::FocusChanged{app_id,..} if app_id=="org.scarlet-os.desktop.shell.console-home")) {break;}
        assert!(Instant::now() < deadline, "Home startup timeout");
        std::thread::sleep(Duration::from_millis(20));
    }
    connection.destroy_surface(startup).unwrap();
    let id = connection
        .create_surface(
            "org.scarlet-os.input-panel-qa",
            "Keyboard input",
            "",
            1280,
            720,
        )
        .unwrap();
    connection
        .with_surface_mut(id, |surface| surface.fill(31, 35, 42, 255))
        .unwrap();
    connection.commit(id).unwrap();
    connection.focus_window(id).unwrap();
    let receiver = connection.subscribe_window_events(id);
    assert!(
        connection
            .get_window_list()
            .unwrap()
            .iter()
            .any(|window| window.window_id == id && window.focused)
    );
    let before = connection
        .get_active_input_method()
        .unwrap()
        .map(|method| method.name);
    assert!(before.is_some(), "production IME not registered");
    let (context, serial) = connection.create_text_input_context(id, 0).unwrap();
    connection
        .set_text_input_cursor_rect(context, 80, 80, 2, 20)
        .unwrap();
    connection.commit_text_input_state(context, serial).unwrap();
    connection.enable_text_input(context).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let area = loop {
        connection.dispatch().unwrap();
        let events = receiver.drain_events();
        if let Some(area) = events.iter().find_map(|event| match event {
            Event::InputPanelOcclusion(area) if area.height > 0 => Some(*area),
            _ => None,
        }) {
            break area;
        }
        assert!(Instant::now() < deadline, "input panel did not appear");
        std::thread::sleep(Duration::from_millis(20));
    };
    // TCG needs substantially longer than hardware to rasterize and commit the
    // first full keyboard frame. Keep the editor alive until that frame exists
    // so this tests the key bounds rather than touching an uncommitted surface.
    std::thread::sleep(Duration::from_secs(15));
    let key_unit = area.width as f32 / 14.78;
    let gap = key_unit * 0.17;
    let row_height = (area.height as f32 - 2.0 * gap - 4.0 * gap) / 4.73;
    let x = (gap + key_unit * 1.285 + gap + key_unit / 2.0) as i32;
    let y = (area.y as f32 + gap + row_height * 0.73 + gap + row_height / 2.0) as i32;
    println!("INPUT_PANEL_READY x={x} y={y} area={area:?}");
    std::thread::sleep(Duration::from_millis(500));
    tap(x, y);
    expect_key(&connection, &receiver, 16, "q");
    let home_y = (y as f32 + row_height + gap) as i32;
    let g_x =
        (area.x as f32 + gap + 1.70 * key_unit + gap + key_unit / 2.0 + 4.0 * (key_unit + gap))
            as i32;
    let h_x = (g_x as f32 + key_unit + gap) as i32;
    tap(g_x, home_y);
    expect_key(&connection, &receiver, 34, "g");
    tap(h_x, home_y);
    expect_key(&connection, &receiver, 35, "h");
    press(g_x, home_y, Duration::from_millis(1500));
    expect_repeat(&connection, &receiver, 34);
    // Settle the release before testing a second held key.
    std::thread::sleep(Duration::from_secs(1));
    connection.dispatch().unwrap();
    receiver.drain_events();
    let delete_x = (area.x as f32 + area.width as f32 - gap - 1.285 * key_unit / 2.0) as i32;
    press(delete_x, y, Duration::from_millis(1500));
    expect_repeat(&connection, &receiver, 14);
    assert!(
        connection
            .get_window_list()
            .unwrap()
            .iter()
            .any(|window| window.window_id == id && window.focused),
        "keyboard stole editor focus"
    );
    assert_eq!(
        connection
            .get_active_input_method()
            .unwrap()
            .map(|method| method.name),
        before,
        "panel replaced the selected IME"
    );
    // A normal application cannot claim an input panel through a foreign surface.
    let stranger = Connection::connect_default().unwrap();
    assert!(!stranger.register_input_panel(id).unwrap());
    // Disabling an editor hides the panel, while re-enabling it opens a fresh activation.
    connection.disable_text_input(context).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        connection.dispatch().unwrap();
        if receiver
            .drain_events()
            .iter()
            .any(|event| matches!(event,Event::InputPanelOcclusion(area) if area.height==0))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "disabled editor kept panel visible"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    connection.enable_text_input(context).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        connection.dispatch().unwrap();
        if receiver
            .drain_events()
            .iter()
            .any(|event| matches!(event,Event::InputPanelOcclusion(area) if area.height>0))
        {
            break;
        }
        assert!(Instant::now() < deadline, "panel failed to reopen");
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("INPUT_PANEL_QA_PASS touch_key editor_focus ime_preserved hide_reopen ownership");
    // Keep the real keyboard visible for the runner's screenshot.
    loop {
        connection.dispatch().unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }
}

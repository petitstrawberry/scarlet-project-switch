//! Exercises native EventDevice -> SWS -> sws-client -> ScarletUI in the guest.
use scarlet_os::handle::Handle;
use scarlet_ui::PlatformWindow;
use scarlet_ui::prelude::*;
use scarlet_ui_macros::View;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use sws_client::{Connection, EventReceiver};
use sws_protocol::gamepad::{RESET, State as GamepadState, button_bit};

fn inject(records: &[(u16, u16, i32)]) {
    let control = Handle::open("/dev/input-qa-control", 1).expect("open test-only input control");
    let mut bytes = Vec::new();
    for &(type_, code, value) in records {
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&type_.to_le_bytes());
        bytes.extend_from_slice(&code.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(
        control.as_stream().unwrap().write(&bytes).unwrap(),
        bytes.len()
    );
}
fn snapshot(pressed: bool) {
    let mut records = Vec::new();
    for code in 0x130..=0x13e {
        records.push((1, code, i32::from(pressed && code == 0x131)));
    }
    for (code, value) in [
        (0, if pressed { 4095 } else { 2048 }),
        (1, 2048),
        (2, if pressed { 255 } else { 0 }),
        (3, if pressed { 0 } else { 2048 }),
        (4, if pressed { 4095 } else { 2048 }),
        (5, 0),
        (0x10, 0),
        (0x11, 0),
    ] {
        records.push((3, code, value));
    }
    records.push((0, 0, 0));
    inject(&records);
}
fn collect(
    connection: &Connection,
    receiver: &EventReceiver,
    duration: Duration,
) -> Vec<sws_client::Event> {
    let deadline = Instant::now() + duration;
    let mut events = Vec::new();
    while Instant::now() < deadline {
        connection.dispatch().expect("dispatch real SWS messages");
        events.extend(receiver.drain_events());
        std::thread::sleep(Duration::from_millis(5));
    }
    events
}
fn collect_until(
    connection: &Connection,
    receiver: &EventReceiver,
    observed: impl Fn(&[sws_client::Event]) -> bool,
) -> Vec<sws_client::Event> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    loop {
        events.extend(collect(connection, receiver, Duration::from_millis(50)));
        if observed(&events) || Instant::now() >= deadline {
            return events;
        }
    }
}
fn is_held(state: &GamepadState) -> bool {
    state.device_id == 0
        && state.buttons == button_bit(0x131).unwrap()
        && state.left_x == 32767
        && state.right_x == -32767
        && state.right_y == 32767
        && state.left_trigger == 32767
        && state.flags == 0
}
fn has_gamepad(
    events: &[sws_client::Event],
    window: u32,
    accept: impl Fn(&GamepadState) -> bool,
) -> bool {
    events.iter().any(|event| match event {
        sws_client::Event::GamepadInput { surface_id, state } if *surface_id == window => {
            accept(state)
        }
        _ => false,
    })
}
fn has_key(events: &[sws_client::Event], code: u16, value: i32) -> bool {
    events.iter().any(|event| match event {
        sws_client::Event::Input(input) => {
            input.type_ == 1 && input.code == code && input.value == value
        }
        _ => false,
    })
}
fn surface(connection: &Connection, name: &str) -> u32 {
    let id = connection
        .create_surface("org.scarlet-os.input-qa", name, "", 400, 240)
        .unwrap();
    connection
        .with_surface_mut(id, |surface| surface.fill(20, 20, 20, 255))
        .unwrap();
    connection.commit(id).unwrap();
    connection.focus_window(id).unwrap();
    id
}
fn sws_checks() {
    // Let the normal console shell finish its asynchronous initial creation.
    std::thread::sleep(Duration::from_secs(3));
    let connection = Connection::connect_default().expect("connect to production SWS");
    assert!(
        connection
            .get_capabilities()
            .unwrap()
            .supports_gamepad_input()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !connection.get_input_environment().unwrap().has_gamepad() {
        assert!(
            Instant::now() < deadline,
            "native gamepad discovery timed out"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let id = surface(&connection, "Input QA menu");
    let receiver = connection.subscribe_window_events(id);
    connection.set_gamepad_input(id, true, true).unwrap();
    // SET_GAMEPAD_INPUT is asynchronous. This request is an IPC barrier before
    // injecting input, including when software rendering delays a frame.
    assert!(
        connection
            .get_window_list()
            .unwrap()
            .iter()
            .any(|window| { window.window_id == id && window.focused })
    );
    collect(&connection, &receiver, Duration::from_millis(300));
    snapshot(true);
    let events = collect_until(&connection, &receiver, |events| {
        has_gamepad(events, id, is_held) && [28, 106].iter().all(|&code| has_key(events, code, 1))
    });
    assert!(
        has_gamepad(&events, id, is_held),
        "normalized snapshot missing: {events:?}"
    );
    for code in [28, 106] {
        assert!(
            has_key(&events, code, 1),
            "menu key {code} missing: {events:?}"
        );
    }
    snapshot(false);
    let events = collect_until(&connection, &receiver, |events| {
        [28, 106].iter().all(|&code| has_key(events, code, 0))
    });
    for code in [28, 106] {
        assert!(has_key(&events, code, 0), "menu release missing");
    }
    inject(&[(1, 0x130, 1), (0, 0, 0)]);
    let events = collect_until(&connection, &receiver, |events| has_key(events, 1, 1));
    assert!(has_key(&events, 1, 1), "Nintendo B did not cancel");
    inject(&[(1, 0x130, 0), (0, 0, 0)]);
    let events = collect_until(&connection, &receiver, |events| has_key(events, 1, 0));
    assert!(has_key(&events, 1, 0), "cancel key did not release");
    connection.set_gamepad_input(id, true, false).unwrap();
    connection.get_window_list().unwrap();
    collect(&connection, &receiver, Duration::from_millis(300));
    snapshot(true);
    let events = collect_until(&connection, &receiver, |events| {
        has_gamepad(events, id, is_held)
    });
    assert!(has_gamepad(&events, id, is_held));
    assert!(
        ![28, 106].iter().any(|&code| has_key(&events, code, 1)),
        "raw mode invented navigation keys"
    );
    let second = surface(&connection, "Input QA focus");
    connection.set_gamepad_input(second, true, false).unwrap();
    connection.get_window_list().unwrap();
    let events = collect_until(&connection, &receiver, |events| {
        has_gamepad(events, id, |state| {
            state.flags == RESET && state.buttons == 0
        })
    });
    assert!(
        has_gamepad(&events, id, |state| state.flags == RESET
            && state.buttons == 0),
        "focus loss did not reset old consumer"
    );
    let second_receiver = connection.subscribe_window_events(second);
    snapshot(true);
    let events = collect_until(&connection, &second_receiver, |events| {
        has_gamepad(events, second, is_held)
    });
    assert!(
        has_gamepad(&events, second, is_held),
        "new focus did not receive native state: {events:?}"
    );
    inject(&[(0, 3, 0)]);
    snapshot(true);
    let events = collect_until(&connection, &second_receiver, |events| {
        has_gamepad(events, second, |state| state.flags == RESET)
    });
    assert!(
        has_gamepad(&events, second, |state| state.flags == RESET),
        "dropped input reset missing: {events:?}"
    );
    snapshot(true);
    let events = collect_until(&connection, &second_receiver, |events| {
        has_gamepad(events, second, is_held)
    });
    assert!(
        has_gamepad(&events, second, is_held),
        "drop recovery did not accept next authoritative snapshot"
    );
    snapshot(false);
    collect(&connection, &second_receiver, Duration::from_millis(300));
    connection.destroy_surface(second).unwrap();
    connection.destroy_surface(id).unwrap();
    collect(&connection, &receiver, Duration::from_millis(300));
    println!("INPUT_GAMEPAD_SWS_PASS navigation raw_mode focus_reset drop_recovery");
}

#[derive(View, Clone)]
struct UiInputApp {
    ready: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<GamepadEvent>>>,
}
impl Application for UiInputApp {
    fn scenes(&self) -> impl Scene {
        WindowGroup::new(
            "input-qa",
            Window::new("ScarletUI input QA", Text::new("Native gamepad event QA"))
                .app_id("org.scarlet-os.scarletui.input-qa")
                .size(Size::new(400.0, 240.0)),
        )
    }
    fn on_window_created(&mut self, _ctx: &WindowContext, window: &mut dyn PlatformWindow) {
        window.set_gamepad_input(true, false).unwrap();
        self.ready.store(true, Ordering::Release);
    }
    fn on_gamepad(&mut self, ctx: &WindowContext, event: GamepadEvent) {
        let mut events = self.events.lock().unwrap();
        let held = events.iter().any(|e| e.pressed(GamepadButton::East));
        events.push(event);
        if held && !event.reset && event.buttons == 0 {
            scarlet_ui::dismiss_window(ctx.scene_key.clone());
        }
    }
}
fn ui_checks() {
    let ready = Arc::new(AtomicBool::new(false));
    let events = Arc::new(Mutex::new(Vec::new()));
    let input_ready = ready.clone();
    let producer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !input_ready.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        snapshot(true);
        std::thread::sleep(Duration::from_millis(300));
        snapshot(false);
    });
    UiInputApp {
        ready,
        events: events.clone(),
    }
    .run()
    .unwrap();
    producer.join().unwrap();
    let events = events.lock().unwrap();
    assert!(
        events.iter().any(|e| e.pressed(GamepadButton::East)
            && e.left_x == 32767
            && e.right_x == -32767
            && e.right_y == 32767
            && e.left_trigger == 32767),
        "ScarletUI lost native input: {events:?}"
    );
    assert!(events.iter().any(|e| !e.reset && e.buttons == 0));
    println!("INPUT_SCARLET_UI_PASS native_callback button_identity axes release");
}

fn press_button(code: u16) {
    inject(&[(1, code, 1), (0, 0, 0)]);
    std::thread::sleep(Duration::from_millis(120));
    inject(&[(1, code, 0), (0, 0, 0)]);
    std::thread::sleep(Duration::from_millis(120));
}

fn wait_presentation(
    connection: &Connection,
    expected: sws_protocol::workspace::ShellPresentation,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = connection.get_workspace_state().unwrap();
        if state.presentation == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "console presentation did not become {expected:?}: {state:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn console_shell_checks() {
    use sws_protocol::workspace::ShellPresentation;

    let connection = Connection::connect_default().unwrap();
    snapshot(false);
    press_button(0x13c); // HOME uses the existing system Home action.
    wait_presentation(&connection, ShellPresentation::Home);
    std::thread::sleep(Duration::from_millis(500));

    // The real console catalog starts with Clock, then Files. A directional
    // press must change the actual shell selection before Nintendo A launches.
    inject(&[(3, 0x10, 1), (0, 0, 0)]);
    std::thread::sleep(Duration::from_millis(120));
    inject(&[(3, 0x10, 0), (0, 0, 0)]);
    std::thread::sleep(Duration::from_millis(120));
    press_button(0x131);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let windows = connection.get_window_list().unwrap();
        if windows
            .iter()
            .any(|window| window.app_id == "org.scarlet-os.desktop.files" && window.focused)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "direction/A did not launch Files through console Home: {windows:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    wait_presentation(&connection, ShellPresentation::Workspace);
    press_button(0x13c);
    wait_presentation(&connection, ShellPresentation::Home);
    press_button(0x130);
    wait_presentation(&connection, ShellPresentation::Workspace);
    println!("INPUT_CONSOLE_SHELL_PASS direction A_launch_files HOME B_return");
}

fn main() {
    sws_checks();
    ui_checks();
    console_shell_checks();
    println!("INPUT_QA_PASS");
}

//! Run in a handheld Switch desktop session with automatic posture and DSI experience.

use sws_client::Connection;
use sws_protocol::outputs::OutputExperience;

fn main() {
    let connection = Connection::connect_default().expect("connect to SWS");
    let snapshot = connection.outputs().expect("read display topology");
    let mut panels = snapshot.outputs.iter().filter(|o| o.name == "Tegra210 DSI");
    let panel = panels.next().expect("missing Tegra210 DSI output");
    assert!(panels.next().is_none(), "duplicate Tegra210 DSI output");
    assert!(
        panel.internal,
        "built-in DSI panel must identify as internal"
    );
    assert_eq!(panel.connector_key, "platform:54200000/connector-0");
    assert_eq!(
        (panel.mode.pixel_width, panel.mode.pixel_height),
        (1280, 720)
    );

    let config = snapshot
        .configuration
        .iter()
        .find(|c| c.id == panel.id)
        .expect("missing DSI output configuration");
    assert_eq!(config.experience, OutputExperience::Auto);
    let input = connection.get_input_environment().expect("read posture");
    assert_eq!(
        input.tablet_mode(),
        Some(true),
        "expected handheld tablet posture"
    );
    assert_eq!(
        input.tablet_mode_override_active(),
        Some(false),
        "expected posture from the hardware switch"
    );
    let context = connection
        .output_context(panel.id)
        .expect("read DSI output context");
    assert!(
        context.tablet,
        "Auto must resolve to tablet on the built-in panel"
    );
    println!(
        "PASS {} internal=true experience=Auto tablet=true",
        panel.connector_key
    );
}

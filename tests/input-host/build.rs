use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=SCARLET_SOURCE");
    println!("cargo:rerun-if-env-changed=SCARLET_UI_SOURCE");
    let scarlet = PathBuf::from(
        env::var("SCARLET_SOURCE")
            .expect("set SCARLET_SOURCE and SCARLET_UI_SOURCE to the checkouts under test"),
    );
    let ui = PathBuf::from(
        env::var("SCARLET_UI_SOURCE")
            .expect("set SCARLET_SOURCE and SCARLET_UI_SOURCE to the checkouts under test"),
    );
    let mut modules = String::new();
    for (name, path) in [
        ("gamepad", scarlet.join("user/std-bin/src/sws/gamepad.rs")),
        (
            "input_panel",
            scarlet.join("user/std-bin/src/sws/input_panel.rs"),
        ),
        (
            "key_repeat",
            scarlet.join("user/std-bin/src/sws/key_repeat.rs"),
        ),
        (
            "volume_policy",
            scarlet.join("user/std-bin/src/scarlet_shell/volume_policy.rs"),
        ),
        (
            "ui_gamepad",
            ui.join("crates/scarlet-ui-core/src/event/gamepad.rs"),
        ),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
        modules.push_str(&format!("#[path = {:?}]\nmod {};\n", path, name));
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("input_modules.rs"),
        modules,
    )
    .expect("write input test module paths");
}

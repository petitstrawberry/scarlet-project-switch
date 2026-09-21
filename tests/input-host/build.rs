use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=SCARLET_SOURCE");
    println!("cargo:rerun-if-env-changed=SCARLET_UI_SOURCE");
    let scarlet = PathBuf::from(
        env::var("SCARLET_SOURCE")
            .expect("run python3 scripts/project_sources.py before the input tests"),
    );
    let ui = PathBuf::from(
        env::var("SCARLET_UI_SOURCE")
            .expect("run python3 scripts/project_sources.py before the input tests"),
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

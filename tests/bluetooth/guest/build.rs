// The runner resolves the same Scarlet checkout for the kernel and SWS.
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=SCARLET_SOURCE");
    let source = PathBuf::from(env::var_os("SCARLET_SOURCE").expect("use tests/bluetooth/run.py"));
    let policy = source.join("user/std-bin/src/sws/gamepad.rs");
    println!("cargo:rerun-if-changed={}", policy.display());
    let module = format!("#[allow(dead_code)]\n#[path = {policy:?}]\nmod gamepad;\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("navigation.rs"),
        module,
    )
    .unwrap();
}

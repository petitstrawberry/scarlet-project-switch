// SPDX-License-Identifier: MIT
use sgfx_core::ir::{ShaderModuleDesc, ShaderStage};
use sgfx_nak::api::{DEBUG, GetDebugFlags};
use sgfx_shader_maxwell::{compile_shader, compile_shader_for_driver};

#[test]
fn public_compile_retains_assembly_and_matches_driver_binary() {
    let desc = ShaderModuleDesc::wgsl(
        "@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }".into(),
    )
    .unwrap();
    let public = compile_shader(&desc, ShaderStage::Fragment, "main").unwrap();
    let driver = compile_shader_for_driver(&desc, ShaderStage::Fragment, "main").unwrap();
    assert!(public.assembly.contains("exit"));
    if DEBUG.print() {
        assert_eq!(driver.assembly, public.assembly);
    } else {
        assert!(driver.assembly.is_empty());
    }
    assert_eq!(driver.code, public.code);
    assert_eq!(driver.header, public.header);
}

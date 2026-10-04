// SPDX-License-Identifier: MIT
//! Genuine graphics programs for the driver's canonical R8 storage convention.

use crate::{CompiledShader, ShaderCompileError, compile_shader};
use sgfx_core::ir::{ShaderModuleDesc, ShaderStage};

const FULLSCREEN_VERTEX: &str = r#"
@vertex fn main(@builtin(vertex_index) index:u32)->@builtin(position) vec4<f32>{
    var p=vec2<f32>(-1.0,-1.0);
    if(index==1u){p=vec2<f32>(3.0,-1.0);}
    if(index==2u){p=vec2<f32>(-1.0,3.0);}
    return vec4<f32>(p,0.5,1.0);
}
"#;

fn compile_pair(fragment: String) -> Result<(CompiledShader, CompiledShader), ShaderCompileError> {
    let vertex = ShaderModuleDesc::wgsl(FULLSCREEN_VERTEX.into())
        .map_err(|e| ShaderCompileError(format!("conversion vertex: {e:?}")))?;
    let fragment = ShaderModuleDesc::wgsl(fragment)
        .map_err(|e| ShaderCompileError(format!("conversion fragment: {e:?}")))?;
    Ok((
        compile_shader(&vertex, ShaderStage::Vertex, "main")?,
        compile_shader(&fragment, ShaderStage::Fragment, "main")?,
    ))
}

/// Compile a full-size R8 storage conversion, with one texture load per pixel.
///
/// Bind group 0 / binding 0 must expose the source with an identity BGRA TIC:
/// its sampled `.a` is the physical alpha byte, not SGFX's logical default alpha.
/// A three-vertex, first-vertex-zero draw needs no vertex buffers. The viewport
/// starts at (0,0) and source/destination dimensions match. Disable blending and
/// write RGBA. The fragment's reflected sRGB flag constant must be zero.
///
/// `to_canonical=false` copies canonical physical A into logical scratch R and
/// initializes scratch A to one. `true` copies logical scratch R into canonical
/// physical A and initializes canonical RGB to zero. This preserves the true
/// user shader alpha during R8 blending on the logical scratch render target.
pub fn compile_r8_conversion(
    to_canonical: bool,
) -> Result<(CompiledShader, CompiledShader), ShaderCompileError> {
    let result = if to_canonical {
        "vec4<f32>(0.0,0.0,0.0,sampled.r)"
    } else {
        "vec4<f32>(sampled.a,0.0,0.0,1.0)"
    };
    compile_pair(format!(
        r#"
@group(0) @binding(0) var source:texture_2d<f32>;
@fragment fn main(@builtin(position) position:vec4<f32>)->@location(0) vec4<f32>{{
    let sampled=textureLoad(source,vec2<i32>(position.xy),0);
    return {result};
}}
"#
    ))
}

/// Compile a filtered/scaled/flipped canonical R8 blit to logical RGBA, or its
/// inverse. Source uses raw identity BGRA at group 0 / binding 0; sampler is at
/// binding 1; a 32-byte uniform at binding 2 stores two `vec4<f32>` values.
///
/// Uniform `destination=(x,y,width,height)` describes the viewport in pixels.
/// `source_rect=(u0,v0,du,dv)` describes normalized source coordinates; negative
/// `du`/`dv` flips an axis. The shader maps fragment pixel centers through that
/// affine transform and samples mip level zero. Bind a single selected mip/layer
/// view so this explicit level denotes the requested subresource. The sampler
/// controls nearest/linear filtering. Disable blending, write RGBA, zero sRGB
/// flags. Outputs use the same channel mapping as [`compile_r8_conversion`].
pub fn compile_r8_blit_conversion(
    to_canonical: bool,
) -> Result<(CompiledShader, CompiledShader), ShaderCompileError> {
    let result = if to_canonical {
        "vec4<f32>(0.0,0.0,0.0,sampled.r)"
    } else {
        "vec4<f32>(sampled.a,0.0,0.0,1.0)"
    };
    compile_pair(format!(
        r#"
struct Mapping {{ destination:vec4<f32>, source_rect:vec4<f32> }}
@group(0) @binding(0) var source:texture_2d<f32>;
@group(0) @binding(1) var source_sampler:sampler;
@group(0) @binding(2) var<uniform> mapping:Mapping;
@fragment fn main(@builtin(position) position:vec4<f32>)->@location(0) vec4<f32>{{
    let unit=(position.xy-mapping.destination.xy)/mapping.destination.zw;
    let uv=mapping.source_rect.xy+unit*mapping.source_rect.zw;
    let sampled=textureSampleLevel(source,source_sampler,uv,0.0);
    return {result};
}}
"#
    ))
}

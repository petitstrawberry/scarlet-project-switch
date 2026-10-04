// SPDX-License-Identifier: MIT
//! Runtime WGSL/SPIR-V graphics compilation for the Tegra X1's SM52 GPU.
//!
//! The validated, bounded Naga frontend is shared with VirGL. Its semantic TGSI
//! instructions are lowered to Mesa NAK IR and then allocated, scheduled, and
//! encoded as genuine Maxwell machine code. No source-name substitutions occur.

mod alu;
mod conversion;
mod lower;
mod texture;
mod tgsi;

#[cfg(test)]
mod tests;

pub use conversion::{compile_r8_blit_conversion, compile_r8_conversion};
pub use sgfx_codegen_virgl::programmable::{
    ImageQueryLevelsBinding, IoLocation, IoScalar, PushConstantBinding, StorageBufferBinding,
    TextureSamplerBinding, UniformBufferBinding,
};
use sgfx_core::ir::{ShaderModuleDesc, ShaderStage};
use std::{fmt, ops::Deref};

/// A parse, validation, unsupported-semantics, or bounded-resource failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderCompileError(pub String);
impl fmt::Display for ShaderCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ShaderCompileError {}

/// Machine code and all descriptor/stage-linkage reflection used by the driver.
#[derive(Debug)]
pub struct CompiledShader {
    pub reflection: sgfx_codegen_virgl::programmable::CompiledShader,
    /// Maxwell scheduler bundles, each eight 32-bit words (32 bytes).
    pub code: Vec<u32>,
    pub header: [u32; 20],
    pub metadata: sgfx_nak::ShaderMetadata,
    /// Final allocated NAK assembly, retained for diagnostics and compiler tests.
    pub assembly: String,
}
impl Deref for CompiledShader {
    type Target = sgfx_codegen_virgl::programmable::CompiledShader;
    fn deref(&self) -> &Self::Target {
        &self.reflection
    }
}

pub fn validate_shader_module(desc: &ShaderModuleDesc) -> Result<(), ShaderCompileError> {
    sgfx_codegen_virgl::programmable::validate_shader_module(desc)
        .map_err(|error| ShaderCompileError(error.0))
}

/// Compile an arbitrary supported graphics entry point with the same semantic
/// validation and resource reflection as the VirGL implementation.
pub fn compile_shader(
    desc: &ShaderModuleDesc,
    stage: ShaderStage,
    entry_point: &str,
) -> Result<CompiledShader, ShaderCompileError> {
    let reflection = sgfx_codegen_virgl::programmable::compile_shader(desc, stage, entry_point)
        .map_err(|error| ShaderCompileError(error.0))?;
    let program = tgsi::parse(&reflection.tgsi).map_err(ShaderCompileError)?;
    let sm = sgfx_nak::ir::ShaderModelInfo::new(52, 64);
    let shader = lower::lower(&sm, &program).map_err(ShaderCompileError)?;
    let compiled = sgfx_nak::compile_graphics_ir(shader)
        .map_err(|error| ShaderCompileError(error.to_string()))?;
    if compiled.code.len() > 16 * 1024 {
        return Err(ShaderCompileError(
            "Maxwell shader exceeds 64 KiB of scheduled machine code".into(),
        ));
    }
    if compiled.metadata.scratch_bytes != 0 || compiled.metadata.crs_bytes != 0 {
        return Err(ShaderCompileError(
            "Maxwell shader requires unsupported local-memory spilling".into(),
        ));
    }
    Ok(CompiledShader {
        reflection,
        code: compiled.code,
        header: compiled.header,
        metadata: compiled.metadata,
        assembly: compiled.assembly,
    })
}

//! Pure chunk planning shared by synchronous execution and tracked dispatch.

use crate::{IrSubmitError, UnsupportedIrFeature, ir};
use alloc::vec::Vec;
use sgfx_codegen_maxwell as codegen;

pub(crate) const MAX_TEXTURED_DRAWS_PER_SUBMIT: usize = 512;
#[derive(Default)]
struct RenderReplayState {
    pipeline: Option<codegen::PipelineId>,
    vertex: Option<(codegen::ObjectId, u64)>,
    index: Option<(codegen::ObjectId, u64, ir::IndexFormat)>,
    texture: Option<codegen::ObjectId>,
    sampler: Option<ir::SamplerDesc>,
    uniforms: Option<ir::DrawUniforms>,
    scissor: Option<ir::PixelRect>,
}

impl RenderReplayState {
    fn append<'data>(&self, operations: &mut Vec<codegen::Operation<'data>>) {
        if let Some(pipeline) = self.pipeline {
            operations.push(codegen::Operation::SetPipeline(pipeline));
        }
        if let Some((buffer, offset)) = self.vertex {
            operations.push(codegen::Operation::SetVertexBuffer { buffer, offset });
        }
        if let Some((buffer, offset, format)) = self.index {
            operations.push(codegen::Operation::SetIndexBuffer {
                buffer,
                offset,
                format,
            });
        }
        if let Some(texture) = self.texture {
            operations.push(codegen::Operation::SetTexture(texture));
        }
        if let Some(sampler) = self.sampler {
            operations.push(codegen::Operation::SetSampler(sampler));
        }
        if let Some(uniforms) = self.uniforms {
            operations.push(codegen::Operation::SetUniforms(uniforms));
        }
        if let Some(scissor) = self.scissor {
            operations.push(codegen::Operation::SetScissor(Some(scissor)));
        }
    }
}

pub(crate) fn split_submission_operations<'data>(
    source: &[codegen::Operation<'data>],
    max_draws: usize,
) -> Result<Vec<Vec<codegen::Operation<'data>>>, IrSubmitError> {
    if max_draws == 0 {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ResourceState,
        ));
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut pass = None;
    let mut replay = RenderReplayState::default();
    let mut chunk_draws = 0usize;

    for operation in source {
        match operation {
            codegen::Operation::BeginRenderPass(descriptor) => {
                if chunk_draws >= max_draws && !current.is_empty() {
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(core::mem::take(&mut current));
                    chunk_draws = 0;
                }
                pass = Some(*descriptor);
                replay = RenderReplayState::default();
                current.push(operation.clone());
            }
            codegen::Operation::EndRenderPass => {
                current.push(codegen::Operation::EndRenderPass);
                pass = None;
                replay = RenderReplayState::default();
            }
            codegen::Operation::SetPipeline(pipeline) => {
                replay.pipeline = Some(*pipeline);
                current.push(operation.clone());
            }
            codegen::Operation::SetVertexBuffer { buffer, offset } => {
                replay.vertex = Some((*buffer, *offset));
                current.push(operation.clone());
            }
            codegen::Operation::SetIndexBuffer {
                buffer,
                offset,
                format,
            } => {
                replay.index = Some((*buffer, *offset, *format));
                current.push(operation.clone());
            }
            codegen::Operation::SetTexture(texture) => {
                replay.texture = Some(*texture);
                current.push(operation.clone());
            }
            codegen::Operation::SetSampler(sampler) => {
                replay.sampler = Some(*sampler);
                current.push(operation.clone());
            }
            codegen::Operation::SetUniforms(uniforms) => {
                replay.uniforms = Some(*uniforms);
                current.push(operation.clone());
            }
            codegen::Operation::SetScissor(scissor) => {
                replay.scissor = *scissor;
                current.push(operation.clone());
            }
            codegen::Operation::Draw { .. } | codegen::Operation::DrawIndexed { .. } => {
                let draw_limit = if replay.texture.is_some() {
                    max_draws.min(MAX_TEXTURED_DRAWS_PER_SUBMIT)
                } else {
                    max_draws
                };
                if chunk_draws >= draw_limit {
                    let mut continuation = pass.ok_or(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ResourceState,
                    ))?;
                    force_active_pass_store(&mut current)?;
                    current.push(codegen::Operation::EndRenderPass);
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(core::mem::take(&mut current));
                    continuation.load = ir::LoadOp::Load;
                    if let Some(depth) = continuation.depth.as_mut() {
                        depth.load = ir::DepthLoadOp::Load;
                    }
                    current.push(codegen::Operation::BeginRenderPass(continuation));
                    replay.append(&mut current);
                    chunk_draws = 0;
                }
                current.push(operation.clone());
                chunk_draws = chunk_draws
                    .checked_add(1)
                    .ok_or(IrSubmitError::OutOfMemory)?;
            }
            _ => current.push(operation.clone()),
        }
    }

    if !current.is_empty() {
        chunks
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        chunks.push(current);
    }
    Ok(chunks)
}

fn force_active_pass_store(operations: &mut [codegen::Operation<'_>]) -> Result<(), IrSubmitError> {
    for operation in operations.iter_mut().rev() {
        if let codegen::Operation::BeginRenderPass(pass) = operation {
            pass.store = ir::StoreOp::Store;
            if let Some(depth) = pass.depth.as_mut() {
                depth.store = ir::StoreOp::Store;
            }
            return Ok(());
        }
    }
    Err(IrSubmitError::Unsupported(
        UnsupportedIrFeature::ResourceState,
    ))
}

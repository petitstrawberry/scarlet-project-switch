//! Pure chunk planning shared by synchronous execution and tracked dispatch.

use crate::wire::BoundObject;
use crate::{IrSubmitError, UnsupportedIrFeature, ir, wire};
use alloc::vec::Vec;
use sgfx_codegen_maxwell as codegen;

pub(crate) const MAX_TEXTURED_DRAWS_PER_SUBMIT: usize = 512;
pub(crate) const UPLOAD_ARENA_BYTES: u64 = 8 * 1024 * 1024;
const UPLOAD_CHUNK_BYTES: usize = 256 * 1024;

pub(crate) struct PreparedChunk {
    pub(crate) compiled: codegen::RelocatableCommands,
    pub(crate) placements: Vec<(codegen::GeneratedObjectId, u64, u64)>,
}

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

fn generated_placements(
    compiled: &codegen::RelocatableCommands,
    mut end: u64,
) -> Result<(Vec<(codegen::GeneratedObjectId, u64, u64)>, u64), IrSubmitError> {
    let mut placements = Vec::new();
    placements
        .try_reserve_exact(compiled.generated_objects.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for generated in &compiled.generated_objects {
        let alignment = u64::from(generated.alignment);
        if generated.bytes.is_empty() || !alignment.is_power_of_two() {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ));
        }
        end = end
            .checked_add(alignment - 1)
            .map(|value| value & !(alignment - 1))
            .ok_or(IrSubmitError::SubmissionTooLarge)?;
        let size =
            u64::try_from(generated.bytes.len()).map_err(|_| IrSubmitError::SubmissionTooLarge)?;
        placements.push((generated.id, end, size));
        end = end
            .checked_add(size)
            .filter(|end| *end <= UPLOAD_ARENA_BYTES)
            .ok_or(IrSubmitError::SubmissionTooLarge)?;
    }
    Ok((placements, end))
}

/// Outside-pass transfers are independently bounded packets; render-pass
/// splitting preserves load/store and replay state through the shared helper.
pub(crate) fn split_async_operations<'data>(
    source: &[codegen::Operation<'data>],
) -> Result<Vec<Vec<codegen::Operation<'data>>>, IrSubmitError> {
    let mut chunks = Vec::new();
    let mut graphics = Vec::new();
    for operation in source {
        if matches!(
            operation,
            codegen::Operation::WriteBuffer { .. }
                | codegen::Operation::WriteTexture { .. }
                | codegen::Operation::CopyTextureToTexture { .. }
        ) {
            let preceding = split_submission_operations(&graphics, 512)?;
            chunks
                .try_reserve(preceding.len())
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            chunks.extend(preceding);
            graphics.clear();
            match operation {
                codegen::Operation::WriteBuffer {
                    destination,
                    offset,
                    data,
                } => {
                    for (index, data) in data.chunks(UPLOAD_CHUNK_BYTES).enumerate() {
                        let offset = offset
                            .checked_add((index * UPLOAD_CHUNK_BYTES) as u64)
                            .ok_or(IrSubmitError::SubmissionTooLarge)?;
                        chunks
                            .try_reserve(1)
                            .map_err(|_| IrSubmitError::OutOfMemory)?;
                        chunks.push(alloc::vec![codegen::Operation::WriteBuffer {
                            destination: *destination,
                            offset,
                            data
                        }]);
                    }
                }
                codegen::Operation::WriteTexture {
                    destination,
                    area,
                    bytes_per_row,
                    data,
                } => {
                    let stride = usize::try_from(*bytes_per_row)
                        .map_err(|_| IrSubmitError::SubmissionTooLarge)?;
                    if stride == 0 {
                        return Err(IrSubmitError::Unsupported(
                            UnsupportedIrFeature::TextureUpload,
                        ));
                    }
                    // BGRA conversion can expand an R8 source by four. Bound
                    // both source bytes and physical rows before compilation.
                    let physical_row = area.width() as usize * 4;
                    let row_budget = stride.max(physical_row);
                    // Every row contributes an upload object and destination
                    // authority. Narrow/tall images must respect the wire's
                    // resource ceiling even when their pixel bytes are tiny.
                    let resource_rows = maxwell_submit_wire::MAX_RESOURCES / 2;
                    let max_rows =
                        (UPLOAD_CHUNK_BYTES / row_budget).max(1).min(resource_rows) as u32;
                    let mut row = 0;
                    while row < area.height() {
                        let height = max_rows.min(area.height() - row);
                        let first = row as usize * stride;
                        let end = ((row + height) as usize * stride).min(data.len());
                        let pixels = data.get(first..end).ok_or(IrSubmitError::Unsupported(
                            UnsupportedIrFeature::TextureUpload,
                        ))?;
                        let part =
                            ir::PixelRect::new(area.x(), area.y() + row, area.width(), height)?;
                        chunks
                            .try_reserve(1)
                            .map_err(|_| IrSubmitError::OutOfMemory)?;
                        chunks.push(alloc::vec![codegen::Operation::WriteTexture {
                            destination: *destination,
                            area: part,
                            bytes_per_row: *bytes_per_row,
                            data: pixels
                        }]);
                        row += height;
                    }
                }
                _ => {
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(alloc::vec![operation.clone()]);
                }
            }
        } else {
            graphics
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            graphics.push(operation.clone());
        }
    }
    let trailing = split_submission_operations(&graphics, 512)?;
    chunks
        .try_reserve(trailing.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    chunks.extend(trailing);
    Ok(chunks)
}

pub(crate) fn prepare_chunk(
    capabilities: codegen::Capabilities,
    resources: &[codegen::ResourceMeta],
    pipelines: &[codegen::PipelineMeta],
    operations: &[codegen::Operation<'_>],
    external: &[BoundObject],
    arena_end: u64,
    native_limit: usize,
) -> Result<(PreparedChunk, u64), IrSubmitError> {
    let compiled = codegen::compile(codegen::CompileInput {
        capabilities,
        resources,
        pipelines,
        operations,
    })?;
    let (placements, end) = generated_placements(&compiled, arena_end)?;
    let mut bindings = external.to_vec();
    bindings
        .try_reserve_exact(placements.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for &(id, offset, size) in &placements {
        bindings.push(BoundObject {
            object: codegen::ObjectRef::Generated(id),
            attachment_token: 1,
            allocation_offset: offset,
            size,
        });
    }
    match wire::encode(&compiled, &bindings) {
        Ok(payload) if payload.len() <= native_limit => {}
        Ok(_) | Err(IrSubmitError::SubmitWire(maxwell_submit_wire::Error::InvalidSize)) => {
            return Err(IrSubmitError::SubmissionTooLarge);
        }
        Err(error) => return Err(error),
    }
    Ok((
        PreparedChunk {
            compiled,
            placements,
        },
        end,
    ))
}

//! Pure chunk planning shared by synchronous execution and tracked dispatch.

use crate::{IrSubmitError, UnsupportedIrFeature, ir};
use alloc::vec::Vec;
use sgfx_codegen_maxwell as codegen;

#[cfg(test)]
mod viewport_tests {
    use super::*;

    #[test]
    fn viewport_and_changes_survive_split_pass_replay_with_two_records_per_draw() {
        let first = ir::Viewport::new(1.25, 31.0, 15.5, -20.0, 0.75, 0.25).unwrap();
        let second = ir::Viewport::new(2.0, 3.0, 16.0, 16.0, 0.0, 1.0).unwrap();
        let pass = codegen::RenderPass {
            target: codegen::ObjectId::new(0),
            area: ir::PixelRect::new(0, 0, 32, 32).unwrap(),
            load: ir::LoadOp::Clear(ir::Color::rgba(0., 0., 0., 1.).unwrap()),
            store: ir::StoreOp::DontCare,
            depth: None,
        };
        let draw = || codegen::Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        };
        let source = vec![
            codegen::Operation::BeginRenderPass(pass),
            codegen::Operation::SetViewport(first),
            draw(),
            draw(),
            codegen::Operation::SetViewport(second),
            draw(),
            codegen::Operation::EndRenderPass,
        ];
        let chunks = split_submission_operations(&source, 2, &[]).unwrap();
        assert_eq!(chunks.len(), 3);
        let mut observed = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let codegen::Operation::BeginRenderPass(p) = chunk[0] else {
                panic!("chunk must begin a pass")
            };
            assert_eq!(p.target, pass.target);
            if index == 0 {
                assert_eq!(p.load, pass.load);
            } else {
                assert_eq!(p.load, ir::LoadOp::Load);
            }
            assert_eq!(
                p.store,
                if index < 2 {
                    ir::StoreOp::Store
                } else {
                    ir::StoreOp::DontCare
                }
            );
            let mut viewport = None;
            let mut draws = 0;
            for operation in chunk {
                match operation {
                    codegen::Operation::SetViewport(value) => viewport = Some(*value),
                    codegen::Operation::Draw { .. } => {
                        observed.push(viewport.unwrap());
                        draws += 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(draws, 1);
            assert_eq!(chunk.last(), Some(&codegen::Operation::EndRenderPass));
        }
        assert_eq!(observed, [first, first, second]);
        let defaults = vec![
            codegen::Operation::BeginRenderPass(pass),
            draw(),
            draw(),
            draw(),
            codegen::Operation::EndRenderPass,
        ];
        assert_eq!(
            split_submission_operations(&defaults, 2, &[])
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn blend_mask_and_viewport_record_costs_survive_replay_and_actual_compilation() {
        let descriptor = |blend| {
            ir::RenderPipelineDesc::new(
                ir::TextureFormat::Bgra8Unorm,
                ir::PrimitiveTopology::TriangleList,
                ir::VertexBufferLayout::new(
                    16,
                    vec![
                        ir::VertexAttribute::new(0, ir::VertexFormat::Float32x2, 0),
                        ir::VertexAttribute::new(1, ir::VertexFormat::Float32x2, 8),
                    ],
                )
                .unwrap(),
                ir::FragmentProgram::Solid,
                blend,
                ir::RasterState::new(ir::CullMode::None, ir::FrontFace::CounterClockwise),
            )
            .unwrap()
        };
        let pipelines = [
            codegen::PipelineMeta {
                id: codegen::PipelineId::new(0),
                descriptor: descriptor(ir::BlendState::DESTINATION_IN),
            },
            codegen::PipelineMeta {
                id: codegen::PipelineId::new(1),
                descriptor: descriptor(ir::BlendState::REPLACE),
            },
        ];
        let resources = [
            codegen::ResourceMeta {
                id: codegen::ObjectId::new(0),
                size: 4096,
                kind: codegen::ResourceKind::Image(codegen::ImageMeta {
                    subresources: None,
                    format: ir::TextureFormat::Bgra8Unorm,
                    storage_format: ir::TextureFormat::Bgra8Unorm,
                    extent: ir::Extent2D::new(32, 32).unwrap(),
                    usage: ir::TextureUsage::RENDER_ATTACHMENT,
                    modifier: codegen::ImageModifier::Linear,
                    planes: vec![codegen::PlaneLayout {
                        offset: 0,
                        stride: 128,
                        size: 4096,
                    }],
                }),
            },
            codegen::ResourceMeta {
                id: codegen::ObjectId::new(1),
                size: 48,
                kind: codegen::ResourceKind::Buffer {
                    usage: ir::BufferUsage::VERTEX,
                },
            },
        ];
        let draw = || codegen::Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        };
        let viewport = ir::Viewport::new(0., 0., 16., 16., 0., 1.).unwrap();
        let source = vec![
            codegen::Operation::BeginRenderPass(codegen::RenderPass {
                target: resources[0].id,
                area: ir::PixelRect::new(0, 0, 32, 32).unwrap(),
                load: ir::LoadOp::Load,
                store: ir::StoreOp::Store,
                depth: None,
            }),
            codegen::Operation::SetPipeline(pipelines[0].id),
            codegen::Operation::SetVertexBuffer {
                buffer: resources[1].id,
                offset: 0,
            },
            codegen::Operation::SetUniforms(ir::DrawUniforms::new(
                ir::Transform::identity(),
                ir::Color::rgba(1., 1., 1., 1.).unwrap(),
            )),
            codegen::Operation::SetViewport(viewport),
            draw(),
            codegen::Operation::SetColorWriteMask(1),
            draw(),
            codegen::Operation::SetColorWriteMask(15),
            draw(),
            codegen::Operation::SetPipeline(pipelines[1].id),
            draw(),
            codegen::Operation::EndRenderPass,
        ];
        let chunks = split_submission_operations(&source, 6, &pipelines).unwrap();
        assert_eq!(chunks.len(), 2);
        let artifacts: Vec<_> = chunks
            .iter()
            .map(|operations| {
                codegen::compile(codegen::CompileInput {
                    capabilities: codegen::Capabilities::gm20b(6 * 64),
                    resources: &resources,
                    pipelines: &pipelines,
                    operations,
                })
                .unwrap()
            })
            .collect();
        assert_eq!(
            artifacts[0]
                .words
                .chunks_exact(64)
                .map(|w| w[0])
                .collect::<Vec<_>>(),
            [8, 7, 2, 8, 7, 2]
        );
        assert_eq!(
            artifacts[1]
                .words
                .chunks_exact(64)
                .map(|w| w[0])
                .collect::<Vec<_>>(),
            [8, 7, 2, 7, 2]
        );
        assert_eq!(artifacts[0].words[21], 15);
        assert_eq!(artifacts[0].words[3 * 64 + 21], 1);
        assert_eq!(artifacts[1].words[21], 15);
    }
}

pub(crate) const MAX_TEXTURED_DRAWS_PER_SUBMIT: usize = 512;
struct RenderReplayState {
    pipeline: Option<codegen::PipelineId>,
    vertex: Option<(codegen::ObjectId, u64)>,
    index: Option<(codegen::ObjectId, u64, ir::IndexFormat)>,
    texture: Option<codegen::ObjectId>,
    sampler: Option<ir::SamplerDesc>,
    uniforms: Option<ir::DrawUniforms>,
    scissor: Option<ir::PixelRect>,
    viewport: Option<ir::Viewport>,
    color_write_mask: u32,
}

impl Default for RenderReplayState {
    fn default() -> Self {
        Self {
            pipeline: None,
            vertex: None,
            index: None,
            texture: None,
            sampler: None,
            uniforms: None,
            scissor: None,
            viewport: None,
            color_write_mask: 15,
        }
    }
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
        if let Some(viewport) = self.viewport {
            operations.push(codegen::Operation::SetViewport(viewport));
        }
        operations.push(codegen::Operation::SetColorWriteMask(self.color_write_mask));
    }
}

pub(crate) fn split_submission_operations<'data>(
    source: &[codegen::Operation<'data>],
    max_draws: usize,
    pipelines: &[codegen::PipelineMeta],
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
    let mut chunk_records = 0usize;

    for operation in source {
        match operation {
            codegen::Operation::BeginRenderPass(descriptor) => {
                if chunk_records >= max_draws && !current.is_empty() {
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(core::mem::take(&mut current));
                    chunk_draws = 0;
                    chunk_records = 0;
                }
                pass = Some(*descriptor);
                replay = RenderReplayState::default();
                replay.color_write_mask = 15;
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
            codegen::Operation::SetViewport(viewport) => {
                replay.viewport = Some(*viewport);
                current.push(operation.clone());
            }
            codegen::Operation::SetColorWriteMask(mask) => {
                replay.color_write_mask = *mask;
                current.push(operation.clone());
            }
            codegen::Operation::Draw { .. } | codegen::Operation::DrawIndexed { .. } => {
                let extended_blend = replay
                    .pipeline
                    .and_then(|id| pipelines.iter().find(|p| p.id == id))
                    .is_some_and(|pipeline| {
                        !matches!(
                            pipeline.descriptor.blend(),
                            ir::BlendState::REPLACE | ir::BlendState::SOURCE_OVER_STRAIGHT_ALPHA
                        )
                    });
                let record_cost = 1
                    + usize::from(replay.viewport.is_some())
                    + usize::from(replay.color_write_mask != 15 || extended_blend);
                if chunk_draws != 0
                    && (chunk_records + record_cost > max_draws
                        || (replay.texture.is_some()
                            && chunk_draws >= MAX_TEXTURED_DRAWS_PER_SUBMIT))
                {
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
                    chunk_records = 0;
                }
                current.push(operation.clone());
                chunk_draws = chunk_draws
                    .checked_add(1)
                    .ok_or(IrSubmitError::OutOfMemory)?;
                chunk_records = chunk_records
                    .checked_add(record_cost)
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

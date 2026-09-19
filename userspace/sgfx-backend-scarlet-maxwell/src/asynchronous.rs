//! Bounded tracked submissions with immutable per-submission upload storage.

use alloc::{sync::Arc, vec::Vec};
use gpu_raw::{GPU_ABI_VERSION, GPU_RESULT_SUCCESS, GpuQueue};
use sgfx_codegen_maxwell as codegen;
use sgfx_core::backend::SubmitError;

use crate::dispatch::Chunk;
use crate::execute::{prepare_bgra_upload, validate_fixed_command_subset};
use crate::preparation::split_submission_operations;
use crate::resource::{ContextResources, RawBuffer, RawImage};
use crate::scheduler::AdmissionError;
use crate::wire::BoundObject;
use crate::{ContextInner, IrSubmitError, Submission, UnsupportedIrFeature, ir, wire};

const MAX_LOGICAL_BYTES: usize = 64 * 1024 * 1024;

/// Each physical owner retains both the capability and its context attachment.
/// Keeping these snapshots in queued jobs prevents session Drop from revoking
/// authority needed by a native chunk which has not reached the kernel yet.
pub(crate) struct RetainedResources {
    _images: Vec<Arc<RawImage>>,
    _buffers: Vec<Arc<RawBuffer>>,
}

pub(crate) struct DispatchOwner {
    pub(crate) queue: Arc<GpuQueue>,
    pub(crate) _resources: Arc<RetainedResources>,
}

impl ContextResources {
    pub(crate) fn drain_async(&self) -> Result<(), IrSubmitError> {
        self.context.dispatcher.wait_idle()?;
        Ok(())
    }

    pub(crate) fn submit_async<'r, 'data>(
        &mut self,
        context: &ContextInner,
        queue: &Arc<GpuQueue>,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<Submission, SubmitError<IrSubmitError, Submission>> {
        validate_fixed_command_subset(commands).map_err(SubmitError::Rejected)?;
        if context.raw.as_handle().as_raw() != self.context_id() {
            return Err(SubmitError::Rejected(IrSubmitError::ContextMismatch));
        }
        if !core::ptr::eq(commands.resources(), self.resources.as_ref()) {
            return Err(SubmitError::Rejected(IrSubmitError::ResourceTableMismatch));
        }
        let limits = queue
            .query_async()
            .map_err(|_| SubmitError::Rejected(IrSubmitError::AsyncUnsupported))?;
        if limits.abi_version != GPU_ABI_VERSION
            || limits.result != GPU_RESULT_SUCCESS
            || limits.reserved != 0
            || limits.reserved2 != 0
            || limits.max_pending_submissions == 0
            || limits.max_opaque_command_size == 0
        {
            return Err(SubmitError::Rejected(IrSubmitError::AsyncUnsupported));
        }
        // Materialize every logical resource and validate/compile every chunk
        // before accepting work. Borrowed uploads become owned bytes before logical admission.
        let chunks = self
            .prepare_async(context, commands, limits.max_opaque_command_size as usize)
            .map_err(SubmitError::Rejected)?;
        let mut images = Vec::new();
        let mut buffers = Vec::new();
        images
            .try_reserve_exact(self.images.len())
            .map_err(|_| SubmitError::Rejected(IrSubmitError::OutOfMemory))?;
        buffers
            .try_reserve_exact(self.buffers.len())
            .map_err(|_| SubmitError::Rejected(IrSubmitError::OutOfMemory))?;
        images.extend(self.images.iter().flatten().cloned());
        buffers.extend(self.buffers.iter().flatten().cloned());
        let owner = Arc::new(DispatchOwner {
            queue: Arc::clone(queue),
            _resources: Arc::new(RetainedResources {
                _images: images,
                _buffers: buffers,
            }),
        });
        let dispatcher = &self.context.dispatcher;
        match dispatcher.enqueue(owner, chunks) {
            Ok(signal) => Ok(Submission::new(signal)),
            Err(AdmissionError::Busy) => Err(SubmitError::Busy),
            Err(AdmissionError::TooLarge) => {
                Err(SubmitError::Rejected(IrSubmitError::SubmissionTooLarge))
            }
            Err(AdmissionError::OutOfMemory) => {
                Err(SubmitError::Rejected(IrSubmitError::OutOfMemory))
            }
            Err(AdmissionError::Failed(error)) => Err(SubmitError::Rejected(error.into())),
        }
    }

    fn prepare_async<'r, 'data>(
        &mut self,
        context: &ContextInner,
        commands: &ir::CommandBuffer<'r, 'data>,
        native_limit: usize,
    ) -> Result<Vec<Chunk>, IrSubmitError> {
        let mut resources = Vec::new();
        let mut pipelines = Vec::new();
        let mut operations = Vec::new();
        let mut external_bindings = Vec::new();
        let mut chunks = Vec::new();
        let mut owned_bytes = 0usize;
        operations
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for command in commands.commands() {
            match command {
                ir::Command::WriteBuffer {
                    buffer,
                    offset,
                    data,
                } => {
                    self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)?;
                    append_render_chunks(
                        context,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                        native_limit,
                        &mut chunks,
                        &mut owned_bytes,
                    )?;
                    let buffer = Arc::clone(self.buffer(*buffer)?);
                    retain_bytes(&mut owned_bytes, data.len())?;
                    let data = copy_bytes(data)?;
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(Chunk::WriteBuffer {
                        buffer,
                        offset: *offset,
                        data,
                    });
                }
                ir::Command::WriteTexture { texture, write } => {
                    self.ensure_image(*texture, &mut resources, &mut external_bindings)?;
                    append_render_chunks(
                        context,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                        native_limit,
                        &mut chunks,
                        &mut owned_bytes,
                    )?;
                    let image = self.texture(*texture)?;
                    let upload = prepare_bgra_upload(image.logical_format, *write)?;
                    retain_bytes(&mut owned_bytes, upload.pixels.len())?;
                    let data = match upload.pixels {
                        alloc::borrow::Cow::Owned(bytes) => bytes,
                        alloc::borrow::Cow::Borrowed(bytes) => copy_bytes(bytes)?,
                    };
                    let area = write.destination();
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(Chunk::WriteImage {
                        image,
                        data,
                        bytes_per_row: upload.bytes_per_row,
                        area: gpu_raw::GpuImageBgraRect::new(
                            area.x(),
                            area.y(),
                            area.width(),
                            area.height(),
                        ),
                    });
                }
                ir::Command::CopyTextureToTexture {
                    source,
                    source_rect,
                    destination,
                    destination_rect,
                } => {
                    let source =
                        self.ensure_image(*source, &mut resources, &mut external_bindings)?;
                    let destination =
                        self.ensure_image(*destination, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::CopyTextureToTexture {
                        source,
                        source_rect: *source_rect,
                        destination,
                        destination_rect: *destination_rect,
                    });
                }
                ir::Command::BeginRenderPass(pass) => {
                    let target =
                        self.ensure_image(pass.target(), &mut resources, &mut external_bindings)?;
                    let depth = if let Some(depth) = pass.depth_attachment() {
                        Some(codegen::DepthAttachment {
                            target: self.ensure_image(
                                depth.target(),
                                &mut resources,
                                &mut external_bindings,
                            )?,
                            load: depth.load(),
                            store: depth.store(),
                        })
                    } else {
                        None
                    };
                    operations.push(codegen::Operation::BeginRenderPass(codegen::RenderPass {
                        target,
                        area: pass.area(),
                        load: pass.load(),
                        store: pass.store(),
                        depth,
                    }));
                }
                ir::Command::EndRenderPass => operations.push(codegen::Operation::EndRenderPass),
                ir::Command::SetPipeline(pipeline) => {
                    let pipeline = self.ensure_pipeline(*pipeline, &mut pipelines)?;
                    operations.push(codegen::Operation::SetPipeline(pipeline));
                }
                ir::Command::SetVertexBuffer { buffer, offset } => {
                    let buffer =
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::SetVertexBuffer {
                        buffer,
                        offset: *offset,
                    });
                }
                ir::Command::SetIndexBuffer {
                    buffer,
                    offset,
                    format,
                } => {
                    let buffer =
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::SetIndexBuffer {
                        buffer,
                        offset: *offset,
                        format: *format,
                    });
                }
                ir::Command::SetTexture(texture) => {
                    let texture =
                        self.ensure_image(*texture, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::SetTexture(texture));
                }
                ir::Command::SetSampler(sampler) => {
                    let descriptor = self.resources.sampler(*sampler)?;
                    operations.push(codegen::Operation::SetSampler(descriptor));
                }
                ir::Command::SetUniforms(uniforms) => {
                    operations.push(codegen::Operation::SetUniforms(*uniforms));
                }
                ir::Command::SetScissor(scissor) => {
                    operations.push(codegen::Operation::SetScissor(*scissor));
                }
                ir::Command::Draw {
                    vertex_count,
                    first_vertex,
                } => operations.push(codegen::Operation::Draw {
                    vertex_count: *vertex_count,
                    first_vertex: *first_vertex,
                }),
                ir::Command::DrawIndexed {
                    index_count,
                    first_index,
                    base_vertex,
                } => operations.push(codegen::Operation::DrawIndexed {
                    index_count: *index_count,
                    first_index: *first_index,
                    base_vertex: *base_vertex,
                }),
                // The published 1.0 core has only fixed commands; development
                // cores also carry programmable operations, rejected preflight.
                #[allow(unreachable_patterns)]
                _ => {
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ProgrammableExecution,
                    ));
                }
            }
        }

        append_render_chunks(
            context,
            &resources,
            &pipelines,
            &mut operations,
            &external_bindings,
            native_limit,
            &mut chunks,
            &mut owned_bytes,
        )?;
        // Even an empty stream must observe the ordered native queue prefix.
        if chunks.is_empty() {
            chunks.push(Chunk::Commands(Vec::new()));
        }
        Ok(chunks)
    }
}

fn retain_bytes(total: &mut usize, bytes: usize) -> Result<(), IrSubmitError> {
    *total = total
        .checked_add(bytes)
        .filter(|size| *size <= MAX_LOGICAL_BYTES)
        .ok_or(IrSubmitError::SubmissionTooLarge)?;
    Ok(())
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, IrSubmitError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

// Uploads are retained CPU operations, not unsupported pseudo-GPU copies.
// They delimit render chunks and are applied by the dispatcher only after
// the earlier native prefix retired. The caller never waits for that boundary.
fn append_render_chunks<'data>(
    context: &ContextInner,
    resources: &[codegen::ResourceMeta],
    pipelines: &[codegen::PipelineMeta],
    operations: &mut Vec<codegen::Operation<'data>>,
    bindings: &[BoundObject],
    native_limit: usize,
    chunks: &mut Vec<Chunk>,
    owned_bytes: &mut usize,
) -> Result<(), IrSubmitError> {
    if operations.is_empty() {
        return Ok(());
    }
    let mut pending = split_submission_operations(operations, 512)?;
    operations.clear();
    pending.reverse();
    while let Some(operations) = pending.pop() {
        let result = (|| -> Result<Vec<u8>, IrSubmitError> {
            let compiled = codegen::compile(codegen::CompileInput {
                capabilities: context.device.codegen_capabilities,
                resources,
                pipelines,
                operations: &operations,
            })?;
            // The dialect contains clear/draw/image-copy records. Uploads
            // have already been retained as ordered CPU operations above.
            if !compiled.generated_objects.is_empty() {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ResourceState,
                ));
            }
            match wire::encode(&compiled, bindings) {
                Ok(payload) if payload.len() <= native_limit => Ok(payload),
                Ok(_) | Err(IrSubmitError::SubmitWire(maxwell_submit_wire::Error::InvalidSize)) => {
                    Err(IrSubmitError::SubmissionTooLarge)
                }
                Err(error) => Err(error),
            }
        })();
        match result {
            Ok(commands) => {
                retain_bytes(owned_bytes, commands.len())?;
                chunks
                    .try_reserve(1)
                    .map_err(|_| IrSubmitError::OutOfMemory)?;
                chunks.push(Chunk::Commands(commands));
            }
            Err(
                error @ (IrSubmitError::SubmissionTooLarge
                | IrSubmitError::Codegen(codegen::CompileError::CommandBudgetExceeded)),
            ) => {
                let draws = operations
                    .iter()
                    .filter(|operation| {
                        matches!(
                            operation,
                            codegen::Operation::Draw { .. }
                                | codegen::Operation::DrawIndexed { .. }
                        )
                    })
                    .count();
                if draws <= 1 {
                    return Err(error);
                }
                let retry = split_submission_operations(&operations, (draws / 2).max(1))?;
                if retry.len() <= 1 {
                    return Err(error);
                }
                pending
                    .try_reserve(retry.len())
                    .map_err(|_| IrSubmitError::OutOfMemory)?;
                pending.extend(retry.into_iter().rev());
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

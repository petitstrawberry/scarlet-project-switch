//! Bounded tracked submissions with immutable per-submission upload storage.

use alloc::{sync::Arc, vec::Vec};
use gpu_raw::{GPU_ABI_VERSION, GPU_RESULT_SUCCESS, GpuQueue};
use sgfx_codegen_maxwell as codegen;
use sgfx_core::backend::SubmitError;

use crate::dispatch::Chunk;
use crate::execute::validate_fixed_command_subset;
use crate::normalization::{NormalizeVertices, VertexSelection};
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
        crate::programmable::validate_negotiated_commands(context, commands).map_err(SubmitError::Rejected)?;
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

    pub(crate) fn prepare_async<'r, 'data>(
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
        let mut source_pipeline = None;
        let mut source_vertex = None;
        let mut source_index = None;
        let mut normalizations: Vec<NormalizeVertices> = Vec::new();
        let mut programmable = crate::programmable::DrawState::new();
        operations
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for command in commands.commands() {
            if programmable.observe(command)? {
                if let ir::Command::SetProgrammablePipeline(reference) = command {
                    self.compiled_pipeline(reference.id())?;
                    self.prepare_narrow_targets(&mut programmable)?;
                }
                continue;
            }
            if let Some((first,count,base,instances,first_instance)) = programmable.draw(command) {
                let pass = programmable.pass.ok_or(ir::Error::InvalidDescriptor)?;
                let replay: Vec<_> = operations.iter().filter(|operation| matches!(operation,
                    codegen::Operation::SetPipeline(_) | codegen::Operation::SetVertexBuffer { .. }
                    | codegen::Operation::SetIndexBuffer { .. } | codegen::Operation::SetTexture(_)
                    | codegen::Operation::SetSampler(_) | codegen::Operation::SetUniforms(_)
                    | codegen::Operation::SetScissor(_) | codegen::Operation::SetViewport(_) | codegen::Operation::SetColorWriteMask(_))).cloned().collect();
                operations.push(codegen::Operation::EndRenderPass);
                for task in normalizations.drain(..) {
                    retain_bytes(&mut owned_bytes, task.budget_bytes()?)?;
                    chunks.push(Chunk::NormalizeVertices(task));
                }
                append_render_chunks(context, &resources, &pipelines, &mut operations,
                    &external_bindings, native_limit, &mut chunks, &mut owned_bytes)?;
                for conversion in self.narrow_conversions(&mut programmable, false)? {
                    retain_bytes(&mut owned_bytes, conversion.budget_bytes())?;
                    chunks.push(Chunk::ProgrammableDraw(conversion));
                }
                let draw = self.prepare_programmable_draw(&programmable,first,count,base,instances,first_instance)?;
                retain_bytes(&mut owned_bytes, draw.budget_bytes())?;
                chunks.try_reserve(1).map_err(|_| IrSubmitError::OutOfMemory)?;
                chunks.push(Chunk::ProgrammableDraw(draw));
                let target = self.ensure_draw_target(&programmable, &mut resources, &mut external_bindings)?;
                let depth = pass.depth_attachment().map(|depth| Ok::<_,IrSubmitError>(codegen::DepthAttachment {
                    target:self.ensure_image(depth.target(), &mut resources, &mut external_bindings)?,
                    load:ir::DepthLoadOp::Load,store:depth.store(),
                })).transpose()?;
                operations.push(codegen::Operation::BeginRenderPass(codegen::RenderPass {
                    target,area:pass.area(),load:ir::LoadOp::Load,store:pass.store(),depth,
                }));
                operations.extend(replay);
                continue;
            }
            if matches!(command,ir::Command::Draw{..}|ir::Command::DrawIndexed{..}) && Self::needs_narrow_initialization(&programmable) {
                let pass=programmable.pass.ok_or(ir::Error::InvalidDescriptor)?;
                let replay:Vec<_>=operations.iter().filter(|operation|matches!(operation,
                    codegen::Operation::SetPipeline(_) | codegen::Operation::SetVertexBuffer{..}
                    | codegen::Operation::SetIndexBuffer{..} | codegen::Operation::SetTexture(_)
                    | codegen::Operation::SetSampler(_) | codegen::Operation::SetUniforms(_)
                    | codegen::Operation::SetScissor(_) | codegen::Operation::SetViewport(_) | codegen::Operation::SetColorWriteMask(_))).cloned().collect();
                operations.push(codegen::Operation::EndRenderPass);
                for task in normalizations.drain(..){
                    retain_bytes(&mut owned_bytes,task.budget_bytes()?)?;
                    chunks.push(Chunk::NormalizeVertices(task));
                }
                append_render_chunks(context,&resources,&pipelines,&mut operations,&external_bindings,native_limit,&mut chunks,&mut owned_bytes)?;
                for conversion in self.narrow_conversions(&mut programmable,false)?{
                    retain_bytes(&mut owned_bytes,conversion.budget_bytes())?;
                    chunks.try_reserve(1).map_err(|_|IrSubmitError::OutOfMemory)?;
                    chunks.push(Chunk::ProgrammableDraw(conversion));
                }
                let target=self.ensure_draw_target(&programmable,&mut resources,&mut external_bindings)?;
                let depth=pass.depth_attachment().map(|depth|Ok::<_,IrSubmitError>(codegen::DepthAttachment{
                    target:self.ensure_image(depth.target(),&mut resources,&mut external_bindings)?,
                    load:ir::DepthLoadOp::Load,store:depth.store(),
                })).transpose()?;
                operations.push(codegen::Operation::BeginRenderPass(codegen::RenderPass{
                    target,area:pass.area(),load:ir::LoadOp::Load,store:pass.store(),depth,
                }));
                operations.extend(replay);
            }
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
                    append_render_chunks(context, &resources, &pipelines, &mut operations,
                        &external_bindings, native_limit, &mut chunks, &mut owned_bytes)?;
                    let image = self.texture(*texture)?;
                    let transfer = crate::image_subresource::prepare_upload(&self.context, image, *write)?;
                    retain_bytes(&mut owned_bytes, transfer.budget_bytes())?;
                    chunks.try_reserve(1).map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(transfer.into_chunk());
                }
                ir::Command::CopyBufferToBuffer {
                    source,
                    source_offset,
                    destination,
                    destination_offset,
                    size,
                } => {
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
                    let source = Arc::clone(self.buffer(*source)?);
                    let destination = Arc::clone(self.buffer(*destination)?);
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    // Read at dispatch time: an earlier admitted upload may not
                    // have reached its CPU mapping when this call returns.
                    chunks.push(Chunk::CopyBuffer {
                        source,
                        source_offset: *source_offset,
                        destination,
                        destination_offset: *destination_offset,
                        size: *size,
                    });
                }
                ir::Command::ResourceBarrier(_) => {
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
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(Chunk::Barrier);
                }
                ir::Command::CopyTextureToTexture { source, source_rect, destination, destination_rect } => {
                    append_render_chunks(context, &resources, &pipelines, &mut operations,
                        &external_bindings, native_limit, &mut chunks, &mut owned_bytes)?;
                    let source = self.texture(*source)?;
                    let destination = self.texture(*destination)?;
                    let transfer = self.prepare_image_blit(source, destination,
                        0, 0, *source_rect, *destination_rect, ir::FilterMode::Nearest, [false; 2])?;
                    retain_bytes(&mut owned_bytes, transfer.budget_bytes())?;
                    chunks.try_reserve(1).map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(transfer.into_chunk());
                }
                ir::Command::BlitTexture { source, source_mip, destination, destination_mip,
                    source_rect, destination_rect, filter, flips } => {
                    append_render_chunks(context, &resources, &pipelines, &mut operations,
                        &external_bindings, native_limit, &mut chunks, &mut owned_bytes)?;
                    let source = self.texture(*source)?;
                    let destination = self.texture(*destination)?;
                    let transfer = self.prepare_image_blit(source, destination,
                        *source_mip, *destination_mip, *source_rect, *destination_rect, *filter, *flips)?;
                    retain_bytes(&mut owned_bytes, transfer.budget_bytes())?;
                    chunks.try_reserve(1).map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(transfer.into_chunk());
                }
                ir::Command::BeginRenderPass(pass) => {
                    if context.device.capabilities.supports_programmable_graphics(){
                        self.prepare_narrow_targets(&mut programmable)?;
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
                    source_pipeline = None;
                    source_vertex = None;
                    source_index = None;
                    // Extra color attachment initialization is an independent
                    // native clear; programmable draw metadata binds all MRTs.
                    for attachment in pass.color_attachments().skip(1) {
                        let target = self.ensure_image(attachment.target(), &mut resources, &mut external_bindings)?;
                        operations.push(codegen::Operation::BeginRenderPass(codegen::RenderPass {
                            target,area:pass.area(),load:attachment.load(),store:attachment.store(),depth:None,
                        }));
                        operations.push(codegen::Operation::EndRenderPass);
                    }
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
                ir::Command::EndRenderPass => {
                    operations.push(codegen::Operation::EndRenderPass);
                    // No accepted fixed render command writes a buffer. The
                    // pass's CPU fetches may precede its native draws; uploads
                    // and copies outside the pass remain ordered chunk boundaries.
                    for task in normalizations.drain(..) {
                        retain_bytes(&mut owned_bytes, task.budget_bytes()?)?;
                        chunks
                            .try_reserve(1)
                            .map_err(|_| IrSubmitError::OutOfMemory)?;
                        chunks.push(Chunk::NormalizeVertices(task));
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
                    for conversion in self.narrow_conversions(&mut programmable, true)? {
                        retain_bytes(&mut owned_bytes, conversion.budget_bytes())?;
                        chunks.push(Chunk::ProgrammableDraw(conversion));
                    }
                    programmable = crate::programmable::DrawState::new();
                }
                ir::Command::SetPipeline(pipeline) => {
                    source_pipeline = Some(*pipeline);
                    let pipeline = self.ensure_pipeline(*pipeline, &mut pipelines)?;
                    operations.push(codegen::Operation::SetPipeline(pipeline));
                }
                ir::Command::SetVertexBuffer { buffer, offset }
                | ir::Command::SetVertexBufferSlot { slot:0, buffer, offset } => {
                    source_vertex = Some((*buffer, *offset));
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
                    source_index = Some((*buffer, *offset, *format));
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
                ir::Command::SetViewport(viewport) => {
                    operations.push(codegen::Operation::SetViewport(*viewport));
                }
                ir::Command::Draw {
                    vertex_count,
                    first_vertex,
                } => {
                    let selection = VertexSelection::Vertices {
                        first: *first_vertex,
                        count: *vertex_count,
                    };
                    let (buffer, offset) = self.prepare_vertex_buffer(
                        source_pipeline,
                        source_vertex,
                        selection,
                        &mut normalizations,
                        &mut resources,
                        &mut external_bindings,
                    )?;
                    operations.push(codegen::Operation::SetVertexBuffer { buffer, offset });
                    let format=self.resources.texture(programmable.pass.ok_or(ir::Error::InvalidDescriptor)?.target())?.format();
                    operations.push(codegen::Operation::SetColorWriteMask(match format{ir::TextureFormat::R8Unorm=>1,ir::TextureFormat::Rg8Unorm=>3,_=>15}));
                    operations.push(codegen::Operation::Draw {
                        vertex_count: *vertex_count,
                        first_vertex: *first_vertex,
                    });
                }
                ir::Command::DrawIndexed {
                    index_count,
                    first_index,
                    base_vertex,
                } => {
                    let (index, offset, format) =
                        source_index.ok_or(ir::Error::IndexBufferNotSet)?;
                    let selection = VertexSelection::Indexed {
                        buffer: Arc::clone(self.buffer(index)?),
                        offset,
                        format,
                        first: *first_index,
                        count: *index_count,
                        base: *base_vertex,
                    };
                    let (buffer, offset) = self.prepare_vertex_buffer(
                        source_pipeline,
                        source_vertex,
                        selection,
                        &mut normalizations,
                        &mut resources,
                        &mut external_bindings,
                    )?;
                    operations.push(codegen::Operation::SetVertexBuffer { buffer, offset });
                    let format=self.resources.texture(programmable.pass.ok_or(ir::Error::InvalidDescriptor)?.target())?.format();
                    operations.push(codegen::Operation::SetColorWriteMask(match format{ir::TextureFormat::R8Unorm=>1,ir::TextureFormat::Rg8Unorm=>3,_=>15}));
                    operations.push(codegen::Operation::DrawIndexed {
                        index_count: *index_count,
                        first_index: *first_index,
                        base_vertex: *base_vertex,
                    });
                }
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
    let mut pending = split_submission_operations(operations, 512, pipelines)?;
    operations.clear();
    pending.reverse();
    while let Some(operations) = pending.pop() {
        let result = (|| -> Result<Vec<u8>, IrSubmitError> {
            let mut compiled = codegen::compile(codegen::CompileInput {
                capabilities: context.device.codegen_capabilities,
                resources,
                pipelines,
                operations: &operations,
            })?;
            if !context.device.capabilities.extended { wire::normalize_legacy_fixed_commands(&mut compiled.words)?; }
            if compiled.words.is_empty() {
                return Ok(Vec::new());
            }
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
                if commands.is_empty() { continue; }
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
                let retry = split_submission_operations(&operations, (draws / 2).max(1), pipelines)?;
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

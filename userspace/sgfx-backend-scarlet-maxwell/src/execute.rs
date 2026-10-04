//! Full SGFX command normalization and Maxwell queue submission.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU8, Ordering};

use sgfx_codegen_maxwell as codegen;

use crate::normalization::{
    NormalizeVertices, VertexSelection, canonical_descriptor, native_layout, vertex_capacity,
};
use crate::preparation::split_submission_operations;
use crate::resource::ContextResources;
use crate::resource::RawBuffer;
use crate::wire::BoundObject;
use crate::{ContextInner, IrSubmitError, UnsupportedIrFeature, ir, wire};

const IMAGE_OBJECT_BASE: u32 = 1;
const BUFFER_OBJECT_BASE: u32 = 1 << 16;
const PIPELINE_OBJECT_BASE: u32 = 1 << 24;
// Each canonical GM20B operation occupies 64 words. The GM20B queue
// advertises a 2 MiB transport so the production 512-item ScarletUI workload
// remains one render pass instead of forcing repeated full-surface load/store
// retirements. This is only the optimistic limit: the submission path bisects
// any unusually resource-heavy chunk whose exact wire encoding exceeds the
// negotiated ABI limit.
const MAX_DRAWS_PER_SUBMIT: usize = 512;

fn submit_trace_enabled() -> bool {
    static TRACE_CACHE: AtomicU8 = AtomicU8::new(u8::MAX);
    let cached = TRACE_CACHE.load(Ordering::Relaxed);
    if cached != u8::MAX {
        return cached != 0;
    }
    #[cfg(feature = "std")]
    let value = std::env::var("SGFX_MAXWELL_TRACE").ok();
    #[cfg(not(feature = "std"))]
    let value = std::env::var("SGFX_MAXWELL_TRACE");
    let enabled = value.as_deref().is_some_and(|value| {
        matches!(
            value,
            "1" | "true" | "TRUE" | "debug" | "DEBUG" | "trace" | "TRACE"
        )
    });
    TRACE_CACHE.store(enabled as u8, Ordering::Relaxed);
    enabled
}

#[cfg(feature = "std")]
fn monotonic_time_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(not(feature = "std"))]
fn monotonic_time_ns() -> u64 {
    // SAFETY: This fixed clock query has no arguments or userspace memory effects.
    (unsafe { std::syscall::syscall0(std::syscall::Syscall::MonotonicTime) }) as u64
}

impl ContextResources {
    pub(crate) fn execute<'r, 'data>(
        &mut self,
        context: &ContextInner,
        queue: &Arc<gpu_raw::GpuQueue>,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<(), IrSubmitError> {
        use sgfx_core::backend::{Completion, SubmitError};
        loop {
            match self.submit_async(context, queue, commands) {
                Ok(submission) => {
                    submission.wait(None)?;
                    return Ok(());
                }
                Err(SubmitError::Busy) => {
                    self.drain_async()?;
                    std::thread::yield_now();
                }
                Err(SubmitError::Rejected(IrSubmitError::AsyncUnsupported)) => {
                    self.drain_async()?;
                    let owner = Arc::clone(&self.context);
                    return owner
                        .dispatcher
                        .with_idle(|| self.execute_native(context, queue, commands));
                }
                Err(SubmitError::Rejected(error) | SubmitError::Failed { error, .. }) => {
                    return Err(error);
                }
                Err(_) => return Err(IrSubmitError::CompletionUnavailable),
            }
        }
    }

    fn execute_native<'r, 'data>(
        &mut self,
        context: &ContextInner,
        queue: &gpu_raw::GpuQueue,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<(), IrSubmitError> {
        validate_fixed_command_subset(commands)?;
        crate::programmable::validate_negotiated_commands(context, commands)?;
        if context.raw.as_handle().as_raw() != self.context_id() {
            return Err(IrSubmitError::ContextMismatch);
        }
        if !core::ptr::eq(commands.resources(), self.resources.as_ref()) {
            return Err(IrSubmitError::ResourceTableMismatch);
        }
        if commands.commands().iter().any(|command| matches!(command,
            ir::Command::SetProgrammablePipeline(_) | ir::Command::DrawInstanced { .. }
            | ir::Command::DrawIndexedInstanced { .. } | ir::Command::SetVertexBufferSlot{..})
            || matches!(command,ir::Command::BeginRenderPass(pass) if commands.resources().texture(pass.target()).is_ok_and(|texture|matches!(texture.format(),ir::TextureFormat::R8Unorm|ir::TextureFormat::Rg8Unorm)))) {
            let chunks = self.prepare_async(context, commands, maxwell_submit_wire::MAX_SUBMIT_SIZE)?;
            for chunk in &chunks { crate::dispatch::execute_synchronously(queue, chunk)?; }
            return Ok(());
        }

        let mut resources = Vec::new();
        let mut pipelines = Vec::new();
        let mut operations = Vec::new();
        let mut external_bindings = Vec::new();
        let mut source_pipeline = None;
        let mut source_vertex = None;
        let mut source_index = None;
        let mut normalizations: Vec<NormalizeVertices> = Vec::new();
        let mut retained_staging = Vec::new();
        resources
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        operations
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        external_bindings
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;

        for command in commands.commands() {
            match command {
                ir::Command::WriteBuffer {
                    buffer,
                    offset,
                    data,
                } => {
                    // Every GM20B SGFX buffer is CPU-visible and mapped into
                    // driver-owned GPU backing at submit time. Drain
                    // earlier GPU work to preserve IR ordering, then update the
                    // retained generic mapping directly. Encoding GPU copies for
                    // dynamic UI data duplicated the host copy and generated
                    // thousands of relocation/authority records per frame.
                    self.submit_operations(
                        context,
                        queue,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                    )?;
                    self.write_buffer(*buffer, *offset, data)?;
                }
                ir::Command::WriteTexture { texture, write } => {
                    let descriptor = self.resources.texture(*texture)?;
                    require_texture_upload_format(descriptor.format())?;

                    // Image uploads can be much larger than the bounded opaque
                    // submit wire. Flush earlier GPU work to preserve IR
                    // ordering, then use Scarlet's synchronous generic BGRA
                    // upload path, which writes the same kernel-owned linear
                    // backing already attached to this context.
                    self.submit_operations(
                        context,
                        queue,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                    )?;
                    self.upload_texture_bgra(queue, *texture, *write)?;
                }
                ir::Command::CopyBufferToBuffer {
                    source,
                    source_offset,
                    destination,
                    destination_offset,
                    size,
                } => {
                    self.submit_operations(
                        context,
                        queue,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                    )?;
                    let source = Arc::clone(self.buffer(*source)?);
                    let destination = Arc::clone(self.buffer(*destination)?);
                    destination.copy_from(&source, *source_offset, *destination_offset, *size)?;
                }
                ir::Command::ResourceBarrier(_) => {
                    // The legacy native submit retires synchronously. Completing
                    // the earlier batch establishes the requested memory dependency.
                    self.submit_operations(
                        context,
                        queue,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                    )?;
                }
                ir::Command::CopyTextureToTexture {
                    source,
                    source_rect,
                    destination,
                    destination_rect,
                } => {
                    self.submit_operations(context, queue, &resources, &pipelines, &mut operations, &external_bindings)?;
                    let source = self.texture(*source)?;
                    let destination = self.texture(*destination)?;
                    let transfer = self.prepare_image_blit(
                        source, destination, 0, 0,
                        *source_rect, *destination_rect, ir::FilterMode::Nearest, [false; 2])?;
                    transfer.submit(queue)?;
                }
                ir::Command::BlitTexture { source, source_mip, destination, destination_mip,
                    source_rect, destination_rect, filter, flips } => {
                    self.submit_operations(context, queue, &resources, &pipelines, &mut operations, &external_bindings)?;
                    let source = self.texture(*source)?;
                    let destination = self.texture(*destination)?;
                    let transfer = self.prepare_image_blit(
                        source, destination, *source_mip, *destination_mip,
                        *source_rect, *destination_rect, *filter, *flips)?;
                    transfer.submit(queue)?;
                }
                ir::Command::BeginRenderPass(pass) => {
                    self.submit_operations(
                        context,
                        queue,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                    )?;
                    source_pipeline = None;
                    source_vertex = None;
                    source_index = None;
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
                    for task in normalizations.drain(..) {
                        task.execute()?;
                        retained_staging.push(task);
                    }
                    self.submit_operations(
                        context,
                        queue,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                    )?;
                }
                ir::Command::SetPipeline(pipeline) => {
                    source_pipeline = Some(*pipeline);
                    let pipeline = self.ensure_pipeline(*pipeline, &mut pipelines)?;
                    operations.push(codegen::Operation::SetPipeline(pipeline));
                }
                ir::Command::SetVertexBuffer { buffer, offset }
                | ir::Command::SetVertexBufferSlot{slot:0,buffer,offset} => {
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
                    operations.push(codegen::Operation::DrawIndexed {
                        index_count: *index_count,
                        first_index: *first_index,
                        base_vertex: *base_vertex,
                    });
                }
                // Keep the published fixed-only 1.0 core and development
                // programmable IR compatible with the same explicit rejection.
                #[allow(unreachable_patterns)]
                _ => {
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ProgrammableExecution,
                    ));
                }
            }
        }

        self.submit_operations(
            context,
            queue,
            &resources,
            &pipelines,
            &mut operations,
            &external_bindings,
        )
    }

    fn submit_operations<'data>(
        &mut self,
        context: &ContextInner,
        queue: &gpu_raw::GpuQueue,
        resources: &[codegen::ResourceMeta],
        pipelines: &[codegen::PipelineMeta],
        operations: &mut Vec<codegen::Operation<'data>>,
        external_bindings: &[BoundObject],
    ) -> Result<(), IrSubmitError> {
        if operations.is_empty() {
            return Ok(());
        }
        let chunks = split_submission_operations(operations, MAX_DRAWS_PER_SUBMIT, pipelines)?;
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(chunks.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        pending.extend(chunks.into_iter().rev());
        while let Some(chunk) = pending.pop() {
            let result = self.submit_one(
                context,
                queue,
                resources,
                pipelines,
                &chunk,
                external_bindings,
            );
            match result {
                Ok(()) => {}
                Err(error @ (IrSubmitError::SubmitWire(maxwell_submit_wire::Error::InvalidSize)
                    | IrSubmitError::Codegen(codegen::CompileError::CommandBudgetExceeded))) => {
                    let draw_count = chunk
                        .iter()
                        .filter(|operation| {
                            matches!(
                                operation,
                                codegen::Operation::Draw { .. }
                                    | codegen::Operation::DrawIndexed { .. }
                            )
                        })
                        .count();
                    if draw_count <= 1 {
                        return Err(error);
                    }
                    let retry_limit = (draw_count / 2).max(1);
                    let retry = split_submission_operations(&chunk, retry_limit, pipelines)?;
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
        operations.clear();
        Ok(())
    }

    fn submit_one<'data>(
        &mut self,
        context: &ContextInner,
        queue: &gpu_raw::GpuQueue,
        resources: &[codegen::ResourceMeta],
        pipelines: &[codegen::PipelineMeta],
        operations: &[codegen::Operation<'data>],
        external_bindings: &[BoundObject],
    ) -> Result<(), IrSubmitError> {
        let trace = submit_trace_enabled();
        let started = if trace { monotonic_time_ns() } else { 0 };
        let mut compiled = codegen::compile(codegen::CompileInput {
            capabilities: context.device.codegen_capabilities,
            resources,
            pipelines,
            operations,
        })?;
        if !context.device.capabilities.extended { wire::normalize_legacy_fixed_commands(&mut compiled.words)?; }
        let compiled_at = if trace { monotonic_time_ns() } else { 0 };
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(external_bindings.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        bindings.extend_from_slice(external_bindings);
        self.materialize_generated(&compiled, &mut bindings)?;
        let materialized_at = if trace { monotonic_time_ns() } else { 0 };
        let payload = wire::encode(&compiled, &bindings)?;
        let encoded_at = if trace { monotonic_time_ns() } else { 0 };
        let result = queue.submit(&payload);
        let submitted_at = if trace { monotonic_time_ns() } else { 0 };
        if trace {
            let draws = operations
                .iter()
                .filter(|operation| {
                    matches!(
                        operation,
                        codegen::Operation::Draw { .. } | codegen::Operation::DrawIndexed { .. }
                    )
                })
                .count();
            std::println!(
                "[gm20b-userspace-path] ops={} draws={} wire={} words={} resources={} relocs={} compile_us={} materialize_us={} encode_us={} queue_us={}",
                operations.len(),
                draws,
                payload.len(),
                compiled.words.len(),
                compiled.accesses.len(),
                compiled.fixups.len(),
                compiled_at.saturating_sub(started) / 1_000,
                materialized_at.saturating_sub(compiled_at) / 1_000,
                encoded_at.saturating_sub(materialized_at) / 1_000,
                submitted_at.saturating_sub(encoded_at) / 1_000,
            );
        }
        result.map_err(|error| {
            std::println!(
                "[gm20b-userspace] queue submit bytes={}: {:?}",
                payload.len(),
                error
            );
            error
        })?;
        Ok(())
    }

    fn upload_texture_bgra(
        &mut self, queue: &gpu_raw::GpuQueue, reference: ir::TextureRef<'_>,
        write: ir::TextureWrite<'_>,
    ) -> Result<(), IrSubmitError> {
        let image = self.texture(reference)?;
        let transfer = crate::image_subresource::prepare_upload(&self.context, image, write)?;
        transfer.submit(queue)?;
        Ok(())
    }

    pub(crate) fn ensure_image(
        &mut self,
        reference: ir::TextureRef<'_>,
        metadata: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<codegen::ObjectId, IrSubmitError> {
        self.ensure_image_with_usage(reference, ir::TextureUsage::empty(), metadata, bindings)
    }

    fn ensure_image_with_usage(
        &mut self,
        reference: ir::TextureRef<'_>,
        additional_usage: ir::TextureUsage,
        metadata: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<codegen::ObjectId, IrSubmitError> {
        let id = image_object_id(reference.slot())?;
        if metadata.iter().any(|resource| resource.id == id) {
            return Ok(id);
        }
        let descriptor = self.resources.texture(reference)?;
        let image = self.texture(reference)?;
        append_image_resource(
            id,
            image.as_ref(),
            descriptor,
            descriptor.usage() | additional_usage,
            metadata,
            bindings,
        )?;
        Ok(id)
    }

    pub(crate) fn ensure_buffer(
        &mut self,
        reference: ir::BufferRef<'_>,
        metadata: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<codegen::ObjectId, IrSubmitError> {
        let id = buffer_object_id(reference.slot())?;
        if metadata.iter().any(|resource| resource.id == id) {
            return Ok(id);
        }
        let descriptor = self.resources.buffer(reference)?;
        let buffer = self.buffer(reference)?;
        metadata
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        metadata.push(codegen::ResourceMeta {
            id,
            size: descriptor.size(),
            kind: codegen::ResourceKind::Buffer {
                usage: descriptor.usage(),
            },
        });
        bindings
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        bindings.push(BoundObject {
            object: codegen::ObjectRef::External(id),
            attachment_token: buffer.attachment_token,
            allocation_offset: 0,
            size: descriptor.size(),
        });
        Ok(id)
    }

    pub(crate) fn ensure_pipeline(
        &self,
        reference: ir::RenderPipelineRef<'_>,
        metadata: &mut Vec<codegen::PipelineMeta>,
    ) -> Result<codegen::PipelineId, IrSubmitError> {
        let id = pipeline_object_id(reference.slot())?;
        if metadata.iter().any(|pipeline| pipeline.id == id) {
            return Ok(id);
        }
        let descriptor = self.resources.render_pipeline(reference)?;
        metadata
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        let mut native=canonical_descriptor(&descriptor)?;
        if matches!(native.target_format(),ir::TextureFormat::R8Unorm|ir::TextureFormat::Rg8Unorm){
            let mut scratch=ir::RenderPipelineDesc::new(ir::TextureFormat::Bgra8Unorm,native.topology(),native.vertex_buffer().clone(),native.fragment(),native.blend(),native.raster())?;
            if let Some(depth)=native.depth_state(){scratch=scratch.with_depth_state(depth)?;}
            native=scratch;
        }
        metadata.push(codegen::PipelineMeta {
            id,
            descriptor: native,
        });
        Ok(id)
    }

    pub(crate) fn prepare_vertex_buffer<'r>(
        &mut self,
        pipeline: Option<ir::RenderPipelineRef<'r>>,
        vertex: Option<(ir::BufferRef<'r>, u64)>,
        selection: VertexSelection,
        tasks: &mut Vec<NormalizeVertices>,
        metadata: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<(codegen::ObjectId, u64), IrSubmitError> {
        let descriptor = self
            .resources
            .render_pipeline(pipeline.ok_or(ir::Error::PipelineNotSet)?)?;
        let (reference, offset) = vertex.ok_or(ir::Error::VertexBufferNotSet)?;
        if native_layout(&descriptor) {
            return Ok((self.ensure_buffer(reference, metadata, bindings)?, offset));
        }
        let source = Arc::clone(self.buffer(reference)?);
        if let Some(task) = tasks.iter_mut().find(|task| {
            Arc::ptr_eq(&task.source, &source)
                && task.source_offset == offset
                && task.descriptor == descriptor
        }) {
            task.selections
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            task.selections.push(selection);
            return Ok((task.object, 0));
        }
        let count = vertex_capacity(
            source.logical_size,
            offset,
            descriptor.vertex_buffer().stride(),
        )?;
        let canonical = canonical_descriptor(&descriptor)?;
        let size = count
            .checked_mul(u64::from(canonical.vertex_buffer().stride()))
            .filter(|size| *size != 0 && *size <= 64 * 1024 * 1024)
            .ok_or(IrSubmitError::SubmissionTooLarge)?;
        let retained_size = metadata
            .iter()
            .filter(|resource| resource.id.raw() >= (1 << 28))
            .try_fold(size, |total, resource| total.checked_add(resource.size))
            .filter(|total| *total <= 64 * 1024 * 1024)
            .ok_or(IrSubmitError::SubmissionTooLarge)?;
        let _ = retained_size;
        let id = codegen::ObjectId::new(
            (1u32 << 28)
                .checked_add(u32::try_from(metadata.len()).map_err(|_| IrSubmitError::OutOfMemory)?)
                .ok_or(ir::Error::Overflow)?,
        );
        let destination = Arc::new(RawBuffer::create(&self.context, size)?);
        metadata
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        bindings
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        tasks
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        let mut selections = Vec::new();
        selections
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        selections.push(selection);
        metadata.push(codegen::ResourceMeta {
            id,
            size,
            kind: codegen::ResourceKind::Buffer {
                usage: ir::BufferUsage::VERTEX,
            },
        });
        bindings.push(BoundObject {
            object: codegen::ObjectRef::External(id),
            attachment_token: destination.attachment_token,
            allocation_offset: 0,
            size,
        });
        tasks.push(NormalizeVertices {
            source,
            destination,
            source_offset: offset,
            descriptor,
            object: id,
            selections,
        });
        Ok((id, 0))
    }

    fn materialize_generated(
        &mut self,
        compiled: &codegen::RelocatableCommands,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<(), IrSubmitError> {
        if compiled.generated_objects.is_empty() {
            return Ok(());
        }
        let mut placements = Vec::new();
        placements
            .try_reserve_exact(compiled.generated_objects.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        let mut total = 0u64;
        for generated in &compiled.generated_objects {
            if generated.bytes.is_empty()
                || generated.alignment == 0
                || !generated.alignment.is_power_of_two()
            {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ResourceState,
                ));
            }
            total = align_up(total, generated.alignment)?;
            let size =
                u64::try_from(generated.bytes.len()).map_err(|_| IrSubmitError::OutOfMemory)?;
            placements.push((generated.id, total, size));
            total = total.checked_add(size).ok_or(IrSubmitError::OutOfMemory)?;
        }
        let (scratch_token, scratch_size) = {
            let scratch = self.scratch(total)?;
            // The generated-object offsets already describe disjoint ranges
            // in the retained CPU-visible scratch buffer.  Write each object
            // directly instead of assembling and then copying a second full
            // staging Vec on every submit. Unaddressed alignment gaps are unused.
            for (generated, (_, offset, size)) in compiled
                .generated_objects
                .iter()
                .zip(placements.iter().copied())
            {
                if usize::try_from(size).ok() != Some(generated.bytes.len()) {
                    return Err(IrSubmitError::OutOfMemory);
                }
                scratch.write(offset, &generated.bytes)?;
            }
            (scratch.attachment_token, scratch.logical_size)
        };
        bindings
            .try_reserve_exact(placements.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for (id, offset, size) in placements {
            let end = offset.checked_add(size).ok_or(IrSubmitError::OutOfMemory)?;
            if end > scratch_size {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ResourceState,
                ));
            }
            bindings.push(BoundObject {
                object: codegen::ObjectRef::Generated(id),
                attachment_token: scratch_token,
                allocation_offset: offset,
                size,
            });
        }
        Ok(())
    }
}

/// Reject extended IR before an upload, native submission, or cold mapping.
pub(crate) fn validate_fixed_command_subset(
    commands: &ir::CommandBuffer<'_, '_>,
) -> Result<(), IrSubmitError> {
    for command in commands.commands() {
        if !matches!(
            command,
            ir::Command::WriteBuffer { .. }
                | ir::Command::WriteTexture { .. }
                | ir::Command::CopyTextureToTexture { .. }
                | ir::Command::BlitTexture { .. }
                | ir::Command::CopyBufferToBuffer { .. }
                | ir::Command::ResourceBarrier(_)
                | ir::Command::BeginRenderPass(_)
                | ir::Command::EndRenderPass
                | ir::Command::SetPipeline(_)
                | ir::Command::SetVertexBuffer { .. }
                | ir::Command::SetIndexBuffer { .. }
                | ir::Command::SetTexture(_)
                | ir::Command::SetSampler(_)
                | ir::Command::SetUniforms(_)
                | ir::Command::SetScissor(_)
                | ir::Command::SetProgrammablePipeline(_)
                | ir::Command::SetBindGroup { .. }
                | ir::Command::SetPushConstants { .. }
                | ir::Command::SetVertexBufferSlot { .. }
                | ir::Command::SetViewport(_)
                | ir::Command::DrawInstanced { .. }
                | ir::Command::DrawIndexedInstanced { .. }
                | ir::Command::Draw { .. }
                | ir::Command::DrawIndexed { .. }
        ) {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ProgrammableExecution,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) use crate::image_subresource::prepare_bgra_upload;

fn image_object_id(slot: usize) -> Result<codegen::ObjectId, IrSubmitError> {
    let slot = u32::try_from(slot).map_err(|_| IrSubmitError::OutOfMemory)?;
    Ok(codegen::ObjectId::new(
        IMAGE_OBJECT_BASE
            .checked_add(slot)
            .ok_or(IrSubmitError::OutOfMemory)?,
    ))
}

fn buffer_object_id(slot: usize) -> Result<codegen::ObjectId, IrSubmitError> {
    let slot = u32::try_from(slot).map_err(|_| IrSubmitError::OutOfMemory)?;
    Ok(codegen::ObjectId::new(
        BUFFER_OBJECT_BASE
            .checked_add(slot)
            .ok_or(IrSubmitError::OutOfMemory)?,
    ))
}

pub(crate) fn append_image_resource(
    id: codegen::ObjectId,
    image: &crate::resource::RawImage,
    descriptor: ir::TextureDesc,
    usage: ir::TextureUsage,
    metadata: &mut Vec<codegen::ResourceMeta>,
    bindings: &mut Vec<BoundObject>,
) -> Result<(), IrSubmitError> {
    if image.logical_format != descriptor.format()
        || metadata.iter().any(|resource| resource.id == id)
        || bindings
            .iter()
            .any(|binding| binding.object == codegen::ObjectRef::External(id))
    {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ResourceState,
        ));
    }
    let plane_count = usize::try_from(image.layout.plane_count)
        .map_err(|_| IrSubmitError::Unsupported(UnsupportedIrFeature::ImageLayout))?;
    let mut planes = Vec::new();
    planes
        .try_reserve_exact(plane_count)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for plane in &image.layout.planes[..plane_count] {
        planes.push(codegen::PlaneLayout {
            offset: plane.offset,
            stride: plane.row_pitch,
            size: plane.size,
        });
    }
    metadata
        .try_reserve(1)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    let chain = image.subresources.is_some_and(|l| l.descriptor.mip_levels > 1 || l.descriptor.array_layers > 1);
    let modifier = match image.layout.modifier {
        gpu_raw::GPU_IMAGE_MODIFIER_LINEAR => codegen::ImageModifier::Linear,
        value if descriptor.format() != ir::TextureFormat::Nv12
            && maxwell_image_layout::modifier_tile_y(value).is_some()
            && (chain || (value != gpu_raw::GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4
                && value != gpu_raw::GPU_IMAGE_MODIFIER_NVIDIA_ZF32_BLOCK_LINEAR_16BX2_H4)) => {
            let tile_y_log2 = maxwell_image_layout::modifier_tile_y(value).unwrap();
            if value & !0xf == maxwell_image_layout::NVIDIA_DEPTH_MODIFIER_BASE {
                codegen::ImageModifier::NvidiaZf32BlockLinear { tile_y_log2 }
            } else { codegen::ImageModifier::NvidiaBlockLinear { tile_y_log2 } }
        },
        0x0300_0000_000f_e011 => codegen::ImageModifier::NvidiaBlockLinear16Bx2H1,
        gpu_raw::GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4 => {
            codegen::ImageModifier::NvidiaBlockLinear16Bx2H4
        }
        gpu_raw::GPU_IMAGE_MODIFIER_NVIDIA_ZF32_BLOCK_LINEAR_16BX2_H4 => {
            codegen::ImageModifier::NvidiaZf32BlockLinear16Bx2H4
        }
        _ => {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ImageLayout,
            ));
        }
    };
    metadata.push(codegen::ResourceMeta {
        id,
        size: image.allocation_size(),
        kind: codegen::ResourceKind::Image(codegen::ImageMeta {
            format: descriptor.format(),
            storage_format: match descriptor.format() {
                ir::TextureFormat::Nv12 => ir::TextureFormat::Nv12,
                ir::TextureFormat::Depth32Float => ir::TextureFormat::Depth32Float,
                ir::TextureFormat::Bgra8Unorm
                | ir::TextureFormat::Rgba8Unorm
                | ir::TextureFormat::R8Unorm
                | ir::TextureFormat::Rg8Unorm
                | ir::TextureFormat::Bgra8UnormSrgb
                | ir::TextureFormat::Rgba8UnormSrgb => ir::TextureFormat::Bgra8Unorm,
            },
            extent: descriptor.extent(),
            usage,
            modifier,
            planes,
            subresources: image.subresources,
        }),
    });
    bindings
        .try_reserve(1)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    bindings.push(BoundObject {
        object: codegen::ObjectRef::External(id),
        attachment_token: image.attachment_token,
        allocation_offset: 0,
        size: image.allocation_size(),
    });
    Ok(())
}

fn pipeline_object_id(slot: usize) -> Result<codegen::PipelineId, IrSubmitError> {
    let slot = u32::try_from(slot).map_err(|_| IrSubmitError::OutOfMemory)?;
    Ok(codegen::PipelineId::new(
        PIPELINE_OBJECT_BASE
            .checked_add(slot)
            .ok_or(IrSubmitError::OutOfMemory)?,
    ))
}

fn align_up(value: u64, alignment: u64) -> Result<u64, IrSubmitError> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(IrSubmitError::OutOfMemory)
}

fn require_texture_upload_format(format: ir::TextureFormat) -> Result<(), IrSubmitError> {
    match format {
        ir::TextureFormat::Bgra8Unorm
        | ir::TextureFormat::Rgba8Unorm
        | ir::TextureFormat::R8Unorm
        | ir::TextureFormat::Rg8Unorm
        | ir::TextureFormat::Bgra8UnormSrgb
        | ir::TextureFormat::Rgba8UnormSrgb => Ok(()),
        ir::TextureFormat::Nv12
        | ir::TextureFormat::Depth32Float => Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::TextureUpload,
        )),
    }
}

//! Full SGFX command normalization and Maxwell queue submission.

use alloc::{borrow::Cow, vec::Vec};
use core::sync::atomic::{AtomicU8, Ordering};

use gpu_raw::GpuImageBgraRect;
use sgfx_codegen_maxwell as codegen;

use crate::preparation::split_submission_operations;
use crate::resource::ContextResources;
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
        queue: &gpu_raw::GpuQueue,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<(), IrSubmitError> {
        validate_fixed_command_subset(commands)?;
        self.drain_async()?;
        if context.raw.as_handle().as_raw() != self.context_id() {
            return Err(IrSubmitError::ContextMismatch);
        }
        if !core::ptr::eq(commands.resources(), self.resources.as_ref()) {
            return Err(IrSubmitError::ResourceTableMismatch);
        }

        let mut resources = Vec::new();
        let mut pipelines = Vec::new();
        let mut operations = Vec::new();
        let mut external_bindings = Vec::new();
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
                    self.ensure_image(*texture, &mut resources, &mut external_bindings)?;

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
                    self.upload_texture_bgra(context, *texture, *write)?;
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
        let chunks = split_submission_operations(operations, MAX_DRAWS_PER_SUBMIT)?;
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
                Err(error @ IrSubmitError::SubmitWire(maxwell_submit_wire::Error::InvalidSize)) => {
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
                    let retry = split_submission_operations(&chunk, retry_limit)?;
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
        let compiled = codegen::compile(codegen::CompileInput {
            capabilities: context.device.codegen_capabilities,
            resources,
            pipelines,
            operations,
        })?;
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
        result?;
        Ok(())
    }

    fn upload_texture_bgra(
        &mut self,
        context: &ContextInner,
        reference: ir::TextureRef<'_>,
        write: ir::TextureWrite<'_>,
    ) -> Result<(), IrSubmitError> {
        let descriptor = self.resources.texture(reference)?;
        let image = self.texture(reference)?;
        let upload = prepare_bgra_upload(descriptor.format(), write)?;
        let area = write.destination();
        context.raw.upload_image_bgra(
            &image.raw,
            upload.pixels.as_ref(),
            upload.bytes_per_row,
            GpuImageBgraRect::new(area.x(), area.y(), area.width(), area.height()),
        )?;
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
        metadata.push(codegen::PipelineMeta { id, descriptor });
        Ok(id)
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
                | ir::Command::BeginRenderPass(_)
                | ir::Command::EndRenderPass
                | ir::Command::SetPipeline(_)
                | ir::Command::SetVertexBuffer { .. }
                | ir::Command::SetIndexBuffer { .. }
                | ir::Command::SetTexture(_)
                | ir::Command::SetSampler(_)
                | ir::Command::SetUniforms(_)
                | ir::Command::SetScissor(_)
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

struct PreparedBgraUpload<'data> {
    pixels: Cow<'data, [u8]>,
    bytes_per_row: u32,
}

fn prepare_bgra_upload<'data>(
    format: ir::TextureFormat,
    write: ir::TextureWrite<'data>,
) -> Result<PreparedBgraUpload<'data>, IrSubmitError> {
    if format == ir::TextureFormat::Bgra8Unorm {
        return Ok(PreparedBgraUpload {
            pixels: Cow::Borrowed(write.data()),
            bytes_per_row: write.bytes_per_row(),
        });
    }
    if !matches!(
        format,
        ir::TextureFormat::Rgba8Unorm | ir::TextureFormat::R8Unorm
    ) {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::TextureUpload,
        ));
    }

    let area = write.destination();
    let destination_stride = area
        .width()
        .checked_mul(ir::TextureFormat::Bgra8Unorm.bytes_per_pixel())
        .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?;
    let destination_len = usize::try_from(
        u64::from(destination_stride)
            .checked_mul(u64::from(area.height()))
            .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?,
    )
    .map_err(|_| IrSubmitError::OutOfMemory)?;
    let logical_row_bytes = area
        .width()
        .checked_mul(format.bytes_per_pixel())
        .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(destination_len)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    pixels.resize(destination_len, 0);

    for row in 0..area.height() {
        let source_start = usize::try_from(
            u64::from(row)
                .checked_mul(u64::from(write.bytes_per_row()))
                .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?,
        )
        .map_err(|_| IrSubmitError::InvalidIr(ir::Error::Overflow))?;
        let source_end = source_start
            .checked_add(logical_row_bytes as usize)
            .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?;
        let source = write
            .data()
            .get(source_start..source_end)
            .ok_or(IrSubmitError::InvalidIr(ir::Error::OutOfBounds))?;
        let destination_start = usize::try_from(u64::from(row) * u64::from(destination_stride))
            .map_err(|_| IrSubmitError::InvalidIr(ir::Error::Overflow))?;
        let destination =
            &mut pixels[destination_start..destination_start + destination_stride as usize];
        match format {
            ir::TextureFormat::Rgba8Unorm => {
                for (source, destination) in
                    source.chunks_exact(4).zip(destination.chunks_exact_mut(4))
                {
                    destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
                }
            }
            ir::TextureFormat::R8Unorm => {
                for (&alpha, destination) in source.iter().zip(destination.chunks_exact_mut(4)) {
                    destination.copy_from_slice(&[0, 0, 0, alpha]);
                }
            }
            _ => unreachable!(),
        }
    }

    Ok(PreparedBgraUpload {
        pixels: Cow::Owned(pixels),
        bytes_per_row: destination_stride,
    })
}

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

fn append_image_resource(
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
    let modifier = if image.layout.modifier == gpu_raw::GPU_IMAGE_MODIFIER_LINEAR {
        codegen::ImageModifier::Linear
    } else {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ImageLayout,
        ));
    };
    metadata.push(codegen::ResourceMeta {
        id,
        size: image.allocation_size(),
        kind: codegen::ResourceKind::Image(codegen::ImageMeta {
            format: descriptor.format(),
            storage_format: match descriptor.format() {
                ir::TextureFormat::Depth32Float => ir::TextureFormat::Depth32Float,
                ir::TextureFormat::Bgra8Unorm
                | ir::TextureFormat::Rgba8Unorm
                | ir::TextureFormat::R8Unorm => ir::TextureFormat::Bgra8Unorm,
                ir::TextureFormat::Bgra8UnormSrgb
                | ir::TextureFormat::Rgba8UnormSrgb => {
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ResourceState,
                    ));
                }
            },
            extent: descriptor.extent(),
            usage,
            modifier,
            planes,
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
        | ir::TextureFormat::R8Unorm => Ok(()),
        ir::TextureFormat::Bgra8UnormSrgb
        | ir::TextureFormat::Rgba8UnormSrgb
        | ir::TextureFormat::Depth32Float => Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::TextureUpload,
        )),
    }
}

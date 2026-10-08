//! Programmable graphics validation and owned draw metadata.
//!
//! Draw blobs carry context attachment tokens and bounded ranges. GPU virtual
//! addresses never cross this userspace/kernel boundary.

use crate::ir::{PrimitiveTopology, TextureFormat, VertexFormat};
use crate::resource::{ContextResources, RawBuffer, RawImage};
use crate::{IrSubmitError, UnsupportedIrFeature, ir};
use alloc::{rc::Rc, sync::Arc, vec, vec::Vec};
use maxwell_program_wire as program_wire;
use sgfx_shader_maxwell::{CompiledShader, ShaderCompileError, compile_shader};

const DRAW_HEADER_SIZE: usize = 256;
const DRAW_RECORD_SIZES: [usize; 6] = [32, 40, 16, 48, 64, 48];

/// Constructs a bounded, contiguous SGMD record without exposing unchecked
/// section offsets to the caller. The kernel uses the same record widths.
fn encode_draw_sections(
    header: &[u32; 64],
    sections: [&[u8]; 6],
    inline: &[u8],
) -> Result<Vec<u8>, crate::IrSubmitError> {
    let mut counts = [0u32; 6];
    let mut offsets = [0u32; 6];
    let mut size = DRAW_HEADER_SIZE;
    for (index, (section, record_size)) in sections
        .iter()
        .zip(DRAW_RECORD_SIZES.iter().copied())
        .enumerate()
    {
        if section.len() % record_size != 0 {
            return Err(crate::IrSubmitError::SubmissionTooLarge);
        }
        counts[index] = u32::try_from(section.len() / record_size)
            .map_err(|_| crate::IrSubmitError::SubmissionTooLarge)?;
        offsets[index] =
            u32::try_from(size).map_err(|_| crate::IrSubmitError::SubmissionTooLarge)?;
        size = size
            .checked_add(section.len())
            .ok_or(crate::IrSubmitError::SubmissionTooLarge)?;
    }
    let inline_offset =
        u32::try_from(size).map_err(|_| crate::IrSubmitError::SubmissionTooLarge)?;
    let inline_size =
        u32::try_from(inline.len()).map_err(|_| crate::IrSubmitError::SubmissionTooLarge)?;
    size = size
        .checked_add(inline.len())
        .filter(|size| *size <= 1024 * 1024)
        .ok_or(crate::IrSubmitError::SubmissionTooLarge)?;
    let mut words = *header;
    words[0] = 0x444d_4753;
    words[1] = 1;
    words[2] = size as u32;
    words[18..24].copy_from_slice(&counts);
    words[24..30].copy_from_slice(&offsets);
    words[30] = inline_offset;
    words[31] = inline_size;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| crate::IrSubmitError::OutOfMemory)?;
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    for section in sections {
        bytes.extend_from_slice(section);
    }
    bytes.extend_from_slice(inline);
    Ok(bytes)
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn compile_error(error: impl core::fmt::Debug) -> IrSubmitError {
    IrSubmitError::ShaderCompile(ShaderCompileError(alloc::format!(
        "Maxwell program validation: {error:?}"
    )))
}

fn constant_bank_size(shader: &CompiledShader) -> Result<u32, IrSubmitError> {
    let mut end = 0;
    let mut include = |register: u32, size: u32| -> Result<(), IrSubmitError> {
        end = end.max(
            register
                .checked_mul(16)
                .and_then(|offset| offset.checked_add(size.next_multiple_of(16)))
                .ok_or(ir::Error::Overflow)?,
        );
        Ok(())
    };
    for binding in &shader.uniform_buffers {
        include(binding.first_register, binding.size)?;
    }
    for binding in &shader.storage_buffers {
        include(binding.first_register, 16)?;
    }
    for query in &shader.image_query_levels {
        include(query.first_register, 16)?;
    }
    if let Some(push) = &shader.push_constants {
        include(push.first_register, push.size)?;
    }
    if let Some(first) = shader.first_instance_register {
        include(first, 16)?;
    }
    if let Some(flags) = shader.srgb_view_flags_register {
        include(
            flags,
            ((shader.storage_buffers.len() + shader.textures.len()) as u32).div_ceil(4) * 16,
        )?;
    }
    if end > 16 * 1024 {
        return Err(IrSubmitError::SubmissionTooLarge);
    }
    Ok(end)
}

fn header_bits(header: &[u32; 20], start: usize, count: usize) -> u32 {
    (0..count).fold(0, |value, index| {
        value | (((header[(start + index) / 32] >> ((start + index) % 32)) & 1) << index)
    })
}

fn encode_program(shader: &CompiledShader) -> Result<Vec<u8>, IrSubmitError> {
    let stage = match shader.stage {
        ir::ShaderStage::Vertex => program_wire::Stage::Vertex,
        ir::ShaderStage::Fragment => program_wire::Stage::Fragment,
        _ => return Err(ir::Error::InvalidDescriptor.into()),
    };
    let mut metadata = program_wire::Metadata::new(stage);
    metadata.gprs = shader.metadata.num_gprs.max(4);
    let h = &shader.header;
    metadata.flags = if header_bits(h, 27, 1) != 0 {
        program_wire::FLAG_FP64
    } else {
        0
    };
    metadata.cb_sizes[0] = constant_bank_size(shader)?;
    for slot in shader
        .storage_buffers
        .iter()
        .map(|binding| binding.slot)
        .chain(shader.textures.iter().map(|binding| binding.slot))
    {
        if slot >= 16 {
            return Err(IrSubmitError::SubmissionTooLarge);
        }
        metadata.resource_mask |= 1u64 << slot;
        metadata.cb_sizes[15] = metadata.cb_sizes[15].max(0x20 + (slot + 1) * 4);
    }
    metadata.sysvals_in_ab = header_bits(h, 160, 32);
    if stage == program_wire::Stage::Vertex {
        metadata.store_req_start = header_bits(h, 140, 8) as u8;
        metadata.store_req_end = header_bits(h, 152, 8) as u8;
        for i in 0..4 {
            metadata.attr_in[i] = header_bits(h, 192 + i * 32, 32);
            metadata.attr_out[i] = header_bits(h, 432 + i * 32, 32);
        }
        metadata.sysvals_in_c = header_bits(h, 336, 16) as u16;
        metadata.sysvals_in_d = header_bits(h, 392, 8) as u8;
        metadata.sysvals_out_ab = header_bits(h, 400, 32);
        metadata.sysvals_out_c = header_bits(h, 576, 16) as u16;
        metadata.sysvals_out_d = header_bits(h, 632, 8) as u8;
    } else {
        if header_bits(h, 15, 1) != 0 {
            metadata.flags |= program_wire::FLAG_KILL;
        }
        if header_bits(h, 608, 1) != 0 {
            metadata.flags |= program_wire::FLAG_SAMPLE_MASK;
        }
        if header_bits(h, 609, 1) != 0 {
            metadata.flags |= program_wire::FLAG_DEPTH;
        }
        for i in 0..128 {
            metadata.fs_inputs[i] = header_bits(h, 192 + i * 2, 2) as u8;
        }
        metadata.sysvals_in_c = header_bits(h, 464, 16) as u16;
        for i in 0..8 {
            metadata.fs_sysvals_d[i] = header_bits(h, 560 + i * 2, 2) as u8;
        }
        metadata.fs_color_mask = header_bits(h, 576, 32);
    }
    let mut code = Vec::new();
    code.try_reserve_exact(shader.code.len() * 4)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for word in &shader.code {
        push_u32(&mut code, *word);
    }
    let program = program_wire::Program {
        metadata,
        code: &code,
    };
    program_wire::validate(
        &program,
        &program_wire::Limits {
            cb_sizes: metadata.cb_sizes,
            resource_mask: metadata.resource_mask,
        },
    )
    .map_err(compile_error)?;
    let mut output = vec![0; program_wire::HEADER_SIZE + code.len()];
    program.encode_into(&mut output).map_err(compile_error)?;
    Ok(output)
}

pub(crate) struct CompiledPipeline {
    pub(crate) topology: ir::PrimitiveTopology,
    pub(crate) vertex: CompiledShader,
    pub(crate) fragment: CompiledShader,
    pub(crate) vertex_package: Vec<u8>,
    pub(crate) fragment_package: Vec<u8>,
    pub(crate) vertex_buffers: Vec<ir::VertexBufferLayout>,
}

impl ContextResources {
    pub(crate) fn compiled_pipeline(
        &self,
        id: ir::ProgrammableRenderPipelineId,
    ) -> Result<Rc<CompiledPipeline>, IrSubmitError> {
        self.resources.programmable_render_pipeline_ref(id)?;
        if let Some((_, compiled)) = self
            .programmable_pipelines
            .borrow()
            .iter()
            .find(|(candidate, _)| *candidate == id)
        {
            return Ok(Rc::clone(compiled));
        }
        let compiled = compile_pipeline(&self.resources, id)?;
        let mut cache = self.programmable_pipelines.borrow_mut();
        cache
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        cache.push((id, Rc::clone(&compiled)));
        Ok(compiled)
    }
}

fn compile_pipeline(
    resources: &ir::ResourceTable,
    id: ir::ProgrammableRenderPipelineId,
) -> Result<Rc<CompiledPipeline>, IrSubmitError> {
    let reference = resources.programmable_render_pipeline_ref(id)?;
    let pipeline = resources.programmable_render_pipeline_shared(reference)?;
    if pipeline.color_targets().any(|target| {
        !matches!(
            target.format(),
            TextureFormat::Bgra8Unorm
                | TextureFormat::Rgba8Unorm
                | TextureFormat::R8Unorm
                | TextureFormat::Rg8Unorm
        )
    }) {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::PipelineTargetFormat,
        ));
    }
    if !matches!(
        pipeline.topology(),
        PrimitiveTopology::TriangleList
            | PrimitiveTopology::TriangleStrip
            | PrimitiveTopology::TriangleFan
    ) {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::PrimitiveTopology,
        ));
    }
    if pipeline.vertex_buffers().len() > 8
        || pipeline
            .vertex_buffers()
            .iter()
            .flat_map(|layout| layout.attributes())
            .any(|a| a.location() >= 16)
    {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::VertexLayout,
        ));
    }
    for group in pipeline.layout().bind_groups() {
        if group.entries().iter().any(|binding| {
            !matches!(
                binding.ty(),
                ir::BindingType::UniformBuffer
                    | ir::BindingType::StorageBuffer { read_only: true }
                    | ir::BindingType::SampledTexture
                    | ir::BindingType::SampledTextureView {
                        dimension: ir::TextureViewDimension::D1
                            | ir::TextureViewDimension::D1Array
                            | ir::TextureViewDimension::D2
                            | ir::TextureViewDimension::D2Array
                            | ir::TextureViewDimension::Cube,
                        depth: _
                    }
                    | ir::BindingType::Sampler
                    | ir::BindingType::ComparisonSampler
            )
        }) {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceBindings,
            ));
        }
    }
    let compile = |entry: &ir::ShaderEntryPoint| {
        let module = resources.shader_module(resources.shader_module_ref(entry.module())?)?;
        compile_shader(&module, entry.stage(), entry.entry_point())
            .map(|shader| {
                (
                    shader,
                    matches!(module.source(), ir::ShaderSource::SpirV(_)),
                )
            })
            .map_err(IrSubmitError::ShaderCompile)
    };
    let (vertex, vertex_spirv) = compile(pipeline.vertex())?;
    let (fragment, fragment_spirv) = compile(pipeline.fragment())?;
    let attributes = || {
        pipeline
            .vertex_buffers()
            .iter()
            .flat_map(|layout| layout.attributes())
    };
    if vertex
        .input_locations
        .iter()
        .any(|location| !attributes().any(|attribute| attribute.location() == *location))
        || fragment
            .input_locations
            .iter()
            .any(|location| !vertex.output_locations.contains(location))
    {
        #[cfg(feature = "std")]
        if std::env::var_os("SGFX_VULKAN_TRACE").is_some() {
            std::eprintln!(
                "[SGFX VirGL] missing pipeline interface: attributes={:?} VS inputs={:?} VS outputs={:?} FS inputs={:?}",
                attributes().collect::<Vec<_>>(),
                vertex.inputs,
                vertex.outputs,
                fragment.inputs
            );
        }
        return Err(ir::Error::InvalidDescriptor.into());
    }
    for input in &vertex.inputs {
        use sgfx_shader_maxwell::IoScalar;
        let attribute = attributes()
            .find(|attribute| attribute.location() == input.location)
            .ok_or(ir::Error::InvalidDescriptor)?;
        let scalar = match attribute.format() {
            VertexFormat::Sint32 | VertexFormat::Sint16x4 => IoScalar::Sint,
            VertexFormat::Uint32 => IoScalar::Uint,
            _ => IoScalar::Float,
        };
        // Vertex fetch supplies a four-component value independently of the
        // shader's vector width. VirGL retains the attribute's actual format,
        // and its host GL fetch fills absent components with (0, 0, 0, 1), as
        // Vulkan requires. A vec2 buffer may therefore feed a vec4 input; excess
        // components are likewise ignored. Scalar interpretation must match.
        if scalar != input.scalar {
            #[cfg(feature = "std")]
            if std::env::var_os("SGFX_VULKAN_TRACE").is_some() {
                std::eprintln!(
                    "[SGFX VirGL] vertex attribute mismatch: shader={input:?} attribute={attribute:?}"
                );
            }
            return Err(ir::Error::InvalidDescriptor.into());
        }
    }
    for input in &fragment.inputs {
        let output = vertex
            .outputs
            .iter()
            .find(|output| output.location == input.location)
            .ok_or(ir::Error::InvalidDescriptor)?;
        // Vulkan/SPIR-V interface matching excludes interpolation decorations:
        // the fragment IN declaration controls TGSI interpolation. Preserve
        // WGSL's stricter cross-stage interpolation matching rule.
        if input.components != output.components
            || input.scalar != output.scalar
            || !(vertex_spirv && fragment_spirv) && input.interpolation != output.interpolation
        {
            #[cfg(feature = "std")]
            if std::env::var_os("SGFX_VULKAN_TRACE").is_some() {
                std::eprintln!(
                    "[SGFX VirGL] stage interface mismatch: vertex={output:?} fragment={input:?}"
                );
            }
            return Err(ir::Error::InvalidDescriptor.into());
        }
    }
    // Each stage is bounded independently and uses VirGL shader continuations.
    if [&vertex, &fragment]
        .iter()
        .any(|shader| shader.tgsi.len() > 256 * 1024)
    {
        return Err(IrSubmitError::SubmissionTooLarge);
    }
    for shader in [&vertex, &fragment] {
        let visibility = match shader.stage {
            ir::ShaderStage::Vertex => ir::ShaderStages::VERTEX,
            ir::ShaderStage::Fragment => ir::ShaderStages::FRAGMENT,
            ir::ShaderStage::Compute => {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ProgrammableExecution,
                ));
            }
        };
        if let Some(push) = &shader.push_constants {
            let ranges = pipeline.layout().push_constant_ranges();
            // Require the declared source block within the bounded stage layout.
            for byte in 0..push.size {
                if !ranges.iter().any(|range| {
                    range.stages().contains(visibility)
                        && byte >= range.offset()
                        && byte < range.offset() + range.size()
                }) {
                    return Err(ir::Error::BindingLayoutMismatch.into());
                }
            }
        }
        for binding in &shader.storage_buffers {
            let layout = pipeline
                .layout()
                .bind_groups()
                .get(binding.group as usize)
                .and_then(|group| {
                    group
                        .entries()
                        .iter()
                        .find(|entry| entry.binding() == binding.binding)
                })
                .ok_or(ir::Error::BindingLayoutMismatch)?;
            if layout.ty() != (ir::BindingType::StorageBuffer { read_only: true })
                || !layout.visibility().contains(visibility)
            {
                return Err(ir::Error::BindingLayoutMismatch.into());
            }
        }
        for binding in &shader.uniform_buffers {
            let layout = pipeline
                .layout()
                .bind_groups()
                .get(binding.group as usize)
                .and_then(|group| {
                    group
                        .entries()
                        .iter()
                        .find(|entry| entry.binding() == binding.binding)
                })
                .ok_or(ir::Error::BindingLayoutMismatch)?;
            if layout.ty() != ir::BindingType::UniformBuffer
                || !layout.visibility().contains(visibility)
                || binding.size > 16 * 1024
                || binding
                    .first_register
                    .checked_add(binding.size.div_ceil(16))
                    .is_none_or(|end| end > 1024)
            {
                return Err(ir::Error::BindingLayoutMismatch.into());
            }
        }
        for query in &shader.image_query_levels {
            let layout = pipeline
                .layout()
                .bind_groups()
                .get(query.group as usize)
                .and_then(|group| {
                    group
                        .entries()
                        .iter()
                        .find(|entry| entry.binding() == query.binding)
                })
                .ok_or(ir::Error::BindingLayoutMismatch)?;
            if !matches!(
                layout.ty(),
                ir::BindingType::SampledTexture | ir::BindingType::SampledTextureView { .. }
            ) || !layout.visibility().contains(visibility)
            {
                return Err(ir::Error::BindingLayoutMismatch.into());
            }
        }
        for pair in &shader.textures {
            for (group, binding, ty) in [
                (
                    pair.image_group,
                    pair.image_binding,
                    ir::BindingType::SampledTextureView {
                        dimension: pair.dimension,
                        depth: pair.depth,
                    },
                ),
                (
                    pair.sampler_group,
                    pair.sampler_binding,
                    if pair.comparison {
                        ir::BindingType::ComparisonSampler
                    } else {
                        ir::BindingType::Sampler
                    },
                ),
            ]
            .into_iter()
            .take(if pair.uses_sampler { 2 } else { 1 })
            {
                let entry = pipeline
                    .layout()
                    .bind_groups()
                    .get(group as usize)
                    .and_then(|group| {
                        group
                            .entries()
                            .iter()
                            .find(|entry| entry.binding() == binding)
                    })
                    .ok_or(ir::Error::BindingLayoutMismatch)?;
                if !(entry.ty() == ty
                    || (entry.ty() == ir::BindingType::SampledTexture
                        && ty
                            == ir::BindingType::SampledTextureView {
                                dimension: ir::TextureViewDimension::D2,
                                depth: false,
                            }))
                    || !entry.visibility().contains(visibility)
                {
                    return Err(ir::Error::BindingLayoutMismatch.into());
                }
            }
        }
    }
    let compiled = Rc::new(CompiledPipeline {
        topology: pipeline.topology(),
        vertex_package: encode_program(&vertex)?,
        fragment_package: encode_program(&fragment)?,
        vertex,
        fragment,
        vertex_buffers: pipeline.vertex_buffers().to_vec(),
    });
    Ok(compiled)
}

/// All data reaching the asynchronous dispatcher owns its native attachments.
/// Uniform buffer contents are read only after earlier queue work retires.
pub(crate) struct PreparedDraw {
    metadata: Arc<RawBuffer>,
    bytes: Vec<u8>,
    patches: Vec<UniformPatch>,
    resources: Vec<maxwell_submit_wire::Resource>,
    private_bytes: usize,
    _buffers: Vec<Arc<RawBuffer>>,
    _images: Vec<Arc<RawImage>>,
}
struct UniformPatch {
    buffer: Arc<RawBuffer>,
    offset: u64,
    destination: usize,
    size: usize,
}
impl PreparedDraw {
    pub(crate) fn budget_bytes(&self) -> usize {
        // Account for the immutable template, the unique native metadata
        // allocation, and the dispatch-time snapshot. Source/program owners
        // share already cached allocations rather than duplicating their data.
        self.bytes
            .len()
            .saturating_mul(3)
            .saturating_add(self.private_bytes)
            .saturating_add(
                self.patches
                    .len()
                    .saturating_mul(core::mem::size_of::<UniformPatch>()),
            )
            .saturating_add(
                (self._buffers.len() + self._images.len())
                    .saturating_mul(core::mem::size_of::<Arc<RawBuffer>>()),
            )
    }
    pub(crate) fn execute(&self) -> crate::HandleResult<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(self.bytes.len())
            .map_err(|_| crate::HandleError::OutOfResources)?;
        bytes.extend_from_slice(&self.bytes);
        patch_uniform_bytes(
            &mut bytes,
            &self.patches,
            |patch| (patch.destination, patch.size),
            |patch, output| patch.buffer.read_into(patch.offset, output),
        )?;
        self.metadata.write(0, &bytes)?;
        let mut words = [0; 64];
        words[0] = 5;
        words[4] = bytes.len() as u32;
        let relocation = maxwell_submit_wire::Relocation {
            commands_word_offset: 2,
            source: maxwell_submit_wire::RelocationSource::Attachment(0),
            resource_offset: 0,
            required_size: bytes.len() as u64,
            access: maxwell_submit_wire::ACCESS_READ,
            encoding: maxwell_submit_wire::AddressEncoding::GpuVa64,
        };
        let submit = maxwell_submit_wire::Submit {
            commands: &words,
            resources: &self.resources,
            relocations: &[relocation],
        };
        let size = maxwell_submit_wire::encoded_len(submit)
            .map_err(|_| crate::HandleError::InvalidParameter)?;
        let mut output = vec![0; size];
        maxwell_submit_wire::encode(submit, &mut output)
            .map_err(|_| crate::HandleError::InvalidParameter)?;
        Ok(output)
    }
}

pub(crate) struct DrawState<'r> {
    pub(crate) pass: Option<ir::RenderPassDesc<'r>>,
    pub(crate) pipeline: Option<ir::ProgrammableRenderPipelineRef<'r>>,
    groups: [Option<ir::BindGroupRef<'r>>; 4],
    vertices: [Option<(ir::BufferRef<'r>, u64)>; 8],
    index: Option<(ir::BufferRef<'r>, u64, ir::IndexFormat)>,
    push: [[u8; 128]; 2],
    scissor: Option<ir::PixelRect>,
    viewport: Option<ir::Viewport>,
    r8_targets: [Option<Arc<RawImage>>; 8],
    initialized: bool,
    private_budgeted: bool,
}
impl<'r> DrawState<'r> {
    pub(crate) fn new() -> Self {
        Self {
            pass: None,
            pipeline: None,
            groups: [None; 4],
            vertices: [None; 8],
            index: None,
            push: [[0; 128]; 2],
            scissor: None,
            viewport: None,
            r8_targets: core::array::from_fn(|_| None),
            initialized: false,
            private_budgeted: false,
        }
    }
    /// Update both shared render state and programmable-only state. No borrowed
    /// command data is kept after preparation completes.
    pub(crate) fn observe(&mut self, command: &ir::Command<'r, '_>) -> Result<bool, IrSubmitError> {
        match command {
            ir::Command::BeginRenderPass(pass) => {
                *self = Self::new();
                self.pass = Some(*pass);
            }
            ir::Command::EndRenderPass => {}
            ir::Command::SetPipeline(_) => self.pipeline = None,
            ir::Command::SetProgrammablePipeline(p) => {
                self.pipeline = Some(*p);
                return Ok(true);
            }
            ir::Command::SetBindGroup { index, bind_group } => {
                *self
                    .groups
                    .get_mut(*index as usize)
                    .ok_or(ir::Error::BindingLayoutMismatch)? = Some(*bind_group);
                return Ok(true);
            }
            ir::Command::SetVertexBuffer { buffer, offset } => {
                self.vertices[0] = Some((*buffer, *offset))
            }
            ir::Command::SetVertexBufferSlot {
                slot,
                buffer,
                offset,
            } => {
                *self
                    .vertices
                    .get_mut(*slot as usize)
                    .ok_or(ir::Error::InvalidValue)? = Some((*buffer, *offset));
                return Ok(*slot != 0);
            }
            ir::Command::SetIndexBuffer {
                buffer,
                offset,
                format,
            } => self.index = Some((*buffer, *offset, *format)),
            ir::Command::SetScissor(s) => self.scissor = *s,
            ir::Command::SetViewport(v) => {
                self.viewport = Some(*v);
                return Ok(false);
            }
            ir::Command::SetPushConstants {
                stages,
                offset,
                data,
            } => {
                if !(ir::ShaderStages::VERTEX | ir::ShaderStages::FRAGMENT).contains(*stages) {
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ProgrammableExecution,
                    ));
                }
                let start = *offset as usize;
                let end = start.checked_add(data.len()).ok_or(ir::Error::Overflow)?;
                if end > 128 {
                    return Err(ir::Error::OutOfBounds.into());
                }
                for (stage, mask) in [ir::ShaderStages::VERTEX, ir::ShaderStages::FRAGMENT]
                    .into_iter()
                    .enumerate()
                {
                    if stages.contains(mask) {
                        self.push[stage][start..end].copy_from_slice(data);
                    }
                }
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }
    pub(crate) fn draw(
        &self,
        command: &ir::Command<'r, '_>,
    ) -> Option<(u32, u32, Option<i32>, u32, u32)> {
        if self.pipeline.is_none() {
            return None;
        }
        match command {
            ir::Command::Draw {
                vertex_count,
                first_vertex,
            } => Some((*first_vertex, *vertex_count, None, 1, 0)),
            ir::Command::DrawIndexed {
                index_count,
                first_index,
                base_vertex,
            } => Some((*first_index, *index_count, Some(*base_vertex), 1, 0)),
            ir::Command::DrawInstanced {
                vertex_count,
                first_vertex,
                instance_count,
                first_instance,
            } => Some((
                *first_vertex,
                *vertex_count,
                None,
                *instance_count,
                *first_instance,
            )),
            ir::Command::DrawIndexedInstanced {
                index_count,
                first_index,
                base_vertex,
                instance_count,
                first_instance,
            } => Some((
                *first_index,
                *index_count,
                Some(*base_vertex),
                *instance_count,
                *first_instance,
            )),
            _ => None,
        }
    }
}

fn write_word(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn append_range(bytes: &mut Vec<u8>, buffer: &RawBuffer, offset: u64, size: u64) {
    push_u64(bytes, buffer.attachment_token);
    push_u64(bytes, offset);
    push_u64(bytes, size);
}
fn vertex_format(format: ir::VertexFormat) -> u32 {
    match format {
        ir::VertexFormat::Float32x2 => 1,
        ir::VertexFormat::Float32x3 => 2,
        ir::VertexFormat::Float32x4 => 3,
        ir::VertexFormat::Uint32 => 4,
        ir::VertexFormat::Sint32 => 8,
        ir::VertexFormat::Unorm8x4 => 12,
        ir::VertexFormat::Float16x2 => 18,
        ir::VertexFormat::Float16x4 => 19,
        ir::VertexFormat::Sint16x4 => 20,
        ir::VertexFormat::Snorm10_10_10_2 => 21,
    }
}
fn compare(compare: ir::CompareFunction) -> u32 {
    match compare {
        ir::CompareFunction::Never => 0,
        ir::CompareFunction::Less => 1,
        ir::CompareFunction::Equal => 2,
        ir::CompareFunction::LessEqual => 3,
        ir::CompareFunction::Greater => 4,
        ir::CompareFunction::NotEqual => 5,
        ir::CompareFunction::GreaterEqual => 6,
        ir::CompareFunction::Always => 7,
    }
}
fn blend_factor(factor: ir::BlendFactor) -> u32 {
    match factor {
        ir::BlendFactor::Zero => 0,
        ir::BlendFactor::One => 1,
        ir::BlendFactor::SourceAlpha => 6,
        ir::BlendFactor::OneMinusSourceAlpha => 7,
        ir::BlendFactor::DestinationAlpha => 8,
        ir::BlendFactor::OneMinusDestinationAlpha => 9,
    }
}
fn blend_op(op: ir::BlendOp) -> u32 {
    match op {
        ir::BlendOp::Add => 0,
        ir::BlendOp::Subtract => 1,
        ir::BlendOp::ReverseSubtract => 2,
    }
}
fn sampler_flags(s: ir::SamplerDesc) -> u32 {
    let wrap = |v| match v {
        ir::AddressMode::ClampToEdge => 0,
        ir::AddressMode::Repeat => 1,
        ir::AddressMode::MirrorRepeat => 2,
    };
    u32::from(s.mag_filter() == ir::FilterMode::Linear)
        | u32::from(s.min_filter() == ir::FilterMode::Linear) << 1
        | u32::from(s.mip_filter() == ir::FilterMode::Linear) << 2
        | wrap(s.address_u()) << 3
        | wrap(s.address_v()) << 5
        | s.compare().map_or(0, |v| (1 << 9) | (compare(v) << 10))
}

/// SGMD samples ordinary color/depth attachments. Shared multi-plane NV12
/// images belong to the fixed YCbCr path and have no programmable descriptor.
fn programmable_sample_format(format: ir::TextureFormat) -> Result<u32, IrSubmitError> {
    match format {
        ir::TextureFormat::Bgra8Unorm
        | ir::TextureFormat::Rgba8Unorm
        | ir::TextureFormat::Bgra8UnormSrgb
        | ir::TextureFormat::Rgba8UnormSrgb => Ok(0),
        ir::TextureFormat::R8Unorm => Ok(2),
        ir::TextureFormat::Rg8Unorm => Ok(3),
        ir::TextureFormat::Depth32Float => Ok(4),
        ir::TextureFormat::Nv12 => Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ImageLayout,
        )),
    }
}

impl ContextResources {
    pub(crate) fn prepare_programmable_draw(
        &mut self,
        state: &DrawState<'_>,
        first: u32,
        count: u32,
        base: Option<i32>,
        instances: u32,
        first_instance: u32,
    ) -> Result<PreparedDraw, IrSubmitError> {
        let table = Rc::clone(&self.resources);
        let reference = state.pipeline.ok_or(ir::Error::PipelineNotSet)?;
        let descriptor = table.programmable_render_pipeline_shared(reference)?;
        let pipeline = self.compiled_pipeline(reference.id())?;
        let pass = state.pass.ok_or(ir::Error::InvalidDescriptor)?;
        if count == 0
            || instances == 0
            || first.checked_add(count).is_none()
            || first_instance.checked_add(instances).is_none()
        {
            return Err(ir::Error::InvalidValue.into());
        }
        let mut sections: [Vec<u8>; 6] = core::array::from_fn(|_| Vec::new());
        let mut inline = Vec::new();
        let mut patches = Vec::new();
        let mut buffers = Vec::new();
        let mut images = Vec::new();
        let mut header = [0; 64];
        header[4] = match pipeline.topology {
            ir::PrimitiveTopology::TriangleList => 0,
            ir::PrimitiveTopology::TriangleStrip => 1,
            ir::PrimitiveTopology::TriangleFan => 2,
        };
        header[5] = count;
        header[6] = instances;
        header[7] = first;
        header[8] = base.unwrap_or(0) as u32;
        header[9] = first_instance;
        let uploaded = if let Some((_, vertex, fragment)) = self
            .programmable_programs
            .iter()
            .find(|(id, _, _)| *id == reference.id())
        {
            [Arc::clone(vertex), Arc::clone(fragment)]
        } else {
            let vertex = Arc::new(RawBuffer::create(
                &self.context,
                pipeline.vertex_package.len() as u64,
            )?);
            vertex.write(0, &pipeline.vertex_package)?;
            let fragment = Arc::new(RawBuffer::create(
                &self.context,
                pipeline.fragment_package.len() as u64,
            )?);
            fragment.write(0, &pipeline.fragment_package)?;
            self.programmable_programs
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            self.programmable_programs.push((
                reference.id(),
                Arc::clone(&vertex),
                Arc::clone(&fragment),
            ));
            [vertex, fragment]
        };
        for (stage, buffer) in [0, 4].into_iter().zip(&uploaded) {
            push_u32(&mut sections[0], stage);
            push_u32(&mut sections[0], 0);
            append_range(&mut sections[0], buffer, 0, buffer.logical_size);
            buffers.push(Arc::clone(buffer));
        }
        for (slot, layout) in pipeline.vertex_buffers.iter().enumerate() {
            let (reference, offset) = state.vertices[slot].ok_or(ir::Error::VertexBufferNotSet)?;
            let desc = table.buffer(reference)?;
            if !desc.usage().contains(ir::BufferUsage::VERTEX) || offset >= desc.size() {
                return Err(ir::Error::InvalidUsage.into());
            }
            let buffer = Arc::clone(self.buffer(reference)?);
            append_range(&mut sections[1], &buffer, offset, desc.size() - offset);
            push_u32(&mut sections[1], layout.stride());
            push_u32(&mut sections[1], 0);
            push_u64(&mut sections[1], 0);
            for attr in layout.attributes() {
                for word in [
                    attr.location(),
                    slot as u32,
                    attr.offset(),
                    vertex_format(attr.format()),
                ] {
                    push_u32(&mut sections[2], word);
                }
            }
            if base.is_none() {
                let end = u64::from(first + count - 1)
                    .checked_mul(u64::from(layout.stride()))
                    .and_then(|n| {
                        n.checked_add(
                            layout
                                .attributes()
                                .iter()
                                .map(|a| u64::from(a.offset() + a.format().byte_size()))
                                .max()
                                .unwrap_or(0),
                        )
                    })
                    .ok_or(ir::Error::Overflow)?;
                if end > desc.size() - offset {
                    return Err(ir::Error::OutOfBounds.into());
                }
            }
            buffers.push(buffer);
        }
        if let Some(_) = base {
            let (reference, offset, format) = state.index.ok_or(ir::Error::IndexBufferNotSet)?;
            let desc = table.buffer(reference)?;
            if !desc.usage().contains(ir::BufferUsage::INDEX)
                || offset >= desc.size()
                || offset % format.byte_size() != 0
                || u64::from(first + count) * format.byte_size() > desc.size() - offset
            {
                return Err(ir::Error::OutOfBounds.into());
            }
            let buffer = Arc::clone(self.buffer(reference)?);
            header[11] = if format == ir::IndexFormat::Uint16 {
                1
            } else {
                2
            };
            for (i, v) in [buffer.attachment_token, offset, desc.size() - offset]
                .into_iter()
                .enumerate()
            {
                header[12 + i * 2] = v as u32;
                header[13 + i * 2] = (v >> 32) as u32;
            }
            buffers.push(buffer);
        }
        let resource = |group: u32, binding: u32| -> Result<ir::BindingResource, IrSubmitError> {
            let group_index = group as usize;
            let group = table.bind_group_shared(
                state
                    .groups
                    .get(group_index)
                    .copied()
                    .flatten()
                    .ok_or(ir::Error::BindingLayoutMismatch)?,
            )?;
            if descriptor.layout().bind_groups().get(group_index) != Some(group.layout()) {
                return Err(ir::Error::BindingLayoutMismatch.into());
            }
            group
                .entries()
                .iter()
                .find(|e| e.binding() == binding)
                .map(|e| e.resource())
                .ok_or_else(|| ir::Error::BindingLayoutMismatch.into())
        };
        for (stage_index, (stage, shader)) in [(0, &pipeline.vertex), (4, &pipeline.fragment)]
            .into_iter()
            .enumerate()
        {
            let bank_start = inline.len();
            let bank_size = constant_bank_size(shader)? as usize;
            inline.resize(bank_start + bank_size, 0);
            let mut put = |reg: u32, data: &[u8]| -> Result<(), IrSubmitError> {
                let start = bank_start + reg as usize * 16;
                let end = start.checked_add(data.len()).ok_or(ir::Error::Overflow)?;
                inline
                    .get_mut(start..end)
                    .ok_or(ir::Error::OutOfBounds)?
                    .copy_from_slice(data);
                Ok(())
            };
            if let Some(reg) = shader.first_instance_register {
                put(reg, &first_instance.to_le_bytes())?;
            }
            if let Some(push) = &shader.push_constants {
                put(
                    push.first_register,
                    &state.push[stage_index][..push.size as usize],
                )?;
            }
            for query in &shader.image_query_levels {
                let (texture, levels) = match resource(query.group, query.binding)? {
                    ir::BindingResource::Texture(id) => (id, None),
                    ir::BindingResource::TextureView { texture, view } => {
                        (texture, Some(view.mip_level_count()))
                    }
                    _ => return Err(ir::Error::BindingLayoutMismatch.into()),
                };
                let desc = table.texture(table.texture_ref(texture)?)?;
                put(
                    query.first_register,
                    &levels.unwrap_or(desc.mip_level_count()).to_le_bytes(),
                )?;
            }
            let mut srgb = [0u8; 64];
            let mut aux_size = 0;
            for binding in &shader.storage_buffers {
                let ir::BindingResource::Buffer {
                    buffer,
                    offset,
                    size,
                } = resource(binding.group, binding.binding)?
                else {
                    return Err(ir::Error::BindingLayoutMismatch.into());
                };
                let reference = table.buffer_ref(buffer)?;
                let desc = table.buffer(reference)?;
                if !desc.usage().contains(ir::BufferUsage::STORAGE)
                    || size == 0
                    || size > 256 * 1024
                    || offset % 4 != 0
                    || size % 4 != 0
                    || offset.checked_add(size).is_none_or(|n| n > desc.size())
                    || offset > u64::from(u32::MAX)
                {
                    return Err(ir::Error::InvalidDescriptor.into());
                }
                let buffer = Arc::clone(self.buffer(reference)?);
                let mut record = [0u8; 64];
                record[0..8].copy_from_slice(&buffer.attachment_token.to_le_bytes());
                write_word(&mut record, 8, offset as u32);
                write_word(&mut record, 12, size as u32);
                write_word(&mut record, 20, 1);
                write_word(&mut record, 24, 1 << 15);
                write_word(&mut record, 56, stage);
                write_word(&mut record, 60, binding.slot);
                sections[4].extend_from_slice(&record);
                // The kernel rebases the R32_UINT view to this descriptor range.
                let mut words = [0; 16];
                words[4..8].copy_from_slice(&(size as u32).to_le_bytes());
                put(binding.first_register, &words)?;
                aux_size = aux_size.max(0x20 + (binding.slot + 1) * 4);
                buffers.push(buffer);
            }
            for binding in &shader.textures {
                let (texture, view) = match resource(binding.image_group, binding.image_binding)? {
                    ir::BindingResource::Texture(id) => (id, None),
                    ir::BindingResource::TextureView { texture, view } => (texture, Some(view)),
                    _ => return Err(ir::Error::BindingLayoutMismatch.into()),
                };
                let reference = table.texture_ref(texture)?;
                let desc = table.texture(reference)?;
                programmable_sample_format(desc.format())?;
                if !desc.usage().contains(ir::TextureUsage::SAMPLED)
                    || pass
                        .color_attachments()
                        .any(|attachment| attachment.target() == reference)
                    || pass
                        .depth_attachment()
                        .is_some_and(|depth| depth.target() == reference && !depth.read_only())
                {
                    return Err(ir::Error::InvalidUsage.into());
                }
                let dimension =
                    view.map(|view| view.dimension())
                        .unwrap_or(if desc.dimension_1d() {
                            if desc.array_layer_count() > 1 {
                                ir::TextureViewDimension::D1Array
                            } else {
                                ir::TextureViewDimension::D1
                            }
                        } else if desc.cube_compatible() {
                            ir::TextureViewDimension::Cube
                        } else if desc.array_layer_count() > 1 {
                            ir::TextureViewDimension::D2Array
                        } else {
                            ir::TextureViewDimension::D2
                        });
                if dimension != binding.dimension
                    || binding.depth != (desc.format() == ir::TextureFormat::Depth32Float)
                {
                    return Err(ir::Error::BindingLayoutMismatch.into());
                }
                let format = view.map(|v| v.format()).unwrap_or(desc.format());
                if !desc.format().view_compatible(format) {
                    return Err(ir::Error::BindingLayoutMismatch.into());
                }
                let logical_format = programmable_sample_format(format)?;
                // Logical sRGB conversion is emitted by the common shader frontend.
                write_word(
                    &mut srgb,
                    binding.slot as usize * 4,
                    u32::from(matches!(
                        format,
                        ir::TextureFormat::Bgra8UnormSrgb | ir::TextureFormat::Rgba8UnormSrgb
                    )),
                );
                let sampler = if binding.uses_sampler {
                    let ir::BindingResource::Sampler(id) =
                        resource(binding.sampler_group, binding.sampler_binding)?
                    else {
                        return Err(ir::Error::BindingLayoutMismatch.into());
                    };
                    table.sampler(table.sampler_ref(id)?)?
                } else {
                    ir::SamplerDesc::new(
                        ir::FilterMode::Nearest,
                        ir::FilterMode::Nearest,
                        ir::AddressMode::ClampToEdge,
                        ir::AddressMode::ClampToEdge,
                    )
                };
                if sampler.compare().is_some() != binding.comparison {
                    return Err(ir::Error::BindingLayoutMismatch.into());
                }
                let image = self.texture(reference)?;
                let mut record = [0; 64];
                record[..8].copy_from_slice(&image.attachment_token.to_le_bytes());
                for (offset, value) in [
                    (8, view.map_or(0, |v| v.base_mip_level())),
                    (
                        12,
                        view.map_or(desc.mip_level_count(), |v| v.mip_level_count()),
                    ),
                    (16, view.map_or(0, |v| v.base_array_layer())),
                    (
                        20,
                        view.map_or(desc.array_layer_count(), |v| v.array_layer_count()),
                    ),
                    (
                        24,
                        sampler_flags(sampler)
                            | (logical_format << 16)
                            | ((match dimension {
                                ir::TextureViewDimension::D2 => 0,
                                ir::TextureViewDimension::D1 => 1,
                                ir::TextureViewDimension::D1Array => 2,
                                ir::TextureViewDimension::D2Array => 3,
                                ir::TextureViewDimension::Cube => 4,
                            }) << 20),
                    ),
                    (28, sampler.min_lod().max(0.).min(15.).to_bits()),
                    (32, sampler.max_lod().max(0.).min(15.).to_bits()),
                    (56, stage),
                    (60, binding.slot),
                ] {
                    write_word(&mut record, offset, value);
                }
                sections[4].extend_from_slice(&record);
                images.push(image);
                aux_size = aux_size.max(0x20 + (binding.slot + 1) * 4);
            }
            if let Some(reg) = shader.srgb_view_flags_register {
                put(
                    reg,
                    &srgb
                        [..(shader.storage_buffers.len() + shader.textures.len()).div_ceil(4) * 16],
                )?;
            }
            drop(put);
            for binding in &shader.uniform_buffers {
                let ir::BindingResource::Buffer {
                    buffer,
                    offset,
                    size,
                } = resource(binding.group, binding.binding)?
                else {
                    return Err(ir::Error::BindingLayoutMismatch.into());
                };
                let reference = table.buffer_ref(buffer)?;
                let desc = table.buffer(reference)?;
                if !desc.usage().contains(ir::BufferUsage::UNIFORM)
                    || binding.required_size > binding.size
                    || size < u64::from(binding.required_size)
                    || size > 16 * 1024
                    || offset.checked_add(size).is_none_or(|n| n > desc.size())
                    || offset % 4 != 0
                {
                    return Err(ir::Error::OutOfBounds.into());
                }
                let buffer = Arc::clone(self.buffer(reference)?);
                patches.push(UniformPatch {
                    buffer: Arc::clone(&buffer),
                    offset,
                    destination: bank_start + binding.first_register as usize * 16,
                    size: binding.required_size as usize,
                });
                buffers.push(buffer);
            }
            if bank_size > 0 {
                for value in [stage, 0] {
                    push_u32(&mut sections[3], value);
                }
                sections[3].extend_from_slice(&[0; 24]);
                push_u32(&mut sections[3], bank_start as u32);
                push_u32(&mut sections[3], bank_size as u32);
                push_u64(&mut sections[3], 0);
            }
            // The kernel initializes base-vertex/instance data and all 16
            // resource handle slots (0x20..0x60), including unbound slots.
            if aux_size > 0 {
                let aux_size = aux_size.max(128);
                let start = inline.len();
                inline.resize(start + aux_size as usize, 0);
                for value in [stage, 15] {
                    push_u32(&mut sections[3], value);
                }
                sections[3].extend_from_slice(&[0; 24]);
                push_u32(&mut sections[3], start as u32);
                push_u32(&mut sections[3], aux_size);
                push_u64(&mut sections[3], 0);
            }
        }
        let targets: Vec<_> = descriptor.color_targets().collect();
        let attachments: Vec<_> = pass.color_attachments().collect();
        if targets.len() != attachments.len() {
            return Err(ir::Error::InvalidDescriptor.into());
        }
        for (slot, (target, attachment)) in targets.into_iter().zip(attachments).enumerate() {
            let desc = table.texture(attachment.target())?;
            if desc.format() != target.format() {
                return Err(ir::Error::InvalidDescriptor.into());
            }
            let image = state.r8_targets[slot]
                .as_ref()
                .cloned()
                .unwrap_or(self.texture(attachment.target())?);
            let blend = target.blend();
            let mask = u32::from(target.write_mask().bits())
                & match target.format() {
                    ir::TextureFormat::R8Unorm => 1,
                    ir::TextureFormat::Rg8Unorm => 3,
                    _ => 15,
                };
            push_u64(&mut sections[5], image.attachment_token);
            for word in [
                0,
                0,
                u32::from(blend != ir::BlendState::REPLACE),
                mask,
                blend_factor(blend.color().source_factor()),
                blend_factor(blend.color().destination_factor()),
                blend_op(blend.color().operation()),
                blend_factor(blend.alpha().source_factor()),
                blend_factor(blend.alpha().destination_factor()),
                blend_op(blend.alpha().operation()),
            ] {
                push_u32(&mut sections[5], word);
            }
            images.push(image);
        }
        let area = pass.area();
        let viewport = state.viewport.map(ir::Viewport::components).unwrap_or([
            area.x() as f32,
            area.y() as f32,
            area.width() as f32,
            area.height() as f32,
            0.,
            1.,
        ]);
        for (i, value) in viewport.into_iter().enumerate() {
            header[32 + i] = value.to_bits();
        }
        let scissor = state.scissor.unwrap_or(area);
        header[38..42].copy_from_slice(&[
            scissor.x(),
            scissor.y(),
            scissor.width(),
            scissor.height(),
        ]);
        let raster = descriptor.raster();
        header[42] = u32::from(raster.front_face() == ir::FrontFace::Clockwise)
            | (match raster.cull_mode() {
                ir::CullMode::None => 0,
                ir::CullMode::Front => 1,
                ir::CullMode::Back => 2,
            } << 1);
        if let Some(depth) = descriptor.depth_state() {
            let attachment = pass
                .depth_attachment()
                .ok_or(ir::Error::InvalidDescriptor)?;
            if depth.write_enabled() && attachment.read_only() {
                return Err(ir::Error::InvalidUsage.into());
            }
            let image = self.texture(attachment.target())?;
            header[43] =
                1 | (u32::from(depth.write_enabled()) << 1) | (compare(depth.compare()) << 2);
            header[48] = image.attachment_token as u32;
            header[49] = (image.attachment_token >> 32) as u32;
            images.push(image);
        }
        let slices = core::array::from_fn(|index| sections[index].as_slice());
        let bytes = encode_draw_sections(&header, slices, &inline)?;
        program_wire::draw::Draw::parse(&bytes).map_err(compile_error)?;
        let inline_offset = u32::from_le_bytes(bytes[120..124].try_into().unwrap()) as usize;
        for patch in &mut patches {
            patch.destination += inline_offset;
        }
        let metadata = Arc::new(RawBuffer::create(&self.context, bytes.len() as u64)?);
        let mut resources = Vec::new();
        resources
            .try_reserve(64)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        resources.push(maxwell_submit_wire::Resource {
            attachment_token: metadata.attachment_token,
            range_offset: 0,
            range_size: bytes.len() as u64,
            access: maxwell_submit_wire::ACCESS_READ,
        });
        let draw = program_wire::draw::Draw::parse(&bytes).map_err(compile_error)?;
        for index in 0..draw.program_count() {
            let range = draw.program(index).unwrap().range;
            declare_range(
                &mut resources,
                range.token,
                range.offset,
                range.size,
                maxwell_submit_wire::ACCESS_READ,
            )?;
        }
        for index in 0..draw.stream_count() {
            let range = draw.stream(index).unwrap().range;
            declare_range(
                &mut resources,
                range.token,
                range.offset,
                range.size,
                maxwell_submit_wire::ACCESS_READ,
            )?;
        }
        if draw.state.index_format != 0 {
            let range = draw.state.index;
            declare_range(
                &mut resources,
                range.token,
                range.offset,
                range.size,
                maxwell_submit_wire::ACCESS_READ,
            )?;
        }
        for index in 0..draw.image_count() {
            let image = draw.image(index).unwrap();
            if image.sampler & (1 << 15) != 0 {
                declare_range(
                    &mut resources,
                    image.token,
                    u64::from(image.base_level),
                    u64::from(image.level_count),
                    maxwell_submit_wire::ACCESS_READ,
                )?;
            } else {
                let owner = images
                    .iter()
                    .find(|owner| owner.attachment_token == image.token)
                    .ok_or(ir::Error::InvalidDescriptor)?;
                declare_range(
                    &mut resources,
                    image.token,
                    0,
                    owner.allocation_size(),
                    maxwell_submit_wire::ACCESS_READ,
                )?;
            }
        }
        for index in 0..draw.target_count() {
            let target = draw.target(index).unwrap();
            let owner = images
                .iter()
                .find(|owner| owner.attachment_token == target.token)
                .ok_or(ir::Error::InvalidDescriptor)?;
            declare_range(
                &mut resources,
                target.token,
                0,
                owner.allocation_size(),
                maxwell_submit_wire::ACCESS_READ | maxwell_submit_wire::ACCESS_WRITE,
            )?;
        }
        if draw.state.depth_token != 0 {
            let owner = images
                .iter()
                .find(|owner| owner.attachment_token == draw.state.depth_token)
                .ok_or(ir::Error::InvalidDescriptor)?;
            declare_range(
                &mut resources,
                owner.attachment_token,
                0,
                owner.allocation_size(),
                maxwell_submit_wire::ACCESS_READ
                    | if draw.state.depth & 2 != 0 {
                        maxwell_submit_wire::ACCESS_WRITE
                    } else {
                        0
                    },
            )?;
        }
        Ok(PreparedDraw {
            metadata,
            bytes,
            patches,
            resources,
            private_bytes: 0,
            _buffers: buffers,
            _images: images,
        })
    }
}

pub(crate) fn validate_negotiated_commands(
    context: &crate::ContextInner,
    commands: &ir::CommandBuffer<'_, '_>,
) -> Result<(), IrSubmitError> {
    let mut legacy_fragment = None;
    let mut legacy_texture = None;
    for command in commands.commands() {
        if !context.device.capabilities.supports_programmable_graphics() {
            match command {
                ir::Command::WriteTexture { write, .. }
                    if write.mip_level() != 0 || write.array_layer() != 0 =>
                {
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ImageLayout,
                    ));
                }
                ir::Command::BlitTexture { .. } => {
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ImageLayout,
                    ));
                }
                ir::Command::CopyTextureToTexture {
                    source,
                    destination,
                    ..
                } => {
                    let source = commands.resources().texture(*source)?;
                    let destination = commands.resources().texture(*destination)?;
                    let full_color = |format| {
                        matches!(
                            format,
                            ir::TextureFormat::Bgra8Unorm | ir::TextureFormat::Rgba8Unorm
                        )
                    };
                    if !(full_color(source.format()) && full_color(destination.format())
                        || source.format() == destination.format())
                    {
                        return Err(IrSubmitError::Unsupported(
                            UnsupportedIrFeature::ImageLayout,
                        ));
                    }
                }
                ir::Command::SetPipeline(reference) => {
                    let descriptor = commands.resources().render_pipeline(*reference)?;
                    legacy_fragment = Some(descriptor.fragment());
                    if !matches!(
                        descriptor.blend(),
                        ir::BlendState::REPLACE | ir::BlendState::SOURCE_OVER_STRAIGHT_ALPHA
                    ) {
                        return Err(IrSubmitError::Unsupported(
                            UnsupportedIrFeature::ProgrammableExecution,
                        ));
                    }
                }
                ir::Command::BeginRenderPass(_) => {
                    legacy_fragment = None;
                    legacy_texture = None;
                }
                ir::Command::SetTexture(reference) => {
                    legacy_texture = Some(commands.resources().texture(*reference)?);
                }
                ir::Command::SetSampler(reference) => {
                    let sampler = commands.resources().sampler(*reference)?;
                    if sampler.min_filter() != sampler.mag_filter()
                        || sampler.address_u() != ir::AddressMode::ClampToEdge
                        || sampler.address_v() != ir::AddressMode::ClampToEdge
                        || sampler.mip_filter() != ir::FilterMode::Nearest
                        || sampler.min_lod() != 0.
                        || sampler.max_lod() != 0.
                        || sampler.compare().is_some()
                    {
                        return Err(IrSubmitError::Unsupported(
                            UnsupportedIrFeature::ResourceBindings,
                        ));
                    }
                }
                ir::Command::Draw { .. } | ir::Command::DrawIndexed { .. } => {
                    if let Some(
                        ir::FragmentProgram::Texture(mode)
                        | ir::FragmentProgram::TextureVertexColor(mode),
                    ) = legacy_fragment
                    {
                        let descriptor = legacy_texture.ok_or(ir::Error::InvalidDescriptor)?;
                        if descriptor.mip_level_count() != 1
                            || descriptor.array_layer_count() != 1
                            || descriptor.dimension_1d()
                            || !(matches!(
                                descriptor.format(),
                                ir::TextureFormat::Bgra8Unorm
                                    | ir::TextureFormat::Rgba8Unorm
                                    | ir::TextureFormat::Nv12
                            ) || descriptor.format() == ir::TextureFormat::R8Unorm
                                && mode == ir::TextureSampleMode::AlphaMask)
                        {
                            return Err(IrSubmitError::Unsupported(
                                UnsupportedIrFeature::ResourceBindings,
                            ));
                        }
                    }
                }
                _ => {}
            }
            if let ir::Command::BeginRenderPass(pass) = command {
                if pass.color_attachments().any(|attachment| {
                    commands
                        .resources()
                        .texture(attachment.target())
                        .is_ok_and(|texture| {
                            matches!(
                                texture.format(),
                                ir::TextureFormat::R8Unorm | ir::TextureFormat::Rg8Unorm
                            )
                        })
                }) {
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::PipelineTargetFormat,
                    ));
                }
            }
        }
        if matches!(
            command,
            ir::Command::BeginComputePass
                | ir::Command::EndComputePass
                | ir::Command::SetComputePipeline(_)
                | ir::Command::Dispatch { .. }
        ) {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ProgrammableExecution,
            ));
        }
        if !context.device.capabilities.supports_programmable_graphics()
            && matches!(
                command,
                ir::Command::SetProgrammablePipeline(_)
                    | ir::Command::SetBindGroup { .. }
                    | ir::Command::SetPushConstants { .. }
                    | ir::Command::SetVertexBufferSlot { .. }
                    | ir::Command::SetViewport(_)
                    | ir::Command::DrawInstanced { .. }
                    | ir::Command::DrawIndexedInstanced { .. }
            )
        {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ProgrammableExecution,
            ));
        }
        if let ir::Command::ResourceBarrier(barrier) = command {
            let invalid = match barrier {
                ir::ResourceBarrier::Buffer { before, after, .. } => [before, after]
                    .iter()
                    .any(|access| matches!(access, ir::BufferAccess::StorageReadWrite)),
                ir::ResourceBarrier::Texture { before, after, .. }
                | ir::ResourceBarrier::TextureMip { before, after, .. } => [before, after]
                    .iter()
                    .any(|access| matches!(access, ir::TextureAccess::StorageWrite)),
            };
            if invalid {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ResourceBindings,
                ));
            }
        }
    }
    Ok(())
}

fn declare_range(
    resources: &mut Vec<maxwell_submit_wire::Resource>,
    token: u64,
    offset: u64,
    size: u64,
    access: u32,
) -> Result<(), IrSubmitError> {
    let end = offset.checked_add(size).ok_or(ir::Error::Overflow)?;
    if token == 0 || size == 0 {
        return Err(ir::Error::InvalidValue.into());
    }
    if let Some(resource) = resources.iter_mut().find(|r| r.attachment_token == token) {
        let old_end = resource
            .range_offset
            .checked_add(resource.range_size)
            .ok_or(ir::Error::Overflow)?;
        resource.range_offset = resource.range_offset.min(offset);
        resource.range_size = old_end.max(end) - resource.range_offset;
        resource.access |= access;
    } else {
        resources
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        resources.push(maxwell_submit_wire::Resource {
            attachment_token: token,
            range_offset: offset,
            range_size: size,
            access,
        });
    }
    Ok(())
}

fn compile_conversion_shaders(kind: u8) -> Result<(CompiledShader, CompiledShader), IrSubmitError> {
    let (vertex, mut fragment) = if kind >= 3 {
        sgfx_shader_maxwell::compile_r8_blit_conversion(kind == 4)
    } else {
        sgfx_shader_maxwell::compile_r8_conversion(kind == 1)
    }
    .map_err(IrSubmitError::ShaderCompile)?;
    if kind == 2 || kind == 5 {
        let module=ir::ShaderModuleDesc::wgsl(alloc::string::String::from(if kind==2 {"@group(0) @binding(0) var source:texture_2d<f32>; @fragment fn main(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32>{let c=textureLoad(source,vec2<i32>(p.xy),0);return vec4<f32>(c.r,c.g,0.0,1.0);}"}else{"struct Mapping{destination:vec4<f32>,source_rect:vec4<f32>} @group(0) @binding(0) var source:texture_2d<f32>; @group(0) @binding(1) var source_sampler:sampler; @group(0) @binding(2) var<uniform> mapping:Mapping; @fragment fn main(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32>{let unit=(p.xy-mapping.destination.xy)/mapping.destination.zw;let uv=mapping.source_rect.xy+unit*mapping.source_rect.zw;let c=textureSampleLevel(source,source_sampler,uv,0.0);return vec4<f32>(c.r,c.g,0.0,1.0);}"})).map_err(IrSubmitError::InvalidIr)?;
        fragment = compile_shader(&module, ir::ShaderStage::Fragment, "main")
            .map_err(IrSubmitError::ShaderCompile)?;
    }
    Ok((vertex, fragment))
}

pub(crate) struct ConversionPrograms {
    vertex: Arc<RawBuffer>,
    fragment: Arc<RawBuffer>,
    cb0: [u32; 2],
    cb15: u32,
}
impl ContextResources {
    fn conversion_programs(&mut self, kind: u8) -> Result<Arc<ConversionPrograms>, IrSubmitError> {
        if let Some((_, programs)) = self
            .conversion_programs
            .iter()
            .find(|(key, _)| *key == kind)
        {
            return Ok(Arc::clone(programs));
        }
        let (vertex, fragment) = compile_conversion_shaders(kind)?;
        let v = encode_program(&vertex)?;
        let f = encode_program(&fragment)?;
        let vb = Arc::new(RawBuffer::create(&self.context, v.len() as u64)?);
        vb.write(0, &v)?;
        let fb = Arc::new(RawBuffer::create(&self.context, f.len() as u64)?);
        fb.write(0, &f)?;
        let programs = Arc::new(ConversionPrograms {
            vertex: vb,
            fragment: fb,
            cb0: [constant_bank_size(&vertex)?, constant_bank_size(&fragment)?],
            cb15: 0x24,
        });
        self.conversion_programs
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        self.conversion_programs.push((kind, Arc::clone(&programs)));
        Ok(programs)
    }
    fn prepare_color_conversion(
        &mut self,
        source: Arc<RawImage>,
        destination: Arc<RawImage>,
        extent: ir::Extent2D,
        area: ir::PixelRect,
        kind: u8,
    ) -> Result<PreparedDraw, IrSubmitError> {
        self.prepare_conversion_draw(
            source,
            destination,
            [
                0.,
                0.,
                extent.width() as f32,
                extent.height() as f32,
                0.,
                1.,
            ],
            area,
            kind,
            0,
            0,
            None,
            ir::FilterMode::Nearest,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn prepare_conversion_draw(
        &mut self,
        source: Arc<RawImage>,
        destination: Arc<RawImage>,
        viewport: [f32; 6],
        area: ir::PixelRect,
        kind: u8,
        source_mip: u32,
        destination_mip: u32,
        mapping: Option<[f32; 8]>,
        filter: ir::FilterMode,
    ) -> Result<PreparedDraw, IrSubmitError> {
        let programs = self.conversion_programs(kind)?;
        let mut sections: [Vec<u8>; 6] = core::array::from_fn(|_| Vec::new());
        let mut inline = Vec::new();
        for (stage, buffer) in [(0, &programs.vertex), (4, &programs.fragment)] {
            push_u32(&mut sections[0], stage);
            push_u32(&mut sections[0], 0);
            append_range(&mut sections[0], buffer, 0, buffer.logical_size);
        }
        for (stage, size) in [(0, programs.cb0[0]), (4, programs.cb0[1])] {
            if size != 0 {
                let start = inline.len();
                inline.resize(start + size as usize, 0);
                if stage == 4 {
                    if let Some(values) = mapping {
                        for (i, value) in values.into_iter().enumerate() {
                            write_word(&mut inline, start + i * 4, value.to_bits());
                        }
                    }
                }
                for word in [stage, 0] {
                    push_u32(&mut sections[3], word);
                }
                sections[3].extend_from_slice(&[0; 24]);
                push_u32(&mut sections[3], start as u32);
                push_u32(&mut sections[3], size);
                push_u64(&mut sections[3], 0);
            }
        }
        let start = inline.len();
        inline.resize(start + programs.cb15 as usize, 0);
        for word in [4, 15] {
            push_u32(&mut sections[3], word);
        }
        sections[3].extend_from_slice(&[0; 24]);
        push_u32(&mut sections[3], start as u32);
        push_u32(&mut sections[3], programs.cb15);
        push_u64(&mut sections[3], 0);
        let mut image = [0; 64];
        image[..8].copy_from_slice(&source.attachment_token.to_le_bytes());
        write_word(&mut image, 8, source_mip);
        write_word(&mut image, 12, 1);
        write_word(&mut image, 20, 1);
        write_word(
            &mut image,
            24,
            if filter == ir::FilterMode::Linear {
                3
            } else {
                0
            },
        );
        write_word(&mut image, 56, 4);
        sections[4].extend_from_slice(&image);
        push_u64(&mut sections[5], destination.attachment_token);
        for word in [destination_mip, 0, 0, 15, 1, 0, 0, 1, 0, 0] {
            push_u32(&mut sections[5], word);
        }
        let mut header = [0; 64];
        header[5] = 3;
        header[6] = 1;
        for (i, v) in viewport.into_iter().enumerate() {
            header[32 + i] = v.to_bits();
        }
        header[38..42].copy_from_slice(&[area.x(), area.y(), area.width(), area.height()]);
        let bytes = encode_draw_sections(
            &header,
            core::array::from_fn(|i| sections[i].as_slice()),
            &inline,
        )?;
        program_wire::draw::Draw::parse(&bytes).map_err(compile_error)?;
        let metadata = Arc::new(RawBuffer::create(&self.context, bytes.len() as u64)?);
        let mut resources = Vec::new();
        declare_range(
            &mut resources,
            metadata.attachment_token,
            0,
            bytes.len() as u64,
            maxwell_submit_wire::ACCESS_READ,
        )?;
        for buffer in [&programs.vertex, &programs.fragment] {
            declare_range(
                &mut resources,
                buffer.attachment_token,
                0,
                buffer.logical_size,
                maxwell_submit_wire::ACCESS_READ,
            )?;
        }
        declare_range(
            &mut resources,
            source.attachment_token,
            0,
            source.allocation_size(),
            maxwell_submit_wire::ACCESS_READ,
        )?;
        declare_range(
            &mut resources,
            destination.attachment_token,
            0,
            destination.allocation_size(),
            maxwell_submit_wire::ACCESS_READ | maxwell_submit_wire::ACCESS_WRITE,
        )?;
        Ok(PreparedDraw {
            metadata,
            bytes,
            patches: Vec::new(),
            resources,
            private_bytes: 0,
            _buffers: vec![Arc::clone(&programs.vertex), Arc::clone(&programs.fragment)],
            _images: vec![source, destination],
        })
    }
    /// Logical narrow targets use a full RGBA scratch view during rendering.
    /// Its alpha remains one while original fragment alpha controls blending.
    pub(crate) fn prepare_narrow_targets(
        &mut self,
        state: &mut DrawState<'_>,
    ) -> Result<(), IrSubmitError> {
        let pass = state.pass.ok_or(ir::Error::InvalidDescriptor)?;
        let table = Rc::clone(&self.resources);
        for (slot, attachment) in pass.color_attachments().enumerate() {
            let desc = table.texture(attachment.target())?;
            if matches!(
                desc.format(),
                ir::TextureFormat::R8Unorm | ir::TextureFormat::Rg8Unorm
            ) {
                let scratch_desc = ir::TextureDesc::new(
                    ir::TextureFormat::Bgra8Unorm,
                    desc.extent(),
                    ir::TextureUsage::RENDER_ATTACHMENT | ir::TextureUsage::SAMPLED,
                )?;
                if state.r8_targets[slot].is_none() {
                    state.r8_targets[slot] = Some(Arc::new(RawImage::create_logical(
                        &self.context,
                        scratch_desc,
                    )?));
                }
            }
        }
        Ok(())
    }
    pub(crate) fn needs_narrow_initialization(state: &DrawState<'_>) -> bool {
        !state.initialized && state.r8_targets.iter().any(Option::is_some)
    }
    pub(crate) fn ensure_draw_target(
        &mut self,
        state: &DrawState<'_>,
        metadata: &mut Vec<sgfx_codegen_maxwell::ResourceMeta>,
        bindings: &mut Vec<crate::wire::BoundObject>,
    ) -> Result<sgfx_codegen_maxwell::ObjectId, IrSubmitError> {
        let pass = state.pass.ok_or(ir::Error::InvalidDescriptor)?;
        if let Some(scratch) = state.r8_targets[0].as_ref().filter(|_| state.initialized) {
            if let Some(binding) = bindings
                .iter()
                .find(|binding| binding.attachment_token == scratch.attachment_token)
            {
                if let sgfx_codegen_maxwell::ObjectRef::External(id) = binding.object {
                    return Ok(id);
                }
            }
            let id = sgfx_codegen_maxwell::ObjectId::new(
                (1u32 << 28)
                    .checked_add(
                        u32::try_from(metadata.len()).map_err(|_| IrSubmitError::OutOfMemory)?,
                    )
                    .ok_or(ir::Error::Overflow)?,
            );
            let extent = self.resources.texture(pass.target())?.extent();
            let descriptor = ir::TextureDesc::new(
                ir::TextureFormat::Bgra8Unorm,
                extent,
                ir::TextureUsage::RENDER_ATTACHMENT | ir::TextureUsage::SAMPLED,
            )?;
            crate::execute::append_image_resource(
                id,
                scratch,
                descriptor,
                descriptor.usage(),
                metadata,
                bindings,
            )?;
            Ok(id)
        } else {
            self.ensure_image(pass.target(), metadata, bindings)
        }
    }
    pub(crate) fn narrow_conversions(
        &mut self,
        state: &mut DrawState<'_>,
        finish: bool,
    ) -> Result<Vec<PreparedDraw>, IrSubmitError> {
        if !finish && state.initialized || finish && !state.initialized {
            return Ok(Vec::new());
        }
        let pass = state.pass.ok_or(ir::Error::InvalidDescriptor)?;
        let table = Rc::clone(&self.resources);
        let mut draws = Vec::new();
        for (slot, attachment) in pass.color_attachments().enumerate() {
            if let Some(scratch) = state.r8_targets[slot].as_ref() {
                let desc = table.texture(attachment.target())?;
                let original = self.texture(attachment.target())?;
                if finish && attachment.store() == ir::StoreOp::DontCare {
                    continue;
                }
                let (source, destination) = if finish {
                    (Arc::clone(scratch), original)
                } else {
                    (original, Arc::clone(scratch))
                };
                let mut draw = self.prepare_color_conversion(
                    source,
                    destination,
                    desc.extent(),
                    pass.area(),
                    if desc.format() == ir::TextureFormat::Rg8Unorm {
                        2
                    } else {
                        u8::from(finish)
                    },
                )?;
                // Charge each temporary render target once, on its initialization
                // draw; subsequent draw owners share this same native allocation.
                if !finish && !state.private_budgeted {
                    draw.private_bytes = usize::try_from(scratch.allocation_size())
                        .map_err(|_| IrSubmitError::SubmissionTooLarge)?;
                }
                draws.push(draw);
            }
        }
        if !finish {
            state.initialized = true;
            state.private_budgeted = true;
        }
        Ok(draws)
    }
}

impl ContextResources {
    /// Convert between canonical physical R8-alpha storage and logical RGBA
    /// while applying the requested mip rectangle, filtering, and flips.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_narrow_blit(
        &mut self,
        source: Arc<RawImage>,
        destination: Arc<RawImage>,
        source_mip: u32,
        destination_mip: u32,
        source_rect: ir::PixelRect,
        destination_rect: ir::PixelRect,
        filter: ir::FilterMode,
        flips: [bool; 2],
    ) -> Result<PreparedDraw, IrSubmitError> {
        let kind = match (source.logical_format, destination.logical_format) {
            (
                ir::TextureFormat::R8Unorm,
                ir::TextureFormat::Bgra8Unorm
                | ir::TextureFormat::Rgba8Unorm
                | ir::TextureFormat::Rg8Unorm,
            ) => 3,
            (
                ir::TextureFormat::Bgra8Unorm
                | ir::TextureFormat::Rgba8Unorm
                | ir::TextureFormat::Rg8Unorm,
                ir::TextureFormat::R8Unorm,
            ) => 4,
            (
                ir::TextureFormat::Bgra8Unorm | ir::TextureFormat::Rgba8Unorm,
                ir::TextureFormat::Rg8Unorm,
            )
            | (
                ir::TextureFormat::Rg8Unorm,
                ir::TextureFormat::Bgra8Unorm | ir::TextureFormat::Rgba8Unorm,
            ) => 5,
            _ => {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ImageLayout,
                ));
            }
        };
        let src = source.subresources.ok_or(ir::Error::InvalidDescriptor)?;
        let dst = destination
            .subresources
            .ok_or(ir::Error::InvalidDescriptor)?;
        src.transfer(
            source_mip,
            0,
            source_rect.x(),
            source_rect.y(),
            source_rect.width(),
            source_rect.height(),
        )
        .map_err(|_| ir::Error::OutOfBounds)?;
        dst.transfer(
            destination_mip,
            0,
            destination_rect.x(),
            destination_rect.y(),
            destination_rect.width(),
            destination_rect.height(),
        )
        .map_err(|_| ir::Error::OutOfBounds)?;
        let src_extent = src.levels[source_mip as usize];
        let mut u = source_rect.x() as f32 / src_extent.width as f32;
        let mut v = source_rect.y() as f32 / src_extent.height as f32;
        let mut du = source_rect.width() as f32 / src_extent.width as f32;
        let mut dv = source_rect.height() as f32 / src_extent.height as f32;
        if flips[0] {
            u += du;
            du = -du;
        }
        if flips[1] {
            v += dv;
            dv = -dv;
        }
        let destination_components = [
            destination_rect.x() as f32,
            destination_rect.y() as f32,
            destination_rect.width() as f32,
            destination_rect.height() as f32,
        ];
        self.prepare_conversion_draw(
            source,
            destination,
            [
                destination_components[0],
                destination_components[1],
                destination_components[2],
                destination_components[3],
                0.,
                1.,
            ],
            destination_rect,
            kind,
            source_mip,
            destination_mip,
            Some([
                destination_components[0],
                destination_components[1],
                destination_components[2],
                destination_components[3],
                u,
                v,
                du,
                dv,
            ]),
            filter,
        )
    }
}

/// Read only the shader-accessible bytes at dispatch time. Padding and every
/// other draw's owned snapshot remain independent of these deferred reads.
fn patch_uniform_bytes<T>(
    bytes: &mut [u8],
    patches: &[T],
    span: impl Fn(&T) -> (usize, usize),
    mut read: impl FnMut(&T, &mut [u8]) -> crate::HandleResult<()>,
) -> crate::HandleResult<()> {
    for patch in patches {
        let (start, size) = span(patch);
        let end = start
            .checked_add(size)
            .ok_or(crate::HandleError::InvalidParameter)?;
        let output = bytes
            .get_mut(start..end)
            .ok_or(crate::HandleError::InvalidParameter)?;
        read(patch, output)?;
    }
    Ok(())
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    #[test]
    fn programmable_samples_reject_nv12_before_native_descriptor_creation() {
        assert!(matches!(
            programmable_sample_format(ir::TextureFormat::Nv12),
            Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ImageLayout
            ))
        ));
        for (format, logical) in [
            (ir::TextureFormat::Bgra8Unorm, 0),
            (ir::TextureFormat::Rgba8Unorm, 0),
            (ir::TextureFormat::Bgra8UnormSrgb, 0),
            (ir::TextureFormat::Rgba8UnormSrgb, 0),
            (ir::TextureFormat::R8Unorm, 2),
            (ir::TextureFormat::Rg8Unorm, 3),
            (ir::TextureFormat::Depth32Float, 4),
        ] {
            assert_eq!(programmable_sample_format(format).unwrap(), logical);
        }
    }
    #[test]
    fn shared_viewport_and_vertex_slot_zero_reach_both_render_paths() {
        let resources = ir::ResourceTable::new();
        let buffer = resources
            .define_buffer(ir::BufferDesc::new(128, ir::BufferUsage::VERTEX).unwrap())
            .unwrap();
        let viewport = ir::Viewport::new(1.25, 8., 16., -4., 0.75, 0.25).unwrap();
        let mut state = DrawState::new();
        assert!(!state.observe(&ir::Command::SetViewport(viewport)).unwrap());
        assert_eq!(state.viewport, Some(viewport));
        assert!(
            !state
                .observe(&ir::Command::SetVertexBufferSlot {
                    slot: 0,
                    buffer,
                    offset: 16
                })
                .unwrap()
        );
        assert_eq!(state.vertices[0], Some((buffer, 16)));
        assert!(
            state
                .observe(&ir::Command::SetVertexBufferSlot {
                    slot: 3,
                    buffer,
                    offset: 32
                })
                .unwrap()
        );
        assert_eq!(state.vertices[3], Some((buffer, 32)));
    }
    #[test]
    fn native_packages_match_flattened_constants_and_stage_metadata() {
        let desc=ir::ShaderModuleDesc::wgsl(alloc::string::String::from("struct U{color:vec4<f32>} @group(0) @binding(0) var<uniform> u:U; @fragment fn main()->@location(0) vec4<f32>{return u.color;}")).unwrap();
        let shader = compile_shader(&desc, ir::ShaderStage::Fragment, "main").unwrap();
        let bytes = encode_program(&shader).unwrap();
        let package = program_wire::Program::parse(&bytes).unwrap();
        assert_eq!(package.metadata.stage, program_wire::Stage::Fragment);
        assert_eq!(
            package.metadata.cb_sizes[0],
            constant_bank_size(&shader).unwrap()
        );
        assert!(package.metadata.cb_sizes[0] >= 16);
        let verified = program_wire::validate(
            &package,
            &program_wire::Limits {
                cb_sizes: package.metadata.cb_sizes,
                resource_mask: 0,
            },
        )
        .unwrap();
        assert_eq!(verified.header, shader.header);
    }
    #[test]
    fn metadata_offsets_round_trip_through_kernel_parser() {
        let mut header = [0; 64];
        header[5] = 3;
        header[6] = 2;
        header[9] = 7;
        header[34] = 16f32.to_bits();
        header[35] = 8f32.to_bits();
        header[37] = 1f32.to_bits();
        header[40] = 16;
        header[41] = 8;
        let mut programs = Vec::new();
        for (stage, token) in [(0, 1u64), (4, 2)] {
            push_u32(&mut programs, stage);
            push_u32(&mut programs, 0);
            for value in [token, 0, 544] {
                push_u64(&mut programs, value);
            }
        }
        let mut target = Vec::new();
        push_u64(&mut target, 3);
        for value in [0, 0, 1, 15, 6, 7, 0, 1, 7, 0] {
            push_u32(&mut target, value);
        }
        let mut uniforms = Vec::new();
        for value in [4, 0] {
            push_u32(&mut uniforms, value);
        }
        uniforms.extend_from_slice(&[0; 24]);
        push_u32(&mut uniforms, 0);
        push_u32(&mut uniforms, 16);
        push_u64(&mut uniforms, 0);
        let bytes = encode_draw_sections(
            &header,
            [&programs, &[], &[], &uniforms, &[], &target],
            &[0; 16],
        )
        .unwrap();
        let draw = program_wire::draw::Draw::parse(&bytes).unwrap();
        assert_eq!(draw.state.instances, 2);
        assert_eq!(draw.state.first_instance, 7);
        assert_eq!(draw.target(0).unwrap().src_color, 6);
        assert_eq!(draw.uniform(0).unwrap().inline_size, 16);
        assert_eq!(draw.inline_data(), &[0; 16]);
        assert!(
            encode_draw_sections(
                &header,
                [&programs[..31], &[], &[], &uniforms, &[], &target],
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn uniform_snapshots_read_after_prior_writes_and_keep_stage_padding() {
        use core::cell::Cell;
        let source = Cell::new(1u8);
        let patches = [(8usize, 4usize), (32, 4)];
        let prepared = vec![0; 48];
        source.set(7);
        let mut first = prepared.clone();
        patch_uniform_bytes(
            &mut first,
            &patches,
            |p| *p,
            |_, dst| {
                dst.fill(source.get());
                Ok(())
            },
        )
        .unwrap();
        source.set(19);
        let mut second = prepared;
        patch_uniform_bytes(
            &mut second,
            &patches,
            |p| *p,
            |_, dst| {
                dst.fill(source.get());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(&first[8..12], &[7; 4]);
        assert_eq!(&second[8..12], &[19; 4]);
        assert_eq!(&first[32..36], &[7; 4]);
        assert!(
            first[..8]
                .iter()
                .chain(first[12..32].iter())
                .chain(first[36..].iter())
                .all(|byte| *byte == 0)
        );
        let mut called = false;
        let result = patch_uniform_bytes(
            &mut first,
            &[(usize::MAX, 4)],
            |p| *p,
            |_, _| {
                called = true;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!called);
    }
    #[test]
    fn narrow_conversion_packages_have_the_zeroed_metadata_banks() {
        for direction in [false, true] {
            let (v, f) = sgfx_shader_maxwell::compile_r8_conversion(direction).unwrap();
            for shader in [&v, &f] {
                let bytes = encode_program(shader).unwrap();
                program_wire::Program::parse(&bytes).unwrap();
            }
            assert_eq!(f.textures.len(), 1);
            assert_eq!(f.textures[0].slot, 0);
            assert_eq!(constant_bank_size(&f).unwrap(), 16);
        }
    }
    #[test]
    fn rg8_conversions_use_validated_native_shaders_and_complete_mapping_bank() {
        for kind in [2, 5] {
            let (vertex, fragment) = compile_conversion_shaders(kind).unwrap();
            for shader in [&vertex, &fragment] {
                let bytes = encode_program(shader).unwrap();
                let package = program_wire::Program::parse(&bytes).unwrap();
                program_wire::validate(
                    &package,
                    &program_wire::Limits {
                        cb_sizes: package.metadata.cb_sizes,
                        resource_mask: package.metadata.resource_mask,
                    },
                )
                .unwrap();
            }
            assert_eq!(fragment.textures.len(), 1);
            assert_eq!(fragment.textures[0].slot, 0);
            assert_eq!(
                constant_bank_size(&fragment).unwrap(),
                if kind == 5 { 48 } else { 16 }
            );
            if kind == 5 {
                assert_eq!(fragment.uniform_buffers.len(), 1);
                assert_eq!(fragment.uniform_buffers[0].first_register, 0);
                assert_eq!(fragment.uniform_buffers[0].required_size, 32);
            }
        }
    }
}

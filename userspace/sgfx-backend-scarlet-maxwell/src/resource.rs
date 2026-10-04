//! Context-local GPU resources and immutable image layouts.

use alloc::{rc::Rc, sync::Arc, vec::Vec};
use core::{
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};
use std::sync::Mutex;

use gpu_raw::{
    GPU_BUFFER_FLAG_CPU_VISIBLE, GPU_IMAGE_FORMAT_BGRA8_UNORM, GPU_IMAGE_FORMAT_DEPTH32_FLOAT,
    GPU_IMAGE_MODIFIER_LINEAR, GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4,
    GPU_IMAGE_MODIFIER_NVIDIA_ZF32_BLOCK_LINEAR_16BX2_H4, GPU_IMAGE_USAGE_DEPTH_COMPATIBLE,
    GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT, GPU_IMAGE_USAGE_PRESENTABLE,
    GPU_IMAGE_USAGE_RENDER_TARGET, GPU_IMAGE_USAGE_SAMPLED, GPU_IMAGE_USAGE_TRANSFER_DST,
    GPU_IMAGE_USAGE_TRANSFER_SRC, GpuBuffer, GpuImage, GpuImageLayout,
};
#[cfg(feature = "std")]
use scarlet_os::handle::capability::memory_mapping::{MemoryMappingOps, flags, prot};
#[cfg(not(feature = "std"))]
use std::handle::capability::memory_mapping::{MemoryMappingOps, flags, prot};

use crate::Handle;
use crate::{ContextInner, HandleError, HandleResult, Image, IrSubmitError, ir};

pub(crate) struct RawImage {
    pub(crate) raw: GpuImage,
    pub(crate) attachment_token: u64,
    pub(crate) layout: GpuImageLayout,
    pub(crate) logical_format: ir::TextureFormat,
    pub(crate) subresources: Option<maxwell_image_layout::Layout>,
    context_id: i32,
    context: Arc<ContextInner>,
    attached: AtomicBool,
}

impl RawImage {
    pub(crate) fn create_present(
        context: &Arc<ContextInner>,
        width: u32,
        height: u32,
    ) -> HandleResult<Self> {
        if width == 0 || height == 0 {
            return Err(HandleError::InvalidParameter);
        }
        let usage = GPU_IMAGE_USAGE_RENDER_TARGET
            | GPU_IMAGE_USAGE_PRESENTABLE
            | GPU_IMAGE_USAGE_SAMPLED
            | GPU_IMAGE_USAGE_TRANSFER_SRC
            | GPU_IMAGE_USAGE_TRANSFER_DST;
        let raw = context.device.gpu.create_image_with_format_and_usage(
            GPU_IMAGE_FORMAT_BGRA8_UNORM,
            width,
            height,
            usage,
        )?;
        Self::finish_create(context, raw, ir::TextureFormat::Bgra8Unorm, width, height)
    }

    pub(crate) fn create_logical(
        context: &Arc<ContextInner>,
        descriptor: ir::TextureDesc,
    ) -> HandleResult<Self> {
        let width = descriptor.extent().width();
        let height = descriptor.extent().height();
        let (format, usage) = image_create_parameters(descriptor)?;
        let usage = native_image_usage(descriptor, format, usage, context.device.capabilities.supports_depth());
        let raw = context
            .device
            .gpu
            .create_layered_image_with_format_and_usage(
                format, width, height, usage, descriptor.mip_level_count(),
                descriptor.array_layer_count(), descriptor.cube_compatible(),
            )
            .map_err(|error| {
                std::println!(
                    "[gm20b-userspace] create image {}x{} usage={:#x}: {:?}",
                    width,
                    height,
                    usage,
                    error
                );
                error
            })?;
        Self::finish_create_shape(context, raw, descriptor.format(), width, height,
            descriptor.mip_level_count(), descriptor.array_layer_count(), descriptor.cube_compatible())
    }

    fn import_sampled(
        context: &Arc<ContextInner>,
        handle: Handle,
        descriptor: ir::TextureDesc,
    ) -> HandleResult<Self> {
        if descriptor.format() != ir::TextureFormat::Bgra8Unorm
            || !descriptor.usage().contains(ir::TextureUsage::SAMPLED)
            || descriptor.usage().contains(ir::TextureUsage::PRESENT)
        {
            return Err(HandleError::InvalidParameter);
        }
        let raw = GpuImage::from_handle(handle)?;
        let info = raw.query()?;
        let width = descriptor.extent().width();
        let height = descriptor.extent().height();
        if info.format != GPU_IMAGE_FORMAT_BGRA8_UNORM
            || info.usage & GPU_IMAGE_USAGE_SAMPLED == 0
            || info.width != width
            || info.height != height
        {
            return Err(HandleError::InvalidParameter);
        }
        Self::finish_create_shape(context, raw, ir::TextureFormat::Bgra8Unorm, width, height,
            descriptor.mip_level_count(), descriptor.array_layer_count(), descriptor.cube_compatible())
    }

    fn import_ycbcr(
        context: &Arc<ContextInner>,
        handle: Handle,
        descriptor: ir::TextureDesc,
        conversion: ir::YcbcrConversion,
    ) -> HandleResult<Self> {
        use gpu_raw::shared_image::*;
        if descriptor.format() != ir::TextureFormat::Nv12
            || descriptor.usage() != ir::TextureUsage::SAMPLED
        {
            return Err(HandleError::InvalidParameter);
        }
        let color = ImageColor {
            matrix: match conversion.matrix {
                ir::YcbcrMatrix::Bt601 => COLOR_MATRIX_BT601,
                ir::YcbcrMatrix::Bt709 => COLOR_MATRIX_BT709,
            },
            range: match conversion.range {
                ir::YcbcrRange::Limited => COLOR_RANGE_LIMITED,
                ir::YcbcrRange::Full => COLOR_RANGE_FULL,
            },
            chroma_x: match conversion.chroma_x {
                ir::ChromaLocation::Cosited => CHROMA_COSITED,
                ir::ChromaLocation::Midpoint => CHROMA_MIDPOINT,
            },
            chroma_y: match conversion.chroma_y {
                ir::ChromaLocation::Cosited => CHROMA_COSITED,
                ir::ChromaLocation::Midpoint => CHROMA_MIDPOINT,
            },
            ..Default::default()
        };
        let raw = context.device.gpu.import_shared_image(&handle, color)?;
        let info = raw.query()?;
        if info.format != gpu_raw::GPU_IMAGE_FORMAT_NV12
            || info.width != descriptor.extent().width()
            || info.height != descriptor.extent().height()
            || info.usage != GPU_IMAGE_USAGE_SAMPLED
        {
            return Err(HandleError::InvalidParameter);
        }
        Self::finish_create(
            context,
            raw,
            ir::TextureFormat::Nv12,
            info.width,
            info.height,
        )
    }

    fn finish_create(
        context: &Arc<ContextInner>,
        raw: GpuImage,
        logical_format: ir::TextureFormat,
        width: u32,
        height: u32,
    ) -> HandleResult<Self> {
        Self::finish_create_shape(context, raw, logical_format, width, height, 1, 1, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_create_shape(
        context: &Arc<ContextInner>, raw: GpuImage, logical_format: ir::TextureFormat,
        width: u32, height: u32, mip_levels: u32, array_layers: u32, cube: bool,
    ) -> HandleResult<Self> {
        let texture = raw.query_texture()?;
        if texture.mip_levels != mip_levels || texture.array_layers != array_layers
            || (texture.flags & gpu_raw::GPU_TEXTURE_CREATE_CUBE != 0) != cube
        { return Err(HandleError::InvalidParameter); }
        let layout = raw.query_layout()?;
        let subresources = if logical_format == ir::TextureFormat::Nv12 {
            validate_image_layout(&layout, width, height, logical_format)?;
            None
        } else {
            Some(validate_image_subresources(&layout, width, height, logical_format, mip_levels, array_layers)?)
        };
        let attachment_token = context.raw.attach_image(&raw)?;
        if attachment_token == 0 {
            return Err(HandleError::InvalidParameter);
        }
        Ok(Self {
            raw,
            attachment_token,
            layout,
            logical_format,
            subresources,
            context_id: context.raw.as_handle().as_raw(),
            context: Arc::clone(context),
            attached: AtomicBool::new(true),
        })
    }

    pub(crate) fn upload_bgra(&self, bytes: &[u8], stride: u32, area: gpu_raw::GpuImageBgraRect) -> HandleResult<()> {
        self.context.raw.upload_image_bgra(&self.raw, bytes, stride, area)
    }

    pub(crate) fn allocation_size(&self) -> u64 {
        self.layout.total_size
    }

    pub(crate) fn belongs_to(&self, context: &ContextInner) -> bool {
        self.context_id == context.raw.as_handle().as_raw()
    }

    fn detach(&self) -> HandleResult<()> {
        if self.attached.swap(false, Ordering::AcqRel) {
            if let Err(error) = self.context.raw.detach_image(&self.raw) {
                self.attached.store(true, Ordering::Release);
                return Err(error);
            }
        }
        Ok(())
    }
}

impl Drop for RawImage {
    fn drop(&mut self) {
        let _ = self.detach();
    }
}

pub(crate) struct RawBuffer {
    pub(crate) raw: GpuBuffer,
    pub(crate) attachment_token: u64,
    pub(crate) logical_size: u64,
    context: Arc<ContextInner>,
    attached: AtomicBool,
    mapping_address: usize,
    mapping_len: usize,
    cpu_access: Mutex<()>,
}

impl RawBuffer {
    pub(crate) fn create(context: &Arc<ContextInner>, logical_size: u64) -> HandleResult<Self> {
        if logical_size == 0 {
            return Err(HandleError::InvalidParameter);
        }
        let raw = context
            .device
            .gpu
            .create_buffer(logical_size, GPU_BUFFER_FLAG_CPU_VISIBLE)
            .map_err(|error| {
                std::println!(
                    "[gm20b-userspace] create buffer size={}: {:?}",
                    logical_size,
                    error
                );
                error
            })?;
        if !raw.cpu_visible() || raw.allocated_size() < logical_size {
            return Err(HandleError::Unsupported);
        }
        let mapping_len =
            usize::try_from(raw.allocated_size()).map_err(|_| HandleError::InvalidParameter)?;
        let mapping = raw.as_handle().as_memory_mapping()?;
        // SAFETY: This creates a fresh mapping. RawBuffer retains the allocation,
        // bounds CPU accesses to it and owns the mapping until teardown.
        let mapping_address =
            unsafe { mapping.mmap(0, mapping_len, prot::READ | prot::WRITE, flags::SHARED, 0) }
                .map_err(|_| HandleError::SystemError(-1))?;
        let attachment_token = match context.raw.attach_buffer(&raw) {
            Ok(token) => token,
            Err(error) => {
                // SAFETY: Attachment failed before any buffer access was exposed.
                let _ = unsafe { MemoryMappingOps::munmap(mapping_address, mapping_len) };
                return Err(error);
            }
        };
        if attachment_token == 0 {
            let _ = context.raw.detach_buffer(&raw);
            // SAFETY: The failed attachment was detached and no CPU view escaped.
            let _ = unsafe { MemoryMappingOps::munmap(mapping_address, mapping_len) };
            return Err(HandleError::InvalidParameter);
        }
        Ok(Self {
            raw,
            attachment_token,
            logical_size,
            context: Arc::clone(context),
            attached: AtomicBool::new(true),
            mapping_address,
            mapping_len,
            cpu_access: Mutex::new(()),
        })
    }

    pub(crate) fn write(&self, offset: u64, bytes: &[u8]) -> HandleResult<()> {
        let byte_len = u64::try_from(bytes.len()).map_err(|_| HandleError::InvalidParameter)?;
        let end = offset
            .checked_add(byte_len)
            .ok_or(HandleError::InvalidParameter)?;
        if bytes.is_empty() || end > self.logical_size {
            std::println!(
                "[gm20b-userspace] buffer write offset={} bytes={} capacity={}",
                offset,
                bytes.len(),
                self.logical_size
            );
            return Err(HandleError::InvalidParameter);
        }
        let destination_offset =
            usize::try_from(offset).map_err(|_| HandleError::InvalidParameter)?;
        let _guard = crate::dispatch::lock(&self.cpu_access);

        // SAFETY: `create` retains a writable mapping of `mapping_len` bytes
        // for the complete RawBuffer lifetime. The checked logical range above
        // is no larger than that backing allocation and the source slice
        // remains valid for this copy.
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (self.mapping_address as *mut u8).add(destination_offset),
                bytes.len(),
            );
        }
        Ok(())
    }

    pub(crate) fn read(&self, offset: u64, length: usize) -> HandleResult<Vec<u8>> {
        checked_buffer_range(self.logical_size, offset, length)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| HandleError::OutOfResources)?;
        bytes.resize(length, 0);
        self.read_into(offset, &mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn read_into(&self, offset: u64, output: &mut [u8]) -> HandleResult<()> {
        let start = checked_buffer_range(self.logical_size, offset, output.len())?;
        let _guard = crate::dispatch::lock(&self.cpu_access);
        if !output.is_empty() {
            // SAFETY: this owner retains the readable mapping, the checked range
            // stays within its logical allocation, and CPU accesses are serialized.
            unsafe {
                ptr::copy_nonoverlapping(
                    (self.mapping_address as *const u8).add(start),
                    output.as_mut_ptr(),
                    output.len(),
                );
            }
        }
        Ok(())
    }

    pub(crate) fn copy_from(
        &self,
        source: &Self,
        source_offset: u64,
        destination_offset: u64,
        size: u64,
    ) -> HandleResult<()> {
        let length = usize::try_from(size).map_err(|_| HandleError::InvalidParameter)?;
        let source_start = checked_buffer_range(source.logical_size, source_offset, length)?;
        let destination_start =
            checked_buffer_range(self.logical_size, destination_offset, length)?;
        let copy = || {
            if length != 0 {
                // SAFETY: both owners retain their mappings, both checked ranges
                // are within them, and their CPU access locks are held. `copy`
                // deliberately permits overlapping ranges in the same allocation.
                unsafe {
                    ptr::copy(
                        (source.mapping_address as *const u8).add(source_start),
                        (self.mapping_address as *mut u8).add(destination_start),
                        length,
                    );
                }
            }
        };
        if ptr::eq(self, source) {
            let _guard = crate::dispatch::lock(&self.cpu_access);
            copy();
        } else if (self as *const Self as usize) < (source as *const Self as usize) {
            let _destination = crate::dispatch::lock(&self.cpu_access);
            let _source = crate::dispatch::lock(&source.cpu_access);
            copy();
        } else {
            let _source = crate::dispatch::lock(&source.cpu_access);
            let _destination = crate::dispatch::lock(&self.cpu_access);
            copy();
        }
        Ok(())
    }

    fn detach(&self) -> HandleResult<()> {
        if self.attached.swap(false, Ordering::AcqRel) {
            if let Err(error) = self.context.raw.detach_buffer(&self.raw) {
                self.attached.store(true, Ordering::Release);
                return Err(error);
            }
        }
        Ok(())
    }
}

fn checked_buffer_range(capacity: u64, offset: u64, length: usize) -> HandleResult<usize> {
    let length = u64::try_from(length).map_err(|_| HandleError::InvalidParameter)?;
    if offset.checked_add(length).is_none_or(|end| end > capacity) {
        return Err(HandleError::InvalidParameter);
    }
    usize::try_from(offset).map_err(|_| HandleError::InvalidParameter)
}

impl Drop for RawBuffer {
    fn drop(&mut self) {
        let _ = self.detach();
        // The mapping must be retired before the capability-backed allocation
        // is dropped. Teardown cannot report an error, and the kernel still
        // revokes the address space when the process exits.
        // SAFETY: RawBuffer owns this CPU mapping and no borrowed CPU view can
        // outlive it. Kernel-held GPU backing is independent of the CPU mapping.
        let _ = unsafe { MemoryMappingOps::munmap(self.mapping_address, self.mapping_len) };
    }
}

pub(crate) struct ContextResources {
    pub(crate) conversion_programs: Vec<(u8, Arc<crate::programmable::ConversionPrograms>)>,
    pub(crate) programmable_programs: Vec<(ir::ProgrammableRenderPipelineId, Arc<RawBuffer>, Arc<RawBuffer>)>,
    pub(crate) programmable_pipelines: core::cell::RefCell<Vec<(ir::ProgrammableRenderPipelineId, Rc<crate::programmable::CompiledPipeline>)>>,
    pub(crate) resources: Rc<ir::ResourceTable>,
    pub(crate) context: Arc<ContextInner>,
    pub(crate) images: Vec<Option<Arc<RawImage>>>,
    pub(crate) buffers: Vec<Option<Arc<RawBuffer>>>,
    image_identities: Vec<Option<ir::TextureId>>,
    buffer_identities: Vec<Option<ir::BufferId>>,
    scratch: Option<RawBuffer>,
}

impl ContextResources {
    pub(crate) fn new(
        resources: Rc<ir::ResourceTable>,
        context: Arc<ContextInner>,
    ) -> Result<Self, IrSubmitError> {
        Ok(Self {
            conversion_programs: Vec::new(),
            programmable_programs: Vec::new(),
            programmable_pipelines: core::cell::RefCell::new(Vec::new()),
            resources,
            context,
            images: empty_slots(ir::MAX_TEXTURES)?,
            buffers: empty_slots(ir::MAX_BUFFERS)?,
            image_identities: empty_slots(ir::MAX_TEXTURES)?,
            buffer_identities: empty_slots(ir::MAX_BUFFERS)?,
            scratch: None,
        })
    }

    pub(crate) fn map_present_image(
        &mut self,
        texture: ir::TextureId,
        image: Arc<Image>,
    ) -> Result<(), IrSubmitError> {
        let reference = self.resources.texture_ref(texture)?;
        if !self
            .resources
            .texture(reference)?
            .usage()
            .contains(ir::TextureUsage::PRESENT)
        {
            return Err(IrSubmitError::Unsupported(
                crate::UnsupportedIrFeature::ResourceState,
            ));
        }
        self.map_image(texture, image)
    }

    pub(crate) fn map_image(
        &mut self,
        texture: ir::TextureId,
        image: Arc<Image>,
    ) -> Result<(), IrSubmitError> {
        let reference = self.resources.texture_ref(texture)?;
        self.validate_mapped_image(reference, &image)?;
        let slot = reference.slot();
        refresh_cache_identity(&mut self.image_identities[slot],&mut self.images[slot],reference.id());
        if self
            .images
            .get(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?
            .is_some()
        {
            return Err(IrSubmitError::TextureAlreadyMapped);
        }
        if self
            .images
            .iter()
            .flatten()
            .any(|candidate| Arc::ptr_eq(candidate, &image.raw))
        {
            return Err(IrSubmitError::ImageAlreadyMapped);
        }
        self.images[slot] = Some(Arc::clone(&image.raw));
        Ok(())
    }

    fn validate_mapped_image(
        &self,
        reference: ir::TextureRef<'_>,
        image: &Image,
    ) -> Result<(), IrSubmitError> {
        let descriptor = self.resources.texture(reference)?;
        let required = ir::TextureUsage::RENDER_ATTACHMENT;
        let allowed = required
            | ir::TextureUsage::PRESENT
            | ir::TextureUsage::SAMPLED
            | ir::TextureUsage::COPY_SRC
            | ir::TextureUsage::COPY_DST;
        if descriptor.format() != ir::TextureFormat::Bgra8Unorm
            || !descriptor.usage().contains(required)
            || !allowed.contains(descriptor.usage())
            || descriptor.mip_level_count() != 1
            || descriptor.array_layer_count() != 1
        {
            return Err(IrSubmitError::Unsupported(
                crate::UnsupportedIrFeature::TargetUsage,
            ));
        }
        if !Arc::ptr_eq(&image.raw.context, &self.context)
            || !image.raw.attached.load(Ordering::Acquire)
        {
            return Err(IrSubmitError::ContextMismatch);
        }
        if descriptor.extent().width() != image.width
            || descriptor.extent().height() != image.height
        {
            return Err(IrSubmitError::TargetExtentMismatch);
        }
        Ok(())
    }

    pub(crate) fn unmap_image(
        &mut self,
        texture: ir::TextureId,
        image: &Image,
    ) -> Result<(), IrSubmitError> {
        let slot = self.resources.texture_ref(texture)?.slot();
        let mapped = self
            .images
            .get_mut(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?;
        if mapped
            .as_ref()
            .is_none_or(|candidate| !Arc::ptr_eq(candidate, &image.raw))
        {
            return Err(IrSubmitError::ImageNotMapped);
        }
        // Keep the physical image attached: callers may retain and remap it.
        // Accepted work owns its own Arc; removing a mapping still requires an
        // idle context so new logical work cannot replace it during retirement.
        self.context.dispatcher.with_idle(|| {
            *mapped = None;
            Ok(())
        })
    }

    pub(crate) fn import_sampled_image(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
    ) -> Result<(), IrSubmitError> {
        let reference = self.resources.texture_ref(texture)?;
        let descriptor = self.resources.texture(reference)?;
        let slot = reference.slot();
        refresh_cache_identity(&mut self.image_identities[slot],&mut self.images[slot],reference.id());
        if self
            .images
            .get(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?
            .is_some()
        {
            return Err(IrSubmitError::TextureAlreadyMapped);
        }
        let image = Arc::new(RawImage::import_sampled(&self.context, handle, descriptor)?);
        if self
            .images
            .iter()
            .flatten()
            .any(|candidate| candidate.attachment_token == image.attachment_token)
        {
            let _ = image.detach();
            return Err(IrSubmitError::ImageAlreadyMapped);
        }
        self.images[slot] = Some(image);
        Ok(())
    }

    pub(crate) fn import_ycbcr_image(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
        conversion: ir::YcbcrConversion,
    ) -> Result<(), IrSubmitError> {
        let reference = self.resources.texture_ref(texture)?;
        let descriptor = self.resources.texture(reference)?;
        let slot = reference.slot();
        refresh_cache_identity(&mut self.image_identities[slot],&mut self.images[slot],reference.id());
        if self
            .images
            .get(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?
            .is_some()
        {
            return Err(IrSubmitError::TextureAlreadyMapped);
        }
        let image = RawImage::import_ycbcr(&self.context, handle, descriptor, conversion)?;
        self.images[slot] = Some(Arc::new(image));
        Ok(())
    }

    pub(crate) fn release_imported_image(
        &mut self,
        texture: ir::TextureId,
    ) -> Result<(), IrSubmitError> {
        let reference=self.resources.texture_ref(texture)?;
        let descriptor=self.resources.texture(reference)?;
        if descriptor.usage().contains(ir::TextureUsage::PRESENT){
            return Err(IrSubmitError::Unsupported(crate::UnsupportedIrFeature::ResourceState));
        }
        let slot=reference.slot();
        if self.image_identities.get(slot).copied().flatten()!=Some(texture) || self.images.get(slot).and_then(Option::as_ref).is_none(){
            return Err(IrSubmitError::ImageNotMapped);
        }
        self.release_texture(texture)
    }

    pub(crate) fn release_texture(&mut self, texture: ir::TextureId) -> Result<(), IrSubmitError> {
        // Validate against the old live table before any metadata replacement
        // can make a later generation resolve to this physical cache slot.
        let slot = self.resources.texture_ref(texture)?.slot();
        let identity=&mut self.image_identities[slot];
        let image = self
            .images
            .get_mut(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?;
        self.context
            .dispatcher
            .with_idle(|| {
                refresh_cache_identity(identity,image,texture);
                release_slot(image,|image|image.detach())?;
                *identity=None;
                Ok(())
            })
    }

    pub(crate) fn release_buffer(&mut self, buffer: ir::BufferId) -> Result<(), IrSubmitError> {
        let buffer_id=buffer;
        let slot = self.resources.buffer_ref(buffer)?.slot();
        let identity=&mut self.buffer_identities[slot];
        let buffer = self
            .buffers
            .get_mut(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?;
        self.context
            .dispatcher
            .with_idle(|| {
                refresh_cache_identity(identity,buffer,buffer_id);
                release_slot(buffer,|buffer|buffer.detach())?;
                *identity=None;
                Ok(())
            })
    }

    pub(crate) fn texture(
        &mut self,
        reference: ir::TextureRef<'_>,
    ) -> Result<Arc<RawImage>, IrSubmitError> {
        let slot = reference.slot();
        let (descriptor,cached)=validated_texture_entry(&self.resources,reference,&self.images,&self.image_identities)?;
        if let Some(image) = cached {
            return Ok(Arc::clone(image));
        }
        if descriptor.usage().contains(ir::TextureUsage::PRESENT) {
            return Err(IrSubmitError::ImageNotMapped);
        }
        let image = Arc::new(RawImage::create_logical(&self.context, descriptor)?);
        let entry = self
            .images
            .get_mut(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?;
        *entry = Some(Arc::clone(&image));
        self.image_identities[slot]=Some(reference.id());
        Ok(image)
    }

    pub(crate) fn buffer(
        &mut self,
        reference: ir::BufferRef<'_>,
    ) -> Result<&Arc<RawBuffer>, IrSubmitError> {
        // ResourceTable validates the reference identity before returning a
        // descriptor, including when this slot already has a cached allocation.
        let descriptor = self.resources.buffer(reference)?;
        let slot = reference.slot();
        if slot >= self.buffers.len() {
            return Err(IrSubmitError::ResourceTableMismatch);
        }
        refresh_cache_identity(&mut self.buffer_identities[slot],&mut self.buffers[slot],reference.id());
        if self.buffers[slot].is_none() {
            self.buffers[slot] = Some(Arc::new(RawBuffer::create(
                &self.context,
                descriptor.size(),
            )?));
        }
        self.buffers[slot]
            .as_ref()
            .ok_or(IrSubmitError::ResourceTableMismatch)
    }

    pub(crate) fn write_buffer(
        &mut self,
        reference: ir::BufferRef<'_>,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), IrSubmitError> {
        self.buffer(reference)?.write(offset, bytes)?;
        Ok(())
    }

    pub(crate) fn scratch(&mut self, required_size: u64) -> Result<&RawBuffer, IrSubmitError> {
        let needs_replacement = self
            .scratch
            .as_ref()
            .is_none_or(|scratch| scratch.logical_size < required_size);
        if needs_replacement {
            let allocation = required_size
                .checked_next_power_of_two()
                .ok_or(IrSubmitError::OutOfMemory)?;
            let replacement = RawBuffer::create(&self.context, allocation)?;
            if let Some(previous) = self.scratch.take() {
                if let Err(error) = previous.detach() {
                    // Preserve the already-live scratch on failure and discard the
                    // newly attached replacement. The original detachment failure
                    // remains authoritative if cleanup also fails.
                    let _ = replacement.detach();
                    self.scratch = Some(previous);
                    return Err(error.into());
                }
            }
            self.scratch = Some(replacement);
        }
        self.scratch.as_ref().ok_or(IrSubmitError::OutOfMemory)
    }

    pub(crate) fn context_id(&self) -> i32 {
        self.context.raw.as_handle().as_raw()
    }
}

/// Validate table identity and generation before exposing a physical cache hit.
fn validated_texture_entry<'a,T>(resources:&ir::ResourceTable,reference:ir::TextureRef<'_>,cache:&'a[Option<T>],identities:&[Option<ir::TextureId>])->Result<(ir::TextureDesc,Option<&'a T>),IrSubmitError>{
    if !reference.belongs_to(resources){return Err(IrSubmitError::ResourceTableMismatch);}
    let descriptor=resources.texture(reference)?;
    let entry=cache.get(reference.slot()).ok_or(IrSubmitError::ResourceTableMismatch)?;
    let cached=if identities.get(reference.slot()).copied().flatten()==Some(reference.id()){entry.as_ref()}else{None};
    Ok((descriptor,cached))
}

/// Replacing a logical identity drops only this cache's ownership. Accepted
/// jobs keep their old Arc, and detach the old attachment after retirement.
fn refresh_cache_identity<K:Copy+Eq,T>(identity:&mut Option<K>,cache:&mut Option<T>,current:K){
    if *identity!=Some(current){*cache=None;*identity=Some(current);}
}

fn release_slot<T>(
    slot: &mut Option<T>,
    detach: impl FnOnce(&T) -> HandleResult<()>,
) -> Result<(), IrSubmitError> {
    if let Some(resource) = slot.as_ref() {
        detach(resource)?;
    }
    // Keep the owner in place until detach succeeds, including on error.
    *slot = None;
    Ok(())
}

fn native_image_usage(descriptor: ir::TextureDesc, format: u32, mut usage: u32, supports_depth: bool) -> u32 {
    if descriptor.dimension_1d() && format != GPU_IMAGE_FORMAT_DEPTH32_FLOAT {
        // The generic ABI accepts DEPTH_COMPATIBLE only alongside RENDER_TARGET.
        // Its tiled allocation enables a typed D1 TIC even for a sampled-only
        // logical image; logical command usage remains checked separately.
        usage |= GPU_IMAGE_USAGE_RENDER_TARGET | GPU_IMAGE_USAGE_DEPTH_COMPATIBLE;
    } else if usage & GPU_IMAGE_USAGE_RENDER_TARGET != 0 && supports_depth {
        usage |= GPU_IMAGE_USAGE_DEPTH_COMPATIBLE;
    }
    usage
}

fn image_create_parameters(descriptor: ir::TextureDesc) -> HandleResult<(u32, u32)> {
    if descriptor.format() == ir::TextureFormat::Nv12 {
        return Err(HandleError::Unsupported);
    }
    let mut usage = 0;
    if descriptor.format() == ir::TextureFormat::Depth32Float {
        if descriptor.usage().contains(ir::TextureUsage::RENDER_ATTACHMENT) {
            usage |= GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT;
        }
        if descriptor.usage().contains(ir::TextureUsage::SAMPLED) {
            usage |= GPU_IMAGE_USAGE_SAMPLED;
        }
    } else {
        if descriptor
            .usage()
            .contains(ir::TextureUsage::RENDER_ATTACHMENT)
        {
            usage |= GPU_IMAGE_USAGE_RENDER_TARGET;
            if matches!(descriptor.format(), ir::TextureFormat::R8Unorm | ir::TextureFormat::Rg8Unorm) {
                usage |= GPU_IMAGE_USAGE_SAMPLED;
            }
        }
        if descriptor.usage().contains(ir::TextureUsage::PRESENT) {
            usage |= GPU_IMAGE_USAGE_PRESENTABLE | GPU_IMAGE_USAGE_TRANSFER_SRC;
        }
        if descriptor.usage().contains(ir::TextureUsage::SAMPLED)
            || descriptor.usage().contains(ir::TextureUsage::COPY_SRC)
        {
            usage |= GPU_IMAGE_USAGE_SAMPLED;
        }
        if descriptor.usage().contains(ir::TextureUsage::COPY_SRC) {
            usage |= GPU_IMAGE_USAGE_TRANSFER_SRC;
        }
        if descriptor.usage().contains(ir::TextureUsage::COPY_DST) {
            // Typed copy conversions execute a native sampled draw into this image.
            // Logical usage remains checked against the SGFX descriptor.
            usage |= GPU_IMAGE_USAGE_TRANSFER_DST | GPU_IMAGE_USAGE_RENDER_TARGET | GPU_IMAGE_USAGE_SAMPLED;
        }
    }
    if usage == 0 {
        return Err(HandleError::InvalidParameter);
    }
    let format = if descriptor.format() == ir::TextureFormat::Depth32Float {
        GPU_IMAGE_FORMAT_DEPTH32_FLOAT
    } else {
        GPU_IMAGE_FORMAT_BGRA8_UNORM
    };
    Ok((format, usage))
}

fn validate_image_subresources(
    layout: &GpuImageLayout,
    width: u32,
    height: u32,
    logical_format: ir::TextureFormat,
    mip_levels: u32,
    array_layers: u32,
) -> HandleResult<maxwell_image_layout::Layout> {
    use maxwell_image_layout::{Descriptor, LayoutKind, NVIDIA_DEPTH_MODIFIER_BASE, modifier_tile_y};
    let depth = logical_format == ir::TextureFormat::Depth32Float;
    let kind = if layout.modifier == GPU_IMAGE_MODIFIER_LINEAR {
        if depth { return Err(HandleError::Unsupported); }
        LayoutKind::Linear
    } else {
        let y = modifier_tile_y(layout.modifier).ok_or(HandleError::Unsupported)?;
        if (layout.modifier & !0xf == NVIDIA_DEPTH_MODIFIER_BASE) != depth {
            return Err(HandleError::Unsupported);
        }
        LayoutKind::BlockLinear { base_y_log2: y, clamp_mips: mip_levels > 1 || array_layers > 1 }
    };
    let planned = maxwell_image_layout::plan(Descriptor {
        width, height, mip_levels, array_layers, bytes_per_pixel: 4,
    }, kind).map_err(|_| HandleError::InvalidParameter)?;
    let p = layout.planes[0];
    if layout.plane_count != 1 || layout.total_size != planned.total_size
        || layout.alignment == 0 || !layout.alignment.is_power_of_two()
        || p.offset != 0 || p.size != planned.total_size
        || p.row_pitch != planned.levels[0].row_pitch || p.array_pitch != planned.array_pitch
        || p.block_width != 1 || p.block_height != 1 || p.bytes_per_block != 4
    { return Err(HandleError::InvalidParameter); }
    Ok(planned)
}

fn validate_image_layout(
    layout: &GpuImageLayout,
    width: u32,
    height: u32,
    logical_format: ir::TextureFormat,
) -> HandleResult<()> {
    if logical_format == ir::TextureFormat::Nv12 {
        if layout.plane_count != 2
            || layout.total_size == 0
            || !matches!(layout.modifier, 0 | 0x0300_0000_000f_e011)
        {
            return Err(HandleError::Unsupported);
        }
        for (i, p) in layout.planes[..2].iter().enumerate() {
            if p.block_width != 1
                || p.block_height != 1
                || p.bytes_per_block != if i == 0 { 1 } else { 2 }
                || p.row_pitch < if i == 0 { width } else { width.div_ceil(2) * 2 }
                || p.size == 0
                || p.offset
                    .checked_add(p.size)
                    .is_none_or(|end| end > layout.total_size)
            {
                return Err(HandleError::InvalidParameter);
            }
        }
        return Ok(());
    }
    if layout.plane_count != 1
        || layout.total_size == 0
        || layout.alignment == 0
        || !layout.alignment.is_power_of_two()
    {
        return Err(HandleError::Unsupported);
    }
    let plane = layout.planes[0];
    let physical_bytes_per_pixel = 4u32;
    let minimum_pitch = width
        .checked_mul(physical_bytes_per_pixel)
        .ok_or(HandleError::InvalidParameter)?;
    let plane_end = plane
        .offset
        .checked_add(plane.size)
        .ok_or(HandleError::InvalidParameter)?;
    if plane_end > layout.total_size
        || plane.block_width != 1
        || plane.block_height != 1
        || u32::from(plane.bytes_per_block) != physical_bytes_per_pixel
    {
        return Err(HandleError::Unsupported);
    }
    let depth = logical_format == ir::TextureFormat::Depth32Float;
    let tiled = layout.modifier
        == if depth {
            GPU_IMAGE_MODIFIER_NVIDIA_ZF32_BLOCK_LINEAR_16BX2_H4
        } else {
            GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4
        };
    let padded_height = if tiled {
        if plane.row_pitch
            != minimum_pitch
                .checked_add(63)
                .ok_or(HandleError::InvalidParameter)?
                & !63
            || plane.offset & 8191 != 0
        {
            return Err(HandleError::Unsupported);
        }
        height
            .checked_add(127)
            .ok_or(HandleError::InvalidParameter)?
            & !127
    } else {
        height
    };
    let minimum_size = u64::from(plane.row_pitch)
        .checked_mul(u64::from(padded_height))
        .ok_or(HandleError::InvalidParameter)?;
    if (depth && !tiled)
        || (!tiled && layout.modifier != GPU_IMAGE_MODIFIER_LINEAR)
        || plane.row_pitch < minimum_pitch
        || plane.size < minimum_size
    {
        return Err(HandleError::Unsupported);
    }

    Ok(())
}

fn empty_slots<T>(length: usize) -> Result<Vec<Option<T>>, IrSubmitError> {
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(length)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    slots.resize_with(length, || None);
    Ok(slots)
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    #[test]
    fn stale_texture_reference_is_rejected_before_a_physical_cache_hit() {
        let resources=ir::ResourceTable::new();
        let descriptor=ir::TextureDesc::new(ir::TextureFormat::Bgra8Unorm,ir::Extent2D::new(2,2).unwrap(),ir::TextureUsage::SAMPLED).unwrap();
        let old=resources.define_texture(descriptor).unwrap();
        let cache=vec![Some(37u32)];
        let identities=vec![Some(old.id())];
        assert_eq!(validated_texture_entry(&resources,old,&cache,&identities).unwrap().1,Some(&37));
        resources.release_texture(old.id()).unwrap();
        assert!(validated_texture_entry(&resources,old,&cache,&identities).is_err());
        let replacement=resources.define_texture(descriptor).unwrap();
        assert_eq!(replacement.slot(),old.slot());
        assert_ne!(replacement.id(),old.id());
        assert!(validated_texture_entry(&resources,old,&cache,&identities).is_err());
        assert!(validated_texture_entry(&resources,replacement,&cache,&identities).unwrap().1.is_none());
    }

    #[test]
    fn reused_buffer_slot_replaces_cache_without_revoking_a_queued_owner(){
        let resources=ir::ResourceTable::new();
        let descriptor=ir::BufferDesc::new(16,ir::BufferUsage::VERTEX).unwrap();
        let old=resources.define_buffer(descriptor).unwrap();
        let dropped=Arc::new(AtomicBool::new(false));
        let old_owner=Arc::new(Allocation(Arc::clone(&dropped)));
        let queued_owner=Arc::clone(&old_owner);
        let mut cache=Some(old_owner);
        let mut identity=Some(old.id());
        resources.release_buffer(old.id()).unwrap();
        let replacement=resources.define_buffer(ir::BufferDesc::new(32,ir::BufferUsage::VERTEX).unwrap()).unwrap();
        assert_eq!(replacement.slot(),old.slot());
        assert!(resources.buffer(old).is_err());
        assert_eq!(resources.buffer(replacement).unwrap().size(),32);
        refresh_cache_identity(&mut identity,&mut cache,replacement.id());
        assert!(cache.is_none());
        assert_eq!(identity,Some(replacement.id()));
        assert!(!dropped.load(Ordering::Relaxed));
        drop(queued_owner);
        assert!(dropped.load(Ordering::Relaxed));
    }

    struct Allocation(Arc<AtomicBool>);

    impl Drop for Allocation {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn retirement_preserves_owner_when_detach_fails() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut slot = Some(Allocation(Arc::clone(&dropped)));
        let result = release_slot(&mut slot, |_| Err(HandleError::PermissionDenied));
        assert!(matches!(
            result,
            Err(IrSubmitError::Backend(HandleError::PermissionDenied))
        ));
        assert!(slot.is_some());
        assert!(!dropped.load(Ordering::Relaxed));
        release_slot(&mut slot, |_| Ok(())).unwrap();
        assert!(slot.is_none());
        assert!(dropped.load(Ordering::Relaxed));
    }

    #[test]
    fn retirement_accepts_unmaterialized_slot() {
        let mut slot: Option<Allocation> = None;
        release_slot(&mut slot, |_| panic!("empty slot has no attachment")).unwrap();
        assert!(slot.is_none());
    }

    #[test]
    fn mapped_buffer_ranges_allow_empty_end_but_reject_overflow() {
        assert_eq!(checked_buffer_range(8, 2, 6).unwrap(), 2);
        assert_eq!(checked_buffer_range(8, 8, 0).unwrap(), 8);
        assert!(checked_buffer_range(8, 2, 7).is_err());
        assert!(checked_buffer_range(8, 9, 0).is_err());
        assert!(checked_buffer_range(u64::MAX, u64::MAX, 1).is_err());
    }
    #[test]
    fn native_one_dimensional_sampling_satisfies_generic_tiled_usage_contract() {
        let descriptor = ir::TextureDesc::new(ir::TextureFormat::Rgba8Unorm,
            ir::Extent2D::new(16, 1).unwrap(), ir::TextureUsage::SAMPLED).unwrap().with_dimension_1d(true).unwrap();
        let (format, usage) = image_create_parameters(descriptor).unwrap();
        let native = native_image_usage(descriptor, format, usage, true);
        assert_ne!(native & GPU_IMAGE_USAGE_RENDER_TARGET, 0);
        assert_ne!(native & GPU_IMAGE_USAGE_DEPTH_COMPATIBLE, 0);
        assert_eq!(descriptor.usage(), ir::TextureUsage::SAMPLED);
        let depth = ir::TextureDesc::new(ir::TextureFormat::Depth32Float,
            ir::Extent2D::new(16, 1).unwrap(), ir::TextureUsage::SAMPLED).unwrap().with_dimension_1d(true).unwrap();
        let (format, usage) = image_create_parameters(depth).unwrap();
        assert_eq!(native_image_usage(depth, format, usage, true), GPU_IMAGE_USAGE_SAMPLED);
    }

}

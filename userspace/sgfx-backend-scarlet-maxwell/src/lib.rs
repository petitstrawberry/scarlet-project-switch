//! Scarlet GPU ABI backend for the implemented NVIDIA Maxwell GM20B SGFX subset.
//!
//! This crate owns the GPU connection, exact backend/dialect negotiation,
//! context-local resource attachments, physical image layouts, command lowering,
//! Maxwell submit-wire encoding, and synchronous or tracked queue submission. The pure Maxwell
//! code generator remains transport-independent.

#[cfg(not(feature = "std"))]
compile_error!("The native Maxwell backend requires feature `std`; `scarlet-std` is a compatibility alias for the native standard library.");

extern crate alloc;

use alloc::{rc::Rc, sync::Arc, vec::Vec};

use gpu_raw::{
    GPU_DEVICE_STATE_READY, GPU_EXECUTION_SUPPORT_IMAGE_READBACK,
    GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD, GPU_EXECUTION_SUPPORT_MEMORY,
    GPU_EXECUTION_SUPPORT_PRESENTATION, GPU_EXECUTION_SUPPORT_QUEUE, GPU_MAX_IMAGE_UPLOAD_SIZE,
    GPU_RESULT_SUCCESS, Gpu, GpuImageBgraRect, GpuQueryInfo,
};
#[cfg(feature = "std")]
pub use scarlet_os::handle::{Handle, HandleError, HandleResult};
#[cfg(not(feature = "std"))]
pub use std::handle::{Handle, HandleError, HandleResult};

pub use sgfx_core::ir;

mod asynchronous;
mod completion;
mod dispatch;
mod execute;
mod image_subresource;
mod normalization;
mod preparation;
mod programmable;
mod resource;
mod scheduler;
mod wire;

pub use completion::Submission;
pub use sgfx_shader_maxwell::ShaderCompileError;

use resource::{ContextResources, RawImage};

/// Exact backend identifier advertised by the NVIDIA Maxwell kernel backend.
pub const BACKEND_ID: &[u8] = b"nvidia-gm20b";

/// Exact Maxwell command dialect required by this backend.
pub const DIALECT_ID: &[u8] = b"maxwell-sgfx-ops-v1";
/// Extended dialect with validated programmable draws and image subresources.
pub const EXTENDED_DIALECT_ID: &[u8] = b"maxwell-sgfx-ops-v2";

/// Return whether a backend identifier selects this Maxwell backend.
///
/// # Arguments
///
/// * `backend_id` - Exact identifier returned by [`GpuQueryInfo`].
///
/// # Returns
///
/// `true` only for [`BACKEND_ID`].
pub fn matches_backend_id(backend_id: &[u8]) -> bool {
    backend_id == BACKEND_ID
}

/// Return whether dialect information selects the required Maxwell submit format.
///
/// # Arguments
///
/// * `dialect_info` - Exact opaque bytes returned by `GPU_QUERY_DIALECT`.
///
/// # Returns
///
/// `true` for [`DIALECT_ID`] or [`EXTENDED_DIALECT_ID`].
pub fn matches_dialect(dialect_info: &[u8]) -> bool {
    dialect_info == DIALECT_ID || dialect_info == EXTENDED_DIALECT_ID
}

/// Device capabilities expressed in portable SGFX terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    rendering: bool,
    presentation: bool,
    image_upload: bool,
    image_readback: bool,
    depth: bool,
    extended: bool,
}

impl Capabilities {
    const fn from_execution_support(execution_support: u32) -> Self {
        Self {
            rendering: true,
            presentation: execution_support & GPU_EXECUTION_SUPPORT_PRESENTATION != 0,
            image_upload: execution_support & GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD != 0,
            image_readback: execution_support & GPU_EXECUTION_SUPPORT_IMAGE_READBACK != 0,
            depth: execution_support & gpu_raw::GPU_EXECUTION_SUPPORT_DEPTH != 0,
            extended: false,
        }
    }

    /// Return whether command execution is available.
    pub const fn supports_rendering(&self) -> bool {
        self.rendering
    }

    /// Return whether mapped images may be presented.
    pub const fn supports_presentation(&self) -> bool {
        self.presentation
    }

    /// Return whether texture upload is available.
    pub const fn supports_image_upload(&self) -> bool {
        self.image_upload
    }

    /// Return whether rendered BGRA images can be read back synchronously.
    pub const fn supports_image_readback(&self) -> bool {
        self.image_readback
    }

    /// Return whether depth attachments are available.
    pub const fn supports_depth(&self) -> bool {
        self.depth
    }

    /// Return whether native programmable graphics translation is implemented.
    pub const fn supports_programmable_graphics(&self) -> bool {
        self.extended && self.rendering
    }

    /// Return whether native texture array allocation and sampling are implemented.
    pub const fn supports_texture_arrays(&self) -> bool {
        self.extended && self.rendering
    }

    /// Return whether depth textures can be sampled by the implemented pipelines.
    pub const fn supports_depth_sampling(&self) -> bool {
        self.extended && self.depth
    }

    /// Return whether native mip allocation and sampling are implemented.
    pub const fn supports_image_mips(&self) -> bool {
        self.extended && self.rendering
    }
}

struct DeviceInner {
    gpu: Gpu,
    dialect: gpu_raw::GpuDialect,
    capabilities: Capabilities,
    codegen_capabilities: sgfx_codegen_maxwell::Capabilities,
}

/// An owning connection to a compatible Scarlet Maxwell GPU device.
pub struct Device {
    inner: Arc<DeviceInner>,
}

impl Device {
    /// Test whether already-queried GPU information is compatible.
    ///
    /// This check is side-effect free. Dialect compatibility is checked later by
    /// [`Device::from_gpu`] because it requires a control request on the owning
    /// connection.
    ///
    /// # Arguments
    ///
    /// * `info` - Information returned from the same `Gpu` connection.
    ///
    /// # Returns
    ///
    /// `true` only for a ready exact-match backend with queue and memory support.
    pub fn supports(info: &GpuQueryInfo) -> bool {
        info.result == GPU_RESULT_SUCCESS
            && info.device_state == GPU_DEVICE_STATE_READY
            && matches_backend_id(info.backend_id_bytes())
            && info.execution_support & GPU_EXECUTION_SUPPORT_QUEUE != 0
            && info.execution_support & GPU_EXECUTION_SUPPORT_MEMORY != 0
            && info.max_opaque_command_size != 0
    }

    /// Adopt an already-opened GPU connection after exact backend negotiation.
    ///
    /// # Arguments
    ///
    /// * `gpu` - Owning connection used to obtain `info`.
    /// * `info` - Query result from that same connection.
    ///
    /// # Returns
    ///
    /// A compatible device, or [`HandleError::Unsupported`] when either the
    /// backend or dialect differs byte-for-byte from this implementation.
    pub fn from_gpu(gpu: Gpu, info: GpuQueryInfo) -> HandleResult<Self> {
        if !Self::supports(&info) {
            return Err(HandleError::Unsupported);
        }

        let dialect = match gpu.query_dialect(1) {
            Ok(dialect) if dialect.opaque_info() == EXTENDED_DIALECT_ID => dialect,
            _ => gpu.query_dialect(0)?,
        };
        if !matches_dialect(dialect.opaque_info()) {
            return Err(HandleError::Unsupported);
        }

        let mut capabilities = Capabilities::from_execution_support(info.execution_support);
        capabilities.extended = dialect.opaque_info() == EXTENDED_DIALECT_ID;
        let transport_bytes = usize::try_from(info.max_opaque_command_size)
            .unwrap_or(maxwell_submit_wire::MAX_SUBMIT_SIZE)
            .min(maxwell_submit_wire::MAX_SUBMIT_SIZE);
        let max_command_words = transport_bytes
            .saturating_sub(maxwell_submit_wire::HEADER_SIZE)
            .checked_div(core::mem::size_of::<u32>())
            .and_then(|words| u32::try_from(words).ok())
            .ok_or(HandleError::Unsupported)?;
        if max_command_words == 0 {
            return Err(HandleError::Unsupported);
        }
        let codegen_capabilities = sgfx_codegen_maxwell::Capabilities::gm20b(max_command_words);
        Ok(Self {
            inner: Arc::new(DeviceInner {
                gpu,
                dialect,
                capabilities,
                codegen_capabilities,
            }),
        })
    }

    /// Open and negotiate a Scarlet GPU device.
    ///
    /// # Arguments
    ///
    /// * `path` - GPU device path such as `/dev/gpu0`.
    ///
    /// # Returns
    ///
    /// An owning compatible device or a handle error.
    pub fn open(path: &str) -> HandleResult<Self> {
        let gpu = Gpu::open(path)?;
        let info = gpu.query_info()?;
        Self::from_gpu(gpu, info)
    }

    /// Return portable capabilities for this negotiated device.
    pub fn capabilities(&self) -> Capabilities {
        self.inner.capabilities
    }

    /// Create a Maxwell execution context for the negotiated dialect.
    pub fn create_context(&self) -> HandleResult<Context> {
        let raw = self.inner.gpu.create_context(&self.inner.dialect)?;
        if raw.effective_dialect_index() != self.inner.dialect.index()
            || raw.effective_dialect_token() != self.inner.dialect.token()
        {
            return Err(HandleError::Unsupported);
        }
        Ok(Context {
            inner: Arc::new(ContextInner {
                device: Arc::clone(&self.inner),
                raw,
                dispatcher: dispatch::NativeScheduler::new()?,
            }),
        })
    }
}

struct ContextInner {
    device: Arc<DeviceInner>,
    raw: gpu_raw::GpuContext,
    dispatcher: dispatch::NativeScheduler,
}

/// An owning Maxwell execution context.
#[derive(Clone)]
pub struct Context {
    inner: Arc<ContextInner>,
}

impl Context {
    /// Observe context retirement without waiting for GPU work or admission locks.
    pub fn is_idle(&self) -> Result<bool, IrSubmitError> {
        self.inner.dispatcher.is_idle()
    }

    /// Create a persistent physical cache for one logical resource table.
    pub fn create_ir_resources(
        &self,
        resources: Rc<ir::ResourceTable>,
    ) -> Result<IrResources, IrSubmitError> {
        Ok(IrResources {
            inner: ContextResources::new(resources, Arc::clone(&self.inner))?,
            mapped_images: Vec::new(),
        })
    }

    /// Create a queue retaining this context and its device authority.
    pub fn create_queue(&self) -> HandleResult<Queue> {
        Ok(Queue {
            raw: Arc::new(self.inner.raw.create_queue()?),
            context: self.clone(),
        })
    }

    /// Create and map physical images for logical presentation targets.
    ///
    /// # Arguments
    ///
    /// * `resources` - Logical SGFX resource table retained by the session.
    /// * `targets` - Distinct `PRESENT` texture identities to materialize.
    ///
    /// # Returns
    ///
    /// A session owning its queue, cache, context, and mapped images. Each
    /// logical target has one stable physical image for the session lifetime;
    /// callers implement buffering with multiple target identities, matching
    /// the VirGL mapped-target contract and shared-image registration model.
    pub fn create_mapped_target_session(
        &self,
        resources: Rc<ir::ResourceTable>,
        targets: &[ir::TextureId],
    ) -> Result<MappedTargetSession, IrSubmitError> {
        let queue = Arc::new(self.inner.raw.create_queue()?);
        let mut cache = ContextResources::new(Rc::clone(&resources), Arc::clone(&self.inner))?;
        let mut images = Vec::new();
        images
            .try_reserve_exact(targets.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;

        for &target in targets {
            if images
                .iter()
                .any(|(candidate, _): &(ir::TextureId, Arc<Image>)| *candidate == target)
            {
                return Err(IrSubmitError::TextureAlreadyMapped);
            }
            let reference = resources.texture_ref(target)?;
            let descriptor = resources.texture(reference)?;
            let required = ir::TextureUsage::RENDER_ATTACHMENT | ir::TextureUsage::PRESENT;
            if descriptor.format() != ir::TextureFormat::Bgra8Unorm
                || !descriptor.usage().contains(required)
            {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::TargetUsage,
                ));
            }
            let image =
                Arc::new(self.create_shared_image(
                    descriptor.extent().width(),
                    descriptor.extent().height(),
                )?);
            cache.map_present_image(target, Arc::clone(&image))?;
            images.push((target, image));
        }

        Ok(MappedTargetSession {
            images,
            resources: cache,
            queue,
            context: Context {
                inner: Arc::clone(&self.inner),
            },
        })
    }

    /// Create a presentation-capable linear BGRA render target.
    ///
    /// # Arguments
    ///
    /// * `width` - Non-zero image width.
    /// * `height` - Non-zero image height.
    ///
    /// # Returns
    ///
    /// An attached image whose queried layout is retained by the backend.
    pub fn create_shared_image(&self, width: u32, height: u32) -> HandleResult<Image> {
        let raw = RawImage::create_present(&self.inner, width, height)?;
        Ok(Image {
            raw: Arc::new(raw),
            width,
            height,
        })
    }

    /// Read one render-target rectangle into a complete BGRA destination buffer.
    ///
    /// # Arguments
    ///
    /// * `image` - Render-target image created by this context.
    /// * `destination` - Complete writable BGRA destination buffer.
    /// * `destination_stride` - Bytes between destination rows.
    /// * `rect` - Source image rectangle written at identical destination coordinates.
    ///
    /// # Returns
    ///
    /// Success after synchronous GPU readback, or a handle error.
    pub fn readback_image_bgra(
        &self,
        image: &Image,
        destination: &mut [u8],
        destination_stride: u32,
        rect: ir::PixelRect,
    ) -> HandleResult<()> {
        // A direct Context readback must also cover logical chunks which have
        // not yet reached the kernel FIFO. All sessions share this dispatcher.
        self.inner.dispatcher.wait_idle()?;
        if !image.raw.belongs_to(&self.inner)
            || !self.inner.device.capabilities.supports_image_readback()
        {
            return Err(HandleError::InvalidParameter);
        }
        validate_readback_destination(
            image.width,
            image.height,
            destination.len(),
            destination_stride,
            rect,
        )?;

        let row_bytes = rect
            .width()
            .checked_mul(4)
            .ok_or(HandleError::InvalidParameter)?;
        let max_rows = max_readback_rows(row_bytes)?;
        let mut y = rect.y();
        let mut remaining = rect.height();
        while remaining != 0 {
            let height = remaining.min(max_rows);
            self.inner.raw.readback_image_bgra(
                &image.raw.raw,
                destination,
                destination_stride,
                GpuImageBgraRect::new(rect.x(), y, rect.width(), height),
            )?;
            y = y.checked_add(height).ok_or(HandleError::InvalidParameter)?;
            remaining -= height;
        }
        Ok(())
    }

    /// Read mip zero, layer zero of a logical color texture into tightly packed rows.
    /// Earlier context submissions retire before the native readback begins.
    pub fn read_texture_into(
        &self,
        resources: &mut IrResources,
        texture: ir::TextureId,
        destination: &mut [u8],
    ) -> Result<(), IrSubmitError> {
        if !Arc::ptr_eq(&self.inner, &resources.inner.context) {
            return Err(IrSubmitError::ContextMismatch);
        }
        let table = Rc::clone(&resources.inner.resources);
        let reference = table.texture_ref(texture)?;
        let descriptor = table.texture(reference)?;
        validate_texture_readback(descriptor, destination.len())?;
        if !self.inner.device.capabilities.supports_image_readback() {
            return Err(HandleError::Unsupported.into());
        }
        resources.inner.drain_async()?;
        let mut image = resources.inner.texture(reference)?;
        let width = descriptor.extent().width();
        let height = descriptor.extent().height();
        // The generic readback ABI addresses level zero of a one-layer image.
        // Resolve a layered/mipped base view with an ordered GPU transfer.
        if descriptor.mip_level_count() > 1 || descriptor.array_layer_count() > 1 {
            let temporary_desc = ir::TextureDesc::new(descriptor.format(), descriptor.extent(),
                ir::TextureUsage::COPY_SRC | ir::TextureUsage::COPY_DST)?;
            let temporary = Arc::new(resource::RawImage::create_logical(&self.inner, temporary_desc)?);
            let area = ir::PixelRect::new(0, 0, width, height)?;
            let transfer = image_subresource::prepare_blit(&self.inner, image, Arc::clone(&temporary),
                0, 0, area, area, ir::FilterMode::Nearest, [false; 2])?;
            self.inner.raw.create_queue()?.submit(&transfer.bytes)?;
            image = temporary;
        }
        let stride = width.checked_mul(4).ok_or(ir::Error::Overflow)?;
        let transport_length = usize::try_from(u64::from(stride) * u64::from(height))
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        let mut transport = Vec::new();
        transport.try_reserve_exact(transport_length).map_err(|_| IrSubmitError::OutOfMemory)?;
        transport.resize(transport_length, 0);
        let max_rows = max_readback_rows(stride)?;
        let mut y = 0;
        while y < height {
            let rows = (height - y).min(max_rows);
            // The raw ABI writes at the source rectangle coordinates in the
            // complete destination, including batches whose source y is nonzero.
            self.inner.raw.readback_image_bgra(&image.raw,
                &mut transport, stride,
                GpuImageBgraRect::new(0, y, width, rows))?;
            y += rows;
        }
        let logical_stride = width.checked_mul(descriptor.format().bytes_per_pixel().ok_or(
            IrSubmitError::Unsupported(UnsupportedIrFeature::TextureReadback))?).ok_or(ir::Error::Overflow)?;
        image_subresource::convert_bgra_readback(descriptor.format(), width, height, &transport, stride,
            destination, logical_stride)?;
        Ok(())
    }
}

fn validate_texture_readback(
    descriptor: ir::TextureDesc,
    destination_length: usize,
) -> Result<(), IrSubmitError> {
    if !descriptor.usage().contains(ir::TextureUsage::COPY_SRC) {
        return Err(ir::Error::InvalidUsage.into());
    }
    if !matches!(
            descriptor.format(),
            ir::TextureFormat::Bgra8Unorm
                | ir::TextureFormat::Rgba8Unorm
                | ir::TextureFormat::R8Unorm
                | ir::TextureFormat::Rg8Unorm
                | ir::TextureFormat::Bgra8UnormSrgb
                | ir::TextureFormat::Rgba8UnormSrgb
        )
    {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::TextureReadback,
        ));
    }
    let expected = u64::from(descriptor.extent().width())
        .checked_mul(u64::from(descriptor.extent().height()))
        .and_then(|pixels| pixels.checked_mul(u64::from(descriptor.format().bytes_per_pixel().unwrap())))
        .ok_or(ir::Error::Overflow)?;
    if expected != u64::try_from(destination_length).map_err(|_| ir::Error::Overflow)?
    {
        return Err(ir::Error::OutOfBounds.into());
    }
    Ok(())
}

fn max_readback_rows(row_bytes: u32) -> HandleResult<u32> {
    GPU_MAX_IMAGE_UPLOAD_SIZE
        .checked_div(row_bytes)
        .filter(|rows| *rows != 0)
        .ok_or(HandleError::InvalidParameter)
}

fn validate_readback_destination(
    image_width: u32,
    image_height: u32,
    destination_length: usize,
    destination_stride: u32,
    rect: ir::PixelRect,
) -> HandleResult<()> {
    let x_end = rect
        .x()
        .checked_add(rect.width())
        .ok_or(HandleError::InvalidParameter)?;
    let y_end = rect
        .y()
        .checked_add(rect.height())
        .ok_or(HandleError::InvalidParameter)?;
    if x_end > image_width || y_end > image_height {
        return Err(HandleError::InvalidParameter);
    }

    let destination_row_end = x_end.checked_mul(4).ok_or(HandleError::InvalidParameter)?;
    if destination_stride < destination_row_end {
        return Err(HandleError::InvalidParameter);
    }
    let destination_end = u64::from(y_end - 1)
        .checked_mul(u64::from(destination_stride))
        .and_then(|offset| offset.checked_add(u64::from(destination_row_end)))
        .ok_or(HandleError::InvalidParameter)?;
    let destination_length =
        u64::try_from(destination_length).map_err(|_| HandleError::InvalidParameter)?;
    if destination_end > destination_length {
        return Err(HandleError::InvalidParameter);
    }
    Ok(())
}

/// Failure while materializing or submitting a logical SGFX command buffer.
#[derive(Debug)]
pub enum IrSubmitError {
    /// The SGFX resource or command buffer failed validation.
    InvalidIr(ir::Error),
    /// The command buffer and persistent cache use different tables.
    ResourceTableMismatch,
    /// A context or queue differs from the cache's creating context.
    ContextMismatch,
    /// The mapped target extent differs from its logical texture.
    TargetExtentMismatch,
    /// A logical target has no mapped physical image.
    ImageNotMapped,
    /// A logical texture already has a physical image mapping.
    TextureAlreadyMapped,
    /// A physical image is mapped to another logical texture.
    ImageAlreadyMapped,
    /// A valid SGFX feature cannot be represented faithfully.
    Unsupported(UnsupportedIrFeature),
    /// Host-side bounded allocation failed.
    OutOfMemory,
    /// The Scarlet GPU ABI rejected an operation.
    Backend(HandleError),
    /// The pure Maxwell code generator rejected the normalized command stream.
    Codegen(sgfx_codegen_maxwell::CompileError),
    /// The canonical Maxwell submit-wire encoder rejected its input.
    SubmitWire(maxwell_submit_wire::Error),
    /// Source compilation or native executable validation failed.
    ShaderCompile(ShaderCompileError),
    /// The negotiated kernel queue does not implement asynchronous admission.
    AsyncUnsupported,
    /// A logical submission exceeds the bounded staging or command capacity.
    SubmissionTooLarge,
    /// Pending dispatch or concurrent retirement prevents resource release.
    ResourceBusy,
    /// Native completion or acceptance cannot be observed authoritatively.
    CompletionUnavailable,
    /// The kernel reported a terminal GPU completion failure.
    CompletionFailed(u32),
}

impl From<ir::Error> for IrSubmitError {
    fn from(error: ir::Error) -> Self {
        Self::InvalidIr(error)
    }
}

impl From<HandleError> for IrSubmitError {
    fn from(error: HandleError) -> Self {
        Self::Backend(error)
    }
}

impl From<sgfx_codegen_maxwell::CompileError> for IrSubmitError {
    fn from(error: sgfx_codegen_maxwell::CompileError) -> Self {
        Self::Codegen(error)
    }
}

impl From<maxwell_submit_wire::Error> for IrSubmitError {
    fn from(error: maxwell_submit_wire::Error) -> Self {
        Self::SubmitWire(error)
    }
}

/// Portable feature rejected before backend submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedIrFeature {
    PipelineTargetFormat,
    PrimitiveTopology,
    VertexLayout,
    ResourceBindings,
    /// The presentation target format or usage is not supported.
    TargetUsage,
    /// A physical image layout is incompatible with Maxwell execution.
    ImageLayout,
    /// A texture upload format cannot be converted without loss.
    TextureUpload,
    /// A requested texture readback format or subresource is not supported.
    TextureReadback,
    /// A command references an unsupported resource state.
    ResourceState,
    /// Programmable pipelines, compute, or barriers need an extended backend.
    ProgrammableExecution,
}

/// Session owning mapped presentation images and all execution state.
pub struct MappedTargetSession {
    // These owners must drop before the queue/context that authorized them.
    images: Vec<(ir::TextureId, Arc<Image>)>,
    resources: ContextResources,
    queue: Arc<gpu_raw::GpuQueue>,
    context: Context,
}

impl MappedTargetSession {
    /// Check whether all work admitted by this context has retired.
    ///
    /// This never waits for the dispatcher lock or GPU completion. Pending
    /// work or lock contention returns `false`; failed dispatch returns its
    /// error even when its retained jobs have already been discarded.
    pub fn is_idle(&self) -> Result<bool, IrSubmitError> {
        self.context.inner.dispatcher.is_idle()
    }

    /// Retire a texture's physical storage while its logical identity is live.
    ///
    /// Call this before releasing the identity or synchronizing replacement
    /// resource metadata. Pending dispatch returns [`IrSubmitError::ResourceBusy`]
    /// without waiting. Detachment failures preserve the existing mapping.
    /// A successfully retired presentation target is no longer returned by
    /// [`Self::image`]. An unmaterialized live texture is also valid to retire.
    pub fn release_texture(&mut self, texture: ir::TextureId) -> Result<(), IrSubmitError> {
        self.resources.release_texture(texture)?;
        self.images.retain(|(candidate, _)| *candidate != texture);
        Ok(())
    }

    /// Retire a buffer's physical storage while its logical identity is live.
    ///
    /// Call this before releasing the identity or synchronizing replacement
    /// resource metadata. Pending dispatch returns [`IrSubmitError::ResourceBusy`]
    /// without waiting. Detachment failures preserve the existing allocation.
    /// An unmaterialized live buffer is also valid to retire.
    pub fn release_buffer(&mut self, buffer: ir::BufferId) -> Result<(), IrSubmitError> {
        self.resources.release_buffer(buffer)
    }

    /// Import a transferred shared BGRA image into a logical sampled texture.
    pub fn import_shared_bgra_texture(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
    ) -> Result<(), IrSubmitError> {
        self.resources.import_sampled_image(texture, handle)
    }

    pub fn import_ycbcr_texture(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
        conversion: ir::YcbcrConversion,
    ) -> Result<(), IrSubmitError> {
        self.resources
            .import_ycbcr_image(texture, handle, conversion)
    }

    /// Detach and release a previously imported sampled texture.
    pub fn release_imported_texture(
        &mut self,
        texture: ir::TextureId,
    ) -> Result<(), IrSubmitError> {
        self.resources.release_imported_image(texture)
    }

    /// Borrow the stable image mapped to a logical presentation target.
    pub fn image(&self, target: ir::TextureId) -> Result<ImageRef<'_>, IrSubmitError> {
        self.images
            .iter()
            .find(|(candidate, _)| *candidate == target)
            .map(|(_, image)| image.as_ref())
            .ok_or(IrSubmitError::ImageNotMapped)
    }

    /// Read one mapped presentation-image rectangle into a BGRA buffer.
    ///
    /// # Arguments
    ///
    /// * `target` - Logical presentation texture identity.
    /// * `destination` - Complete writable BGRA destination buffer.
    /// * `destination_stride` - Bytes between destination rows.
    /// * `rect` - Source target rectangle written at identical destination coordinates.
    ///
    /// # Returns
    ///
    /// Success after synchronous GPU readback, or an execution error.
    pub fn readback_bgra(
        &self,
        target: ir::TextureId,
        destination: &mut [u8],
        destination_stride: u32,
        rect: ir::PixelRect,
    ) -> Result<(), IrSubmitError> {
        self.resources.drain_async()?;
        let image = self.image(target)?;
        self.context
            .readback_image_bgra(image, destination, destination_stride, rect)?;
        Ok(())
    }

    /// Bind this session's queue and resource cache for command execution.
    pub fn executor(&mut self) -> Executor<'_> {
        Executor {
            queue: &self.queue,
            queue_context: &self.context.inner,
            context: &self.context,
            resources: &mut self.resources,
        }
    }
}

/// Borrowed view of a mapped Scarlet Maxwell presentation image.
///
/// This alias keeps the backend surface directly usable by the SGFX frontend
/// without weakening the session's ownership of the underlying GPU image.
pub type ImageRef<'a> = &'a Image;

/// Persistent logical resources for independent queue and image operations.
/// All native allocations use the same context cache as mapped-target sessions.
pub struct IrResources {
    inner: ContextResources,
    mapped_images: Vec<(ir::TextureId, Arc<Image>)>,
}

impl IrResources {
    /// Borrow the logical resource table retained by this cache.
    pub fn resources(&self) -> &ir::ResourceTable {
        self.inner.resources.as_ref()
    }

    /// Associate a logical BGRA render target with an image from this context.
    pub fn map_image(
        &mut self,
        texture: ir::TextureId,
        image: Arc<Image>,
    ) -> Result<(), IrSubmitError> {
        self.mapped_images
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        self.inner.map_image(texture, Arc::clone(&image))?;
        self.mapped_images.push((texture, image));
        Ok(())
    }

    /// Remove a mapping while retaining any independently owned physical image.
    pub fn unmap_image(&mut self, texture: ir::TextureId) -> Result<(), IrSubmitError> {
        let index = self
            .mapped_images
            .iter()
            .position(|(candidate, _)| *candidate == texture)
            .ok_or(IrSubmitError::ImageNotMapped)?;
        self.inner
            .unmap_image(texture, &self.mapped_images[index].1)?;
        self.mapped_images.remove(index);
        Ok(())
    }

    /// Retire a live texture's native allocation without waiting for busy work.
    pub fn release_texture(&mut self, texture: ir::TextureId) -> Result<(), IrSubmitError> {
        if self.mapped_images.iter().any(|(candidate, _)| *candidate == texture) {
            return Err(ir::Error::InvalidDescriptor.into());
        }
        self.inner.release_texture(texture)?;
        Ok(())
    }

    /// Retire a live buffer's native allocation without waiting for busy work.
    pub fn release_buffer(&mut self, buffer: ir::BufferId) -> Result<(), IrSubmitError> {
        self.inner.release_buffer(buffer)
    }

    /// Validate a retired bind group; command lowering retains its bindings.
    pub fn release_bind_group(&mut self, group: ir::BindGroupId) -> Result<(), IrSubmitError> {
        self.inner.resources.bind_group_ref(group)?;
        self.inner.context.dispatcher.with_idle(|| Ok(()))
    }

    /// Read native CPU-visible buffer storage after earlier submissions retire.
    pub fn read_buffer_into(
        &mut self,
        buffer: ir::BufferId,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), IrSubmitError> {
        let table = Rc::clone(&self.inner.resources);
        let reference = table.buffer_ref(buffer)?;
        let descriptor = table.buffer(reference)?;
        let length = u64::try_from(destination.len()).map_err(|_| ir::Error::Overflow)?;
        if offset.checked_add(length).ok_or(ir::Error::Overflow)? > descriptor.size() {
            return Err(ir::Error::OutOfBounds.into());
        }
        self.inner.drain_async()?;
        self.inner
            .buffer(reference)?
            .read_into(offset, destination)?;
        Ok(())
    }

    /// Parse and validate supported shader source before admitting its handle.
    pub fn validate_shader_module(&self, shader: ir::ShaderModuleId) -> Result<(), IrSubmitError> {
        let module = self.inner
            .resources
            .shader_module(self.inner.resources.shader_module_ref(shader)?)?;
        sgfx_shader_maxwell::validate_shader_module(&module).map_err(IrSubmitError::ShaderCompile)
    }

    /// Reject pipelines whose shader execution is not implemented by Maxwell.
    pub fn validate_programmable_render_pipeline(
        &self,
        pipeline: ir::ProgrammableRenderPipelineId,
    ) -> Result<(), IrSubmitError> {
        if !self.inner.context.device.capabilities.supports_programmable_graphics() {
            return Err(IrSubmitError::Unsupported(UnsupportedIrFeature::ProgrammableExecution));
        }
        self.inner.compiled_pipeline(pipeline).map(|_| ())
    }

    /// Reject compute execution until its native lowering is implemented.
    pub fn validate_compute_pipeline(
        &self,
        pipeline: ir::ComputePipelineId,
    ) -> Result<(), IrSubmitError> {
        self.inner
            .resources
            .compute_pipeline(self.inner.resources.compute_pipeline_ref(pipeline)?)?;
        Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ProgrammableExecution,
        ))
    }
}

/// Native queue retaining the context which granted its submission authority.
pub struct Queue {
    raw: Arc<gpu_raw::GpuQueue>,
    context: Context,
}

impl Queue {
    /// Bind this queue, a context, and a persistent cache for command execution.
    /// Context mismatches are rejected before materialization or submission.
    pub fn executor<'a>(
        &'a self,
        context: &'a Context,
        resources: &'a mut IrResources,
    ) -> Executor<'a> {
        Executor {
            queue: &self.raw,
            queue_context: &self.context.inner,
            context,
            resources: &mut resources.inner,
        }
    }

    /// Admit a complete logical command stream using the native async dispatcher.
    pub fn submit_ir_async<'r, 'data>(
        &self,
        context: &Context,
        resources: &mut IrResources,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<Submission, sgfx_core::backend::SubmitError<IrSubmitError, Submission>> {
        sgfx_core::backend::CommandSubmitter::submit(
            &mut self.executor(context, resources),
            commands,
        )
    }
}

/// Renderable image that can be presented through a Scarlet display surface.
pub struct Image {
    raw: Arc<RawImage>,
    width: u32,
    height: u32,
}

impl Image {
    /// Return image width in pixels.
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Return image height in pixels.
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the owning GPU image capability used for presentation.
    pub fn shared_handle(&self) -> &Handle {
        self.raw.raw.as_handle()
    }
}

/// Command executor bound to one context, queue, and persistent resource cache.
pub struct Executor<'a> {
    queue: &'a Arc<gpu_raw::GpuQueue>,
    queue_context: &'a Arc<ContextInner>,
    context: &'a Context,
    resources: &'a mut ContextResources,
}

impl sgfx_core::backend::CommandExecutor for Executor<'_> {
    type Error = IrSubmitError;

    fn execute<'r, 'data>(
        &mut self,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<(), Self::Error> {
        if !Arc::ptr_eq(self.queue_context, &self.context.inner)
            || !Arc::ptr_eq(self.queue_context, &self.resources.context)
        {
            return Err(IrSubmitError::ContextMismatch);
        }
        let result = self
            .resources
            .execute(&self.context.inner, self.queue, commands);
        if let Err(error) = &result {
            std::println!("[gm20b-userspace] command execution failed: {:?}", error);
        }
        result
    }
}

impl sgfx_core::backend::CommandSubmitter for Executor<'_> {
    type Submission = Submission;

    fn supports_async_submission(&self) -> bool {
        Arc::ptr_eq(self.queue_context, &self.context.inner)
            && Arc::ptr_eq(self.queue_context, &self.resources.context)
            && self.queue.query_async().is_ok_and(|info| {
                info.result == GPU_RESULT_SUCCESS && info.max_pending_submissions != 0
            })
    }

    /// Submit owned GPU work without waiting for its completion or capacity.
    ///
    /// Requires a queue which genuinely advertises asynchronous ownership.
    /// Older synchronous GM20B queues return AsyncUnsupported before admission.
    /// It never substitutes a blocking submit for enqueue.
    fn submit<'r, 'data>(
        &mut self,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<Submission, sgfx_core::backend::SubmitError<IrSubmitError, Submission>> {
        if !Arc::ptr_eq(self.queue_context, &self.context.inner)
            || !Arc::ptr_eq(self.queue_context, &self.resources.context)
        {
            return Err(sgfx_core::backend::SubmitError::Rejected(
                IrSubmitError::ContextMismatch,
            ));
        }
        self.resources
            .submit_async(&self.context.inner, self.queue, commands)
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    fn texture(format: ir::TextureFormat, usage: ir::TextureUsage) -> ir::TextureDesc {
        ir::TextureDesc::new(format, ir::Extent2D::new(3, 2).unwrap(), usage).unwrap()
    }

    #[test]
    fn logical_readback_requires_copy_source_and_exact_packed_size() {
        let descriptor = texture(ir::TextureFormat::Rgba8Unorm, ir::TextureUsage::COPY_SRC);
        assert!(validate_texture_readback(descriptor, 24).is_ok());
        assert!(matches!(
            validate_texture_readback(descriptor, 23),
            Err(IrSubmitError::InvalidIr(ir::Error::OutOfBounds))
        ));
        let sampled = texture(ir::TextureFormat::Bgra8Unorm, ir::TextureUsage::SAMPLED);
        assert!(matches!(
            validate_texture_readback(sampled, 24),
            Err(IrSubmitError::InvalidIr(ir::Error::InvalidUsage))
        ));
        let narrow = texture(ir::TextureFormat::R8Unorm, ir::TextureUsage::COPY_SRC);
        assert!(validate_texture_readback(narrow, 6).is_ok());
        assert!(validate_texture_readback(narrow, 24).is_err());
    }

    #[test]
    fn logical_readback_supports_encoded_and_narrow_color_base_views() {
        for (format, bytes) in [
            (ir::TextureFormat::Rg8Unorm, 12),
            (ir::TextureFormat::Bgra8UnormSrgb, 24),
            (ir::TextureFormat::Rgba8UnormSrgb, 24),
        ] {
            assert!(validate_texture_readback(texture(format, ir::TextureUsage::COPY_SRC), bytes).is_ok());
        }
        let descriptor = texture(ir::TextureFormat::Bgra8Unorm, ir::TextureUsage::COPY_SRC);
        assert!(validate_texture_readback(descriptor.with_mip_level_count(2).unwrap(), 24).is_ok());
        assert!(validate_texture_readback(descriptor.with_array_layer_count(2).unwrap(), 24).is_ok());
        assert!(validate_texture_readback(descriptor.with_mip_level_count(2).unwrap(), 28).is_err());
        assert!(validate_texture_readback(descriptor.with_array_layer_count(2).unwrap(), 48).is_err());
    }

    #[test]
    fn physical_bgra_readback_preserves_rgba_channels_and_canonical_r8_red() {
        let physical = [5, 17, 203, 61, 241, 3, 19, 255];
        let mut rgba = [0; 8];
        crate::image_subresource::convert_bgra_readback(ir::TextureFormat::Rgba8Unorm,
            2, 1, &physical, 8, &mut rgba, 8).unwrap();
        assert_eq!(rgba, [203, 17, 5, 61, 19, 3, 241, 255]);
        let mut red = [0; 2];
        crate::image_subresource::convert_bgra_readback(ir::TextureFormat::R8Unorm,
            2, 1, &physical, 8, &mut red, 2).unwrap();
        assert_eq!(red, [61, 255]);
    }

    #[test]
    fn fixed_backend_does_not_advertise_unimplemented_execution() {
        let caps = Capabilities::from_execution_support(u32::MAX);
        assert!(!caps.supports_programmable_graphics());
        assert!(!caps.supports_texture_arrays());
        assert!(!caps.supports_image_mips());
        assert!(!caps.supports_depth_sampling());
    }
}

//! CPU transfers between logical color formats and physical BGRA storage.

use alloc::{borrow::Cow, sync::Arc, vec, vec::Vec};

use crate::{IrSubmitError, UnsupportedIrFeature, ir};

/// A native transfer keeps its immutable metadata and upload buffer attached
/// until the ordered dispatch job has observed GPU retirement.
pub(crate) struct ImageCommands {
    pub(crate) bytes: Vec<u8>,
    _buffers: Vec<Arc<crate::resource::RawBuffer>>,
    _images: Vec<Arc<crate::resource::RawImage>>,
    pub(crate) budget_bytes: usize,
}

impl ImageCommands {
    pub(crate) fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }
    pub(crate) fn submit(&self, queue: &gpu_raw::GpuQueue) -> Result<(), IrSubmitError> {
        queue.submit(&self.bytes)?;
        Ok(())
    }
    pub(crate) fn into_chunk(self) -> crate::dispatch::Chunk {
        crate::dispatch::Chunk::ImageCommands(self)
    }
}

pub(crate) struct LegacyImageUpload {
    image: Arc<crate::resource::RawImage>,
    pixels: Vec<u8>,
    stride: u32,
    rect: ir::PixelRect,
}
impl LegacyImageUpload {
    pub(crate) fn budget_bytes(&self) -> usize {
        self.pixels.len()
    }
    pub(crate) fn execute(&self) -> crate::HandleResult<()> {
        let row_bytes = self
            .rect
            .width()
            .checked_mul(4)
            .ok_or(crate::HandleError::InvalidParameter)?;
        let max_rows = legacy_upload_batch_rows(row_bytes, self.stride)?;
        let mut row = 0;
        while row < self.rect.height() {
            let rows = (self.rect.height() - row).min(max_rows);
            let start = row as usize * self.stride as usize;
            let span = (rows - 1) as usize * self.stride as usize + row_bytes as usize;
            self.image.upload_bgra(
                &self.pixels[start..start + span],
                self.stride,
                gpu_raw::GpuImageBgraRect::new(
                    self.rect.x(),
                    self.rect.y() + row,
                    self.rect.width(),
                    rows,
                ),
            )?;
            row += rows;
        }
        Ok(())
    }
}
fn legacy_upload_batch_rows(row_bytes: u32, stride: u32) -> crate::HandleResult<u32> {
    if row_bytes == 0 || stride < row_bytes || row_bytes > gpu_raw::GPU_MAX_IMAGE_UPLOAD_SIZE {
        return Err(crate::HandleError::InvalidParameter);
    }
    Ok((gpu_raw::GPU_MAX_IMAGE_UPLOAD_SIZE - row_bytes) / stride + 1)
}
pub(crate) enum ImageUpload {
    Native(ImageCommands),
    Legacy(LegacyImageUpload),
}
impl ImageUpload {
    pub(crate) fn budget_bytes(&self) -> usize {
        match self {
            Self::Native(command) => command.budget_bytes(),
            Self::Legacy(upload) => upload.budget_bytes(),
        }
    }
    pub(crate) fn submit(&self, queue: &gpu_raw::GpuQueue) -> Result<(), IrSubmitError> {
        match self {
            Self::Native(command) => command.submit(queue),
            Self::Legacy(upload) => upload.execute().map_err(Into::into),
        }
    }
    pub(crate) fn into_chunk(self) -> crate::dispatch::Chunk {
        match self {
            Self::Native(command) => command.into_chunk(),
            Self::Legacy(upload) => crate::dispatch::Chunk::LegacyImageUpload(upload),
        }
    }
}

pub(crate) fn prepare_upload(
    context: &Arc<crate::ContextInner>,
    image: Arc<crate::resource::RawImage>,
    write: ir::TextureWrite<'_>,
) -> Result<ImageUpload, IrSubmitError> {
    use maxwell_image_layout::wire::{Command, Filter, Opcode};
    let layout = image.subresources.ok_or(IrSubmitError::Unsupported(
        UnsupportedIrFeature::ImageLayout,
    ))?;
    let rect = write.destination();
    layout
        .transfer(
            write.mip_level(),
            write.array_layer(),
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
        )
        .map_err(|_| IrSubmitError::InvalidIr(ir::Error::OutOfBounds))?;
    let upload = prepare_bgra_upload(image.logical_format, write)?;
    if !context.device.capabilities.extended {
        if write.mip_level() != 0
            || write.array_layer() != 0
            || layout.descriptor.mip_levels != 1
            || layout.descriptor.array_layers != 1
        {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ImageLayout,
            ));
        }
        let size = (rect.height() as usize - 1)
            .checked_mul(upload.bytes_per_row as usize)
            .and_then(|n| n.checked_add(rect.width() as usize * 4))
            .ok_or(IrSubmitError::OutOfMemory)?;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(size)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        pixels.extend_from_slice(&upload.pixels[..size]);
        return Ok(ImageUpload::Legacy(LegacyImageUpload {
            image,
            pixels,
            stride: upload.bytes_per_row,
            rect,
        }));
    }

    // 902D pitch-linear surfaces require aligned rows. Logical client rows
    // may be tightly packed or have arbitrary padding, so normalize them.
    let row_bytes = rect
        .width()
        .checked_mul(4)
        .ok_or(IrSubmitError::OutOfMemory)?;
    let pitch = row_bytes
        .checked_add(31)
        .ok_or(IrSubmitError::OutOfMemory)?
        & !31;
    let span = u64::from(pitch) * u64::from(rect.height() - 1) + u64::from(row_bytes);
    let staging_size = usize::try_from(span).map_err(|_| IrSubmitError::OutOfMemory)?;
    let mut staging = Vec::new();
    staging
        .try_reserve_exact(staging_size)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    staging.resize(staging_size, 0);
    for row in 0..rect.height() as usize {
        let start = row * upload.bytes_per_row as usize;
        let dst = row * pitch as usize;
        staging[dst..dst + row_bytes as usize]
            .copy_from_slice(&upload.pixels[start..start + row_bytes as usize]);
    }
    let source = Arc::new(crate::resource::RawBuffer::create(context, span)?);
    source.write(0, &staging)?;
    let command = Command {
        opcode: Opcode::Upload,
        source_attachment: source.attachment_token,
        destination_attachment: image.attachment_token,
        source_offset: 0,
        source_stride: pitch,
        source_mip: 0,
        source_layer: 0,
        destination_mip: write.mip_level(),
        destination_layer: write.array_layer(),
        source_rect: [0, 0, rect.width(), rect.height()],
        destination_rect: [rect.x(), rect.y(), rect.width(), rect.height()],
        filter: Filter::Nearest,
        flip_x: false,
        flip_y: false,
    };
    encode_command(context, command, vec![source], vec![image], span as usize)
        .map(ImageUpload::Native)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_blit(
    context: &Arc<crate::ContextInner>,
    source: Arc<crate::resource::RawImage>,
    destination: Arc<crate::resource::RawImage>,
    source_mip: u32,
    destination_mip: u32,
    source_rect: ir::PixelRect,
    destination_rect: ir::PixelRect,
    filter: ir::FilterMode,
    flips: [bool; 2],
) -> Result<ImageCommands, IrSubmitError> {
    use maxwell_image_layout::wire::{Command, Filter, Opcode};
    if !physical_blit_compatible(source.logical_format, destination.logical_format)
        || (filter == ir::FilterMode::Linear
            && matches!(
                source.logical_format,
                ir::TextureFormat::Bgra8UnormSrgb | ir::TextureFormat::Rgba8UnormSrgb
            ))
    {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ImageLayout,
        ));
    }
    for (image, mip, rect) in [
        (&source, source_mip, source_rect),
        (&destination, destination_mip, destination_rect),
    ] {
        image
            .subresources
            .ok_or(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ImageLayout,
            ))?
            .transfer(mip, 0, rect.x(), rect.y(), rect.width(), rect.height())
            .map_err(|_| IrSubmitError::InvalidIr(ir::Error::OutOfBounds))?;
    }
    let command = Command {
        opcode: Opcode::Blit,
        source_attachment: source.attachment_token,
        destination_attachment: destination.attachment_token,
        source_offset: 0,
        source_stride: 0,
        source_mip,
        source_layer: 0,
        destination_mip,
        destination_layer: 0,
        source_rect: [
            source_rect.x(),
            source_rect.y(),
            source_rect.width(),
            source_rect.height(),
        ],
        destination_rect: [
            destination_rect.x(),
            destination_rect.y(),
            destination_rect.width(),
            destination_rect.height(),
        ],
        filter: match filter {
            ir::FilterMode::Nearest => Filter::Nearest,
            ir::FilterMode::Linear => Filter::Linear,
        },
        flip_x: flips[0],
        flip_y: flips[1],
    };
    encode_command(context, command, Vec::new(), vec![source, destination], 0)
}

fn physical_blit_compatible(source: ir::TextureFormat, destination: ir::TextureFormat) -> bool {
    use ir::TextureFormat::*;
    matches!(
        (source, destination),
        (Bgra8Unorm | Rgba8Unorm, Bgra8Unorm | Rgba8Unorm)
            | (Bgra8UnormSrgb, Bgra8UnormSrgb)
            | (Rgba8UnormSrgb, Rgba8UnormSrgb)
            | (R8Unorm, R8Unorm)
            | (Rg8Unorm, Rg8Unorm)
    )
}

fn encode_command(
    context: &Arc<crate::ContextInner>,
    command: maxwell_image_layout::wire::Command,
    mut buffers: Vec<Arc<crate::resource::RawBuffer>>,
    images: Vec<Arc<crate::resource::RawImage>>,
    payload_bytes: usize,
) -> Result<ImageCommands, IrSubmitError> {
    use maxwell_submit_wire::{
        ACCESS_READ, ACCESS_WRITE, AddressEncoding, Relocation, RelocationSource, Resource, Submit,
    };
    let mut record = [0; maxwell_image_layout::wire::RECORD_SIZE];
    command
        .encode_into(&mut record)
        .map_err(|_| IrSubmitError::InvalidIr(ir::Error::InvalidValue))?;
    let metadata = Arc::new(crate::resource::RawBuffer::create(
        context,
        record.len() as u64,
    )?);
    metadata.write(0, &record)?;
    let mut resources = vec![Resource {
        attachment_token: metadata.attachment_token,
        range_offset: 0,
        range_size: record.len() as u64,
        access: ACCESS_READ,
    }];
    for image in &images {
        resources.push(Resource {
            attachment_token: image.attachment_token,
            range_offset: 0,
            range_size: image.allocation_size(),
            access: if image.attachment_token == command.destination_attachment {
                ACCESS_WRITE
            } else {
                ACCESS_READ
            },
        });
    }
    for buffer in &buffers {
        resources.push(Resource {
            attachment_token: buffer.attachment_token,
            range_offset: 0,
            range_size: buffer.logical_size,
            access: ACCESS_READ,
        });
    }
    // Same-image blits between distinct levels need both read and write
    // authority in the single retained capability record.
    if command.source_attachment == command.destination_attachment {
        for resource in &mut resources {
            if resource.attachment_token == command.source_attachment {
                resource.access = ACCESS_READ | ACCESS_WRITE;
            }
        }
    }
    let mut words = [0; maxwell_submit_wire::OPERATION_WORDS];
    words[0] = 6;
    let relocations = [Relocation {
        commands_word_offset: 2,
        source: RelocationSource::Attachment(0),
        resource_offset: 0,
        required_size: record.len() as u64,
        access: ACCESS_READ,
        encoding: AddressEncoding::GpuVa64,
    }];
    let submit = Submit {
        commands: &words,
        resources: &resources,
        relocations: &relocations,
    };
    let mut bytes = vec![0; maxwell_submit_wire::encoded_len(submit)?];
    maxwell_submit_wire::encode(submit, &mut bytes)?;
    let budget_bytes = bytes
        .len()
        .checked_add(record.len())
        .and_then(|n| n.checked_add(payload_bytes))
        .ok_or(IrSubmitError::OutOfMemory)?;
    buffers.push(metadata);
    Ok(ImageCommands {
        bytes,
        _buffers: buffers,
        _images: images,
        budget_bytes,
    })
}

fn prepare_legacy_copy(
    source: Arc<crate::resource::RawImage>,
    destination: Arc<crate::resource::RawImage>,
    source_rect: ir::PixelRect,
    destination_rect: ir::PixelRect,
) -> Result<ImageCommands, IrSubmitError> {
    use maxwell_submit_wire::{
        ACCESS_READ, ACCESS_WRITE, AddressEncoding, Relocation, RelocationSource, Resource, Submit,
    };
    if !physical_blit_compatible(source.logical_format, destination.logical_format)
        || source.attachment_token == destination.attachment_token
        || source_rect.width() != destination_rect.width()
        || source_rect.height() != destination_rect.height()
    {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ImageLayout,
        ));
    }
    let mut words = [0; 64];
    words[0] = 3;
    for (image, rect, layout_index, rect_index, mode_index) in [
        (&destination, destination_rect, 10, 13, 52),
        (&source, source_rect, 29, 17, 53),
    ] {
        let layout = image.subresources.ok_or(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ImageLayout,
        ))?;
        if layout.descriptor.mip_levels != 1 || layout.descriptor.array_layers != 1 {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ImageLayout,
            ));
        }
        layout
            .transfer(0, 0, rect.x(), rect.y(), rect.width(), rect.height())
            .map_err(|_| ir::Error::OutOfBounds)?;
        words[layout_index..layout_index + 3].copy_from_slice(&[
            layout.descriptor.width,
            layout.descriptor.height,
            layout.levels[0].row_pitch,
        ]);
        words[rect_index..rect_index + 4].copy_from_slice(&[
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
        ]);
        words[mode_index] = match layout.kind {
            maxwell_image_layout::LayoutKind::Linear => 0,
            maxwell_image_layout::LayoutKind::BlockLinear { base_y_log2: 4, .. } => 0x40,
            maxwell_image_layout::LayoutKind::BlockLinear { base_y_log2, .. } => {
                0x100 | (u32::from(base_y_log2) << 4)
            }
        };
    }
    let resources = [
        Resource {
            attachment_token: destination.attachment_token,
            range_offset: 0,
            range_size: destination.allocation_size(),
            access: ACCESS_WRITE,
        },
        Resource {
            attachment_token: source.attachment_token,
            range_offset: 0,
            range_size: source.allocation_size(),
            access: ACCESS_READ,
        },
    ];
    let relocations = [
        Relocation {
            commands_word_offset: 2,
            source: RelocationSource::Attachment(0),
            resource_offset: 0,
            required_size: destination.allocation_size(),
            access: ACCESS_WRITE,
            encoding: AddressEncoding::GpuVa64,
        },
        Relocation {
            commands_word_offset: 4,
            source: RelocationSource::Attachment(1),
            resource_offset: 0,
            required_size: source.allocation_size(),
            access: ACCESS_READ,
            encoding: AddressEncoding::GpuVa64,
        },
    ];
    let submit = Submit {
        commands: &words,
        resources: &resources,
        relocations: &relocations,
    };
    let size = maxwell_submit_wire::encoded_len(submit)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    bytes.resize(size, 0);
    maxwell_submit_wire::encode(submit, &mut bytes)?;
    Ok(ImageCommands {
        budget_bytes: bytes.len(),
        bytes,
        _buffers: Vec::new(),
        _images: vec![source, destination],
    })
}

/// Typed conversions use the native shader path; byte-compatible transfers
/// retain the cheaper 902D path. Both variants own every referenced allocation.
pub(crate) enum ImageBlit {
    Physical(ImageCommands),
    Shader(crate::programmable::PreparedDraw),
}
impl ImageBlit {
    pub(crate) fn budget_bytes(&self) -> usize {
        match self {
            Self::Physical(command) => command.budget_bytes,
            Self::Shader(draw) => draw.budget_bytes(),
        }
    }
    pub(crate) fn submit(&self, queue: &gpu_raw::GpuQueue) -> Result<(), IrSubmitError> {
        match self {
            Self::Physical(command) => {
                queue.submit(&command.bytes)?;
            }
            Self::Shader(draw) => {
                queue.submit(&draw.execute()?)?;
            }
        }
        Ok(())
    }
    pub(crate) fn into_chunk(self) -> crate::dispatch::Chunk {
        match self {
            Self::Physical(command) => crate::dispatch::Chunk::ImageCommands(command),
            Self::Shader(draw) => crate::dispatch::Chunk::ProgrammableDraw(draw),
        }
    }
}
impl crate::resource::ContextResources {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_image_blit(
        &mut self,
        source: Arc<crate::resource::RawImage>,
        destination: Arc<crate::resource::RawImage>,
        source_mip: u32,
        destination_mip: u32,
        source_rect: ir::PixelRect,
        destination_rect: ir::PixelRect,
        filter: ir::FilterMode,
        flips: [bool; 2],
    ) -> Result<ImageBlit, IrSubmitError> {
        if !self.context.device.capabilities.extended {
            if source_mip != 0
                || destination_mip != 0
                || filter != ir::FilterMode::Nearest
                || flips != [false; 2]
            {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ImageLayout,
                ));
            }
            return prepare_legacy_copy(source, destination, source_rect, destination_rect)
                .map(ImageBlit::Physical);
        }
        if physical_blit_compatible(source.logical_format, destination.logical_format) {
            prepare_blit(
                &self.context,
                source,
                destination,
                source_mip,
                destination_mip,
                source_rect,
                destination_rect,
                filter,
                flips,
            )
            .map(ImageBlit::Physical)
        } else {
            self.prepare_narrow_blit(
                source,
                destination,
                source_mip,
                destination_mip,
                source_rect,
                destination_rect,
                filter,
                flips,
            )
            .map(ImageBlit::Shader)
        }
    }
}

pub(crate) struct PreparedBgraUpload<'data> {
    pub(crate) pixels: Cow<'data, [u8]>,
    pub(crate) bytes_per_row: u32,
}

/// Prepare physical BGRA rows, preserving encoded sRGB bytes without applying
/// a transfer function. R8 occupies physical alpha for the fixed alpha-mask
/// pipeline; ordinary sampling recovers logical red through the image view.
pub(crate) fn prepare_bgra_upload<'data>(
    format: ir::TextureFormat,
    write: ir::TextureWrite<'data>,
) -> Result<PreparedBgraUpload<'data>, IrSubmitError> {
    let bytes_per_pixel = color_bytes_per_pixel(format, UnsupportedIrFeature::TextureUpload)?;
    let area = write.destination();
    let source = RowLayout::new(
        area.width(),
        area.height(),
        bytes_per_pixel,
        write.bytes_per_row(),
        write.data().len(),
    )?;
    if matches!(
        format,
        ir::TextureFormat::Bgra8Unorm | ir::TextureFormat::Bgra8UnormSrgb
    ) {
        return Ok(PreparedBgraUpload {
            pixels: Cow::Borrowed(write.data()),
            bytes_per_row: write.bytes_per_row(),
        });
    }

    let stride = area.width().checked_mul(4).ok_or(ir::Error::Overflow)?;
    let length = usize::try_from(u64::from(stride) * u64::from(area.height()))
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(length)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    pixels.resize(length, 0);
    let destination_stride = usize::try_from(stride).map_err(|_| ir::Error::Overflow)?;
    for row in 0..source.rows {
        let source_row = source.row(write.data(), row);
        let destination_start = row * destination_stride;
        let destination = &mut pixels[destination_start..destination_start + destination_stride];
        match format {
            ir::TextureFormat::Rgba8Unorm | ir::TextureFormat::Rgba8UnormSrgb => {
                for (source, destination) in source_row
                    .chunks_exact(4)
                    .zip(destination.chunks_exact_mut(4))
                {
                    destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
                }
            }
            ir::TextureFormat::R8Unorm => {
                for (&red, destination) in source_row.iter().zip(destination.chunks_exact_mut(4)) {
                    destination.copy_from_slice(&[0, 0, 0, red]);
                }
            }
            ir::TextureFormat::Rg8Unorm => {
                for (source, destination) in source_row
                    .chunks_exact(2)
                    .zip(destination.chunks_exact_mut(4))
                {
                    destination.copy_from_slice(&[0, source[1], source[0], 255]);
                }
            }
            _ => unreachable!("BGRA returned above; non-color formats rejected above"),
        }
    }
    Ok(PreparedBgraUpload {
        pixels: Cow::Owned(pixels),
        bytes_per_row: stride,
    })
}

/// Convert physical BGRA rows to the requested logical format. Validate both
/// spans before writing, leave row padding untouched, and allow the last row
/// to omit its trailing padding.
pub(crate) fn convert_bgra_readback(
    format: ir::TextureFormat,
    width: u32,
    height: u32,
    source: &[u8],
    source_stride: u32,
    destination: &mut [u8],
    destination_stride: u32,
) -> Result<(), IrSubmitError> {
    let bytes_per_pixel = color_bytes_per_pixel(format, UnsupportedIrFeature::TextureReadback)?;
    let source_layout = RowLayout::new(width, height, 4, source_stride, source.len())?;
    let destination_layout = RowLayout::new(
        width,
        height,
        bytes_per_pixel,
        destination_stride,
        destination.len(),
    )?;
    for row in 0..source_layout.rows {
        let source = source_layout.row(source, row);
        let start = row * destination_layout.stride;
        let destination = &mut destination[start..start + destination_layout.row_bytes];
        match format {
            ir::TextureFormat::Bgra8Unorm | ir::TextureFormat::Bgra8UnormSrgb => {
                destination.copy_from_slice(source)
            }
            ir::TextureFormat::Rgba8Unorm | ir::TextureFormat::Rgba8UnormSrgb => {
                for (source, destination) in
                    source.chunks_exact(4).zip(destination.chunks_exact_mut(4))
                {
                    destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
                }
            }
            ir::TextureFormat::R8Unorm => {
                for (source, destination) in source.chunks_exact(4).zip(destination.iter_mut()) {
                    *destination = source[3];
                }
            }
            ir::TextureFormat::Rg8Unorm => {
                for (source, destination) in
                    source.chunks_exact(4).zip(destination.chunks_exact_mut(2))
                {
                    destination.copy_from_slice(&[source[2], source[1]]);
                }
            }
            _ => unreachable!("non-color formats rejected above"),
        }
    }
    Ok(())
}

fn color_bytes_per_pixel(
    format: ir::TextureFormat,
    feature: UnsupportedIrFeature,
) -> Result<u32, IrSubmitError> {
    match format {
        ir::TextureFormat::Bgra8Unorm
        | ir::TextureFormat::Bgra8UnormSrgb
        | ir::TextureFormat::Rgba8Unorm
        | ir::TextureFormat::Rgba8UnormSrgb => Ok(4),
        ir::TextureFormat::R8Unorm => Ok(1),
        ir::TextureFormat::Rg8Unorm => Ok(2),
        ir::TextureFormat::Nv12 | ir::TextureFormat::Depth32Float => {
            Err(IrSubmitError::Unsupported(feature))
        }
    }
}

struct RowLayout {
    row_bytes: usize,
    stride: usize,
    rows: usize,
}

impl RowLayout {
    fn new(
        width: u32,
        height: u32,
        bytes_per_pixel: u32,
        stride: u32,
        length: usize,
    ) -> Result<Self, IrSubmitError> {
        if width == 0 || height == 0 {
            return Err(ir::Error::InvalidValue.into());
        }
        let row_bytes = width
            .checked_mul(bytes_per_pixel)
            .ok_or(ir::Error::Overflow)?;
        if stride < row_bytes {
            return Err(ir::Error::InvalidValue.into());
        }
        let required = u64::from(stride) * u64::from(height - 1) + u64::from(row_bytes);
        if u64::try_from(length).map_err(|_| ir::Error::Overflow)? < required {
            return Err(ir::Error::OutOfBounds.into());
        }
        // A valid span also proves every row start and end fits usize.
        Ok(Self {
            row_bytes: usize::try_from(row_bytes).map_err(|_| ir::Error::Overflow)?,
            stride: usize::try_from(stride).map_err(|_| ir::Error::Overflow)?,
            rows: usize::try_from(height).map_err(|_| ir::Error::Overflow)?,
        })
    }

    fn row<'a>(&self, bytes: &'a [u8], row: usize) -> &'a [u8] {
        let start = row * self.stride;
        &bytes[start..start + self.row_bytes]
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    fn upload<'a>(
        format: ir::TextureFormat,
        width: u32,
        height: u32,
        stride: u32,
        data: &'a [u8],
    ) -> Result<PreparedBgraUpload<'a>, IrSubmitError> {
        prepare_bgra_upload(
            format,
            ir::TextureWrite::new(
                ir::PixelRect::new(7, 9, width, height).unwrap(),
                stride,
                data,
            )
            .unwrap(),
        )
    }

    #[test]
    fn rgba_upload_ignores_row_padding_and_accepts_short_final_row() {
        let pixels = [1, 2, 3, 4, 0xaa, 0xbb, 5, 6, 7, 8];
        let converted = upload(ir::TextureFormat::Rgba8Unorm, 1, 2, 6, &pixels).unwrap();
        assert_eq!(converted.bytes_per_row, 4);
        assert_eq!(&*converted.pixels, &[3, 2, 1, 4, 7, 6, 5, 8]);
        assert!(matches!(
            upload(ir::TextureFormat::Rgba8Unorm, 1, 2, 6, &pixels[..9]),
            Err(IrSubmitError::InvalidIr(ir::Error::OutOfBounds))
        ));
    }

    #[test]
    fn borrowed_bgra_upload_still_validates_the_complete_strided_span() {
        let pixels = [1, 2, 3, 4, 0xaa, 0xbb, 5, 6, 7, 8];
        let prepared = upload(ir::TextureFormat::Bgra8Unorm, 1, 2, 6, &pixels).unwrap();
        assert!(matches!(prepared.pixels, Cow::Borrowed(_)));
        assert_eq!(prepared.bytes_per_row, 6);
        assert!(matches!(
            upload(ir::TextureFormat::Bgra8Unorm, 1, 2, 6, &pixels[..9]),
            Err(IrSubmitError::InvalidIr(ir::Error::OutOfBounds))
        ));
        assert!(matches!(
            upload(ir::TextureFormat::Bgra8Unorm, 1, 2, 3, &pixels),
            Err(IrSubmitError::InvalidIr(ir::Error::InvalidValue))
        ));
    }

    #[test]
    fn rg8_preserves_red_green_and_expands_blue_zero_alpha_one() {
        let converted = upload(ir::TextureFormat::Rg8Unorm, 2, 1, 4, &[17, 29, 43, 61]).unwrap();
        assert_eq!(&*converted.pixels, &[0, 29, 17, 255, 0, 61, 43, 255]);
        let mut logical = [0u8; 4];
        convert_bgra_readback(
            ir::TextureFormat::Rg8Unorm,
            2,
            1,
            &converted.pixels,
            8,
            &mut logical,
            4,
        )
        .unwrap();
        assert_eq!(logical, [17, 29, 43, 61]);
    }

    #[test]
    fn r8_preserves_physical_alpha_and_reads_it_back() {
        let converted = upload(ir::TextureFormat::R8Unorm, 2, 1, 2, &[23, 149]).unwrap();
        assert_eq!(&*converted.pixels, &[0, 0, 0, 23, 0, 0, 0, 149]);
        let mut logical = [0u8; 2];
        convert_bgra_readback(
            ir::TextureFormat::R8Unorm,
            2,
            1,
            &[5, 7, 11, 23, 13, 17, 19, 149],
            8,
            &mut logical,
            2,
        )
        .unwrap();
        assert_eq!(logical, [23, 149]);
    }

    #[test]
    fn readback_preserves_destination_padding_and_accepts_short_final_rows() {
        let physical = [3, 2, 1, 4, 0xab, 0xcd, 7, 6, 5, 8];
        let mut logical = [0xee; 9];
        convert_bgra_readback(
            ir::TextureFormat::Rgba8Unorm,
            1,
            2,
            &physical,
            6,
            &mut logical,
            5,
        )
        .unwrap();
        assert_eq!(logical, [1, 2, 3, 4, 0xee, 5, 6, 7, 8]);
    }

    #[test]
    fn invalid_readback_spans_do_not_partially_modify_destination() {
        let physical = [3, 2, 1, 4, 7, 6, 5, 8];
        let mut logical = [0xee; 8];
        assert!(matches!(
            convert_bgra_readback(
                ir::TextureFormat::Rgba8Unorm,
                1,
                2,
                &physical[..7],
                4,
                &mut logical,
                4
            ),
            Err(IrSubmitError::InvalidIr(ir::Error::OutOfBounds))
        ));
        assert_eq!(logical, [0xee; 8]);
        assert!(matches!(
            convert_bgra_readback(
                ir::TextureFormat::Rgba8Unorm,
                1,
                2,
                &physical,
                4,
                &mut logical[..7],
                4
            ),
            Err(IrSubmitError::InvalidIr(ir::Error::OutOfBounds))
        ));
        assert_eq!(logical, [0xee; 8]);
    }

    #[test]
    fn srgb_transfers_preserve_encoded_bytes() {
        let rgba = [1, 127, 254, 33];
        let converted = upload(ir::TextureFormat::Rgba8UnormSrgb, 1, 1, 4, &rgba).unwrap();
        assert_eq!(&*converted.pixels, &[254, 127, 1, 33]);
        let mut logical = [0u8; 4];
        convert_bgra_readback(
            ir::TextureFormat::Rgba8UnormSrgb,
            1,
            1,
            &converted.pixels,
            4,
            &mut logical,
            4,
        )
        .unwrap();
        assert_eq!(logical, rgba);
        let prepared = upload(ir::TextureFormat::Bgra8UnormSrgb, 1, 1, 4, &rgba).unwrap();
        assert_eq!(&*prepared.pixels, &rgba);
        convert_bgra_readback(
            ir::TextureFormat::Bgra8UnormSrgb,
            1,
            1,
            &rgba,
            4,
            &mut logical,
            4,
        )
        .unwrap();
        assert_eq!(logical, rgba);
    }

    #[test]
    fn depth_and_multiplane_transfers_are_rejected() {
        for format in [ir::TextureFormat::Nv12, ir::TextureFormat::Depth32Float] {
            assert!(matches!(
                upload(format, 1, 1, 4, &[0; 4]),
                Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::TextureUpload
                ))
            ));
            assert!(matches!(
                convert_bgra_readback(format, 1, 1, &[0; 4], 4, &mut [0; 4], 4),
                Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::TextureReadback
                ))
            ));
        }
    }

    #[test]
    fn empty_extent_and_row_size_overflow_are_rejected() {
        assert!(matches!(
            convert_bgra_readback(ir::TextureFormat::R8Unorm, 0, 1, &[], 4, &mut [], 1),
            Err(IrSubmitError::InvalidIr(ir::Error::InvalidValue))
        ));
        assert!(matches!(
            convert_bgra_readback(ir::TextureFormat::R8Unorm, 1, 0, &[], 4, &mut [], 1),
            Err(IrSubmitError::InvalidIr(ir::Error::InvalidValue))
        ));
        assert!(matches!(
            convert_bgra_readback(
                ir::TextureFormat::Rgba8Unorm,
                u32::MAX,
                1,
                &[],
                u32::MAX,
                &mut [],
                u32::MAX
            ),
            Err(IrSubmitError::InvalidIr(ir::Error::Overflow))
        ));
    }
    #[test]
    fn legacy_upload_batches_keep_exact_strided_spans_inside_the_abi_limit() {
        let limit = gpu_raw::GPU_MAX_IMAGE_UPLOAD_SIZE;
        for (row_bytes, stride) in [(4, 4), (4, 32), (5120, 8192), (limit, limit)] {
            let rows = legacy_upload_batch_rows(row_bytes, stride).unwrap();
            assert!(
                u64::from(rows - 1) * u64::from(stride) + u64::from(row_bytes) <= u64::from(limit)
            );
            assert!(u64::from(rows) * u64::from(stride) + u64::from(row_bytes) > u64::from(limit));
        }
        assert!(legacy_upload_batch_rows(0, 4).is_err());
        assert!(legacy_upload_batch_rows(8, 4).is_err());
        assert!(legacy_upload_batch_rows(limit + 1, limit + 1).is_err());
    }
}

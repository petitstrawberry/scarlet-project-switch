//! Ordered CPU vertex fetch into the canonical fixed shader layouts.
//!
//! A task owns its source, index and destination allocations. Dispatch executes
//! it after the preceding native prefix retired, so queued uploads and copies
//! are visible without making logical submission wait for the GPU.

use crate::resource::RawBuffer;
use crate::{IrSubmitError, ir};
use alloc::{sync::Arc, vec, vec::Vec};

pub(crate) enum VertexSelection {
    Vertices {
        first: u32,
        count: u32,
    },
    Indexed {
        buffer: Arc<RawBuffer>,
        offset: u64,
        format: ir::IndexFormat,
        first: u32,
        count: u32,
        base: i32,
    },
}

pub(crate) struct NormalizeVertices {
    pub(crate) source: Arc<RawBuffer>,
    pub(crate) destination: Arc<RawBuffer>,
    pub(crate) source_offset: u64,
    pub(crate) descriptor: ir::RenderPipelineDesc,
    pub(crate) object: sgfx_codegen_maxwell::ObjectId,
    pub(crate) selections: Vec<VertexSelection>,
}

impl NormalizeVertices {
    pub(crate) fn budget_bytes(&self) -> Result<usize, IrSubmitError> {
        let scratch = self
            .selections
            .iter()
            .map(|selection| match selection {
                VertexSelection::Vertices { count, .. } => u64::from(*count) * 4,
                VertexSelection::Indexed { count, format, .. } => {
                    u64::from(*count) * (4 + format.byte_size())
                }
            })
            .max()
            .unwrap_or(0);
        let metadata = self
            .selections
            .capacity()
            .checked_mul(core::mem::size_of::<VertexSelection>())
            .and_then(|n| {
                n.checked_add(
                    core::mem::size_of::<Self>()
                        + ir::MAX_VERTEX_ATTRIBUTES * core::mem::size_of::<ir::VertexAttribute>(),
                )
            })
            .ok_or(IrSubmitError::SubmissionTooLarge)?;
        self.destination
            .logical_size
            .checked_add(metadata as u64)
            .and_then(|n| n.checked_add(scratch))
            .and_then(|n| n.checked_add(u64::from(self.descriptor.vertex_buffer().stride()) + 40))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(IrSubmitError::SubmissionTooLarge)
    }
    pub(crate) fn execute(&self) -> Result<(), IrSubmitError> {
        let count = vertex_capacity(
            self.source.logical_size,
            self.source_offset,
            self.descriptor.vertex_buffer().stride(),
        )?;
        let stride = canonical_descriptor(&self.descriptor)?
            .vertex_buffer()
            .stride();
        for selection in &self.selections {
            let indices = match selection {
                VertexSelection::Vertices {
                    first,
                    count: selected,
                } => {
                    let end = first.checked_add(*selected).ok_or(ir::Error::Overflow)?;
                    if u64::from(end) > count {
                        return Err(ir::Error::OutOfBounds.into());
                    }
                    let mut indices = Vec::new();
                    indices
                        .try_reserve_exact(*selected as usize)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    indices.extend(*first..end);
                    indices
                }
                VertexSelection::Indexed {
                    buffer,
                    offset,
                    format,
                    first,
                    count: selected,
                    base,
                } => {
                    let start = offset
                        .checked_add(
                            u64::from(*first)
                                .checked_mul(format.byte_size())
                                .ok_or(ir::Error::Overflow)?,
                        )
                        .ok_or(ir::Error::Overflow)?;
                    let length = u64::from(*selected)
                        .checked_mul(format.byte_size())
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or(ir::Error::Overflow)?;
                    let bytes = buffer.read(start, length)?;
                    resolved_indices(&bytes, *format, *base, count)?
                }
            };
            for index in indices {
                let source_offset = self
                    .source_offset
                    .checked_add(
                        u64::from(index)
                            .checked_mul(u64::from(self.descriptor.vertex_buffer().stride()))
                            .ok_or(ir::Error::Overflow)?,
                    )
                    .ok_or(ir::Error::Overflow)?;
                let record = self.source.read(
                    source_offset,
                    self.descriptor.vertex_buffer().stride() as usize,
                )?;
                let vertex = normalize_vertex(&record, &self.descriptor)?;
                self.destination
                    .write(u64::from(index) * u64::from(stride), &vertex)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn vertex_capacity(size: u64, offset: u64, stride: u32) -> Result<u64, IrSubmitError> {
    if stride == 0 {
        return Err(ir::Error::InvalidDescriptor.into());
    }
    let remaining = size.checked_sub(offset).ok_or(ir::Error::OutOfBounds)?;
    Ok(remaining / u64::from(stride))
}

/// The native fast paths retain their original bytes and avoid CPU vertex fetch.
pub(crate) fn native_layout(descriptor: &ir::RenderPipelineDesc) -> bool {
    use ir::{FragmentProgram as F, VertexFormat as V};
    let layout = descriptor.vertex_buffer();
    let attr = |location, format, offset| {
        layout
            .attributes()
            .iter()
            .any(|a| a.location() == location && a.format() == format && a.offset() == offset)
    };
    match (layout.stride(), descriptor.fragment()) {
        (16, F::Solid) => attr(0, V::Float32x2, 0),
        (16, F::Texture(_)) => attr(0, V::Float32x2, 0) && attr(1, V::Float32x2, 8),
        (24 | 32 | 40, F::Solid) => attr(0, V::Float32x4, 0),
        (24, F::Texture(_)) => attr(0, V::Float32x4, 0) && attr(1, V::Float32x2, 16),
        (28, F::VertexColor) => attr(0, V::Float32x4, 0) && attr(1, V::Float32x3, 16),
        (32 | 40, F::VertexColor) => attr(0, V::Float32x4, 0) && attr(1, V::Float32x4, 16),
        (40, F::TextureVertexColor(_)) => {
            attr(0, V::Float32x4, 0) && attr(1, V::Float32x4, 16) && attr(2, V::Float32x2, 32)
        }
        _ => false,
    }
}

pub(crate) fn canonical_descriptor(
    descriptor: &ir::RenderPipelineDesc,
) -> Result<ir::RenderPipelineDesc, IrSubmitError> {
    use ir::{FragmentProgram as F, VertexAttribute as A, VertexFormat as V};
    if native_layout(descriptor) {
        return Ok(descriptor.clone());
    }
    let (stride, attributes) = match descriptor.fragment() {
        F::Solid => (24, vec![A::new(0, V::Float32x4, 0)]),
        F::Texture(_) => (
            24,
            vec![A::new(0, V::Float32x4, 0), A::new(1, V::Float32x2, 16)],
        ),
        F::VertexColor => (
            40,
            vec![A::new(0, V::Float32x4, 0), A::new(1, V::Float32x4, 16)],
        ),
        F::TextureVertexColor(_) => (
            40,
            vec![
                A::new(0, V::Float32x4, 0),
                A::new(1, V::Float32x4, 16),
                A::new(2, V::Float32x2, 32),
            ],
        ),
    };
    let layout = ir::VertexBufferLayout::new(stride, attributes)?;
    let mut canonical = ir::RenderPipelineDesc::new(
        descriptor.target_format(),
        descriptor.topology(),
        layout,
        descriptor.fragment(),
        descriptor.blend(),
        descriptor.raster(),
    )?;
    if let Some(depth) = descriptor.depth_state() {
        canonical = canonical.with_depth_state(depth)?;
    }
    Ok(canonical)
}

pub(crate) fn resolved_indices(
    bytes: &[u8],
    format: ir::IndexFormat,
    base: i32,
    capacity: u64,
) -> Result<Vec<u32>, IrSubmitError> {
    let width = format.byte_size() as usize;
    if bytes.is_empty() || !bytes.len().is_multiple_of(width) {
        return Err(ir::Error::OutOfBounds.into());
    }
    let mut result = Vec::new();
    result
        .try_reserve_exact(bytes.len() / width)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for bytes in bytes.chunks_exact(width) {
        let index = match format {
            ir::IndexFormat::Uint16 => u32::from(u16::from_le_bytes(bytes.try_into().unwrap())),
            ir::IndexFormat::Uint32 => u32::from_le_bytes(bytes.try_into().unwrap()),
        };
        let resolved = i64::from(index) + i64::from(base);
        if resolved < 0 || resolved as u64 >= capacity || resolved > i64::from(u32::MAX) {
            return Err(ir::Error::OutOfBounds.into());
        }
        result.push(resolved as u32);
    }
    Ok(result)
}

fn floats(
    record: &[u8],
    attribute: ir::VertexAttribute,
    default: [f32; 4],
) -> Result<[f32; 4], IrSubmitError> {
    let count = match attribute.format() {
        ir::VertexFormat::Float32x2 => 2,
        ir::VertexFormat::Float32x3 => 3,
        ir::VertexFormat::Float32x4 => 4,
        ir::VertexFormat::Unorm8x4 => {
            let start = attribute.offset() as usize;
            let bytes = record
                .get(start..start.checked_add(4).ok_or(ir::Error::Overflow)?)
                .ok_or(ir::Error::OutOfBounds)?;
            return Ok(core::array::from_fn(|i| f32::from(bytes[i]) / 255.0));
        }
        _ => return Err(ir::Error::InvalidDescriptor.into()),
    };
    let mut result = default;
    for (i, component) in result.iter_mut().enumerate().take(count) {
        let start = (attribute.offset() as usize)
            .checked_add(i * 4)
            .ok_or(ir::Error::Overflow)?;
        *component = f32::from_le_bytes(
            record
                .get(start..start.checked_add(4).ok_or(ir::Error::Overflow)?)
                .ok_or(ir::Error::OutOfBounds)?
                .try_into()
                .unwrap(),
        );
        if !component.is_finite() {
            return Err(ir::Error::InvalidValue.into());
        }
    }
    Ok(result)
}

fn normalize_vertex(
    record: &[u8],
    descriptor: &ir::RenderPipelineDesc,
) -> Result<Vec<u8>, IrSubmitError> {
    use ir::FragmentProgram as F;
    if record.len() < descriptor.vertex_buffer().stride() as usize {
        return Err(ir::Error::OutOfBounds.into());
    }
    let attr = |location| {
        descriptor
            .vertex_buffer()
            .attributes()
            .iter()
            .copied()
            .find(|a| a.location() == location)
            .ok_or(ir::Error::InvalidDescriptor)
    };
    let position = floats(record, attr(0)?, [0.0, 0.0, 0.0, 1.0])?;
    let color = if matches!(
        descriptor.fragment(),
        F::VertexColor | F::TextureVertexColor(_)
    ) {
        let color = floats(record, attr(1)?, [1.0; 4])?;
        if !color.iter().all(|n| (0.0..=1.0).contains(n)) {
            return Err(ir::Error::InvalidValue.into());
        }
        color
    } else {
        [1.0; 4]
    };
    let uv = match descriptor.fragment() {
        F::Texture(_) => floats(record, attr(1)?, [0.0; 4])?,
        F::TextureVertexColor(_) => floats(record, attr(2)?, [0.0; 4])?,
        _ => [0.0; 4],
    };
    let stride = if matches!(
        descriptor.fragment(),
        F::VertexColor | F::TextureVertexColor(_)
    ) {
        40
    } else {
        24
    };
    let mut output = Vec::new();
    output
        .try_reserve_exact(stride)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for value in position {
        output.extend_from_slice(&value.to_le_bytes());
    }
    if stride == 40 {
        for value in color {
            output.extend_from_slice(&value.to_le_bytes());
        }
    }
    for value in &uv[..2] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    Ok(output)
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    fn descriptor(
        stride: u32,
        attributes: Vec<ir::VertexAttribute>,
        fragment: ir::FragmentProgram,
    ) -> ir::RenderPipelineDesc {
        ir::RenderPipelineDesc::new(
            ir::TextureFormat::Bgra8Unorm,
            ir::PrimitiveTopology::TriangleList,
            ir::VertexBufferLayout::new(stride, attributes).unwrap(),
            fragment,
            ir::BlendState::REPLACE,
            ir::RasterState::new(ir::CullMode::None, ir::FrontFace::CounterClockwise),
        )
        .unwrap()
    }
    fn decoded(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect()
    }
    #[test]
    fn padded_out_of_order_unorm_color_and_position_three() {
        let d = descriptor(
            32,
            vec![
                ir::VertexAttribute::new(1, ir::VertexFormat::Unorm8x4, 1),
                ir::VertexAttribute::new(0, ir::VertexFormat::Float32x3, 12),
            ],
            ir::FragmentProgram::VertexColor,
        );
        let mut record = vec![0xaa; 32];
        record[1..5].copy_from_slice(&[255, 128, 0, 64]);
        for (offset, value) in [(12, 2f32), (16, 3f32), (20, 4f32)] {
            record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let normalized = decoded(&normalize_vertex(&record, &d).unwrap());
        assert_eq!(&normalized[..4], &[2.0, 3.0, 4.0, 1.0]);
        assert_eq!(&normalized[4..8], &[1.0, 128.0 / 255.0, 0.0, 64.0 / 255.0]);
        assert_eq!(&normalized[8..], &[0.0, 0.0]);
        assert_eq!(
            canonical_descriptor(&d).unwrap().vertex_buffer().stride(),
            40
        );
    }
    #[test]
    fn position_two_and_separate_uv_with_arbitrary_padding() {
        let d = descriptor(
            36,
            vec![
                ir::VertexAttribute::new(0, ir::VertexFormat::Float32x2, 24),
                ir::VertexAttribute::new(1, ir::VertexFormat::Float32x2, 4),
            ],
            ir::FragmentProgram::Texture(ir::TextureSampleMode::Rgba),
        );
        let mut record = vec![0; 36];
        for (offset, value) in [(24, 5f32), (28, 6f32), (4, 0.25f32), (8, 0.75f32)] {
            record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(
            decoded(&normalize_vertex(&record, &d).unwrap()),
            [5.0, 6.0, 0.0, 1.0, 0.25, 0.75]
        );
        assert!(normalize_vertex(&record[..35], &d).is_err());
    }
    #[test]
    fn texture_vertex_color_fetches_four_component_position_and_three_component_color() {
        let d = descriptor(
            72,
            vec![
                ir::VertexAttribute::new(2, ir::VertexFormat::Float32x2, 56),
                ir::VertexAttribute::new(0, ir::VertexFormat::Float32x4, 32),
                ir::VertexAttribute::new(1, ir::VertexFormat::Float32x3, 8),
            ],
            ir::FragmentProgram::TextureVertexColor(ir::TextureSampleMode::RgbIgnoreAlpha),
        );
        let mut record = vec![0xcc; 72];
        for (offset, value) in [
            (32, 2f32),
            (36, 3f32),
            (40, 4f32),
            (44, 0.5f32),
            (8, 0.25f32),
            (12, 0.5f32),
            (16, 0.75f32),
            (56, 0.125f32),
            (60, 0.875f32),
        ] {
            record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(
            decoded(&normalize_vertex(&record, &d).unwrap()),
            [2.0, 3.0, 4.0, 0.5, 0.25, 0.5, 0.75, 1.0, 0.125, 0.875]
        );
        assert_eq!(canonical_descriptor(&d).unwrap().fragment(), d.fragment());
    }

    #[test]
    fn unaligned_four_component_color_keeps_alpha() {
        let d = descriptor(
            40,
            vec![
                ir::VertexAttribute::new(0, ir::VertexFormat::Float32x2, 3),
                ir::VertexAttribute::new(1, ir::VertexFormat::Float32x4, 19),
            ],
            ir::FragmentProgram::VertexColor,
        );
        let mut record = vec![0; 40];
        for (offset, value) in [
            (3, 1f32),
            (7, 2f32),
            (19, 0.25f32),
            (23, 0.5f32),
            (27, 0.75f32),
            (31, 0.125f32),
        ] {
            record[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(
            &decoded(&normalize_vertex(&record, &d).unwrap())[4..8],
            &[0.25, 0.5, 0.75, 0.125]
        );
    }

    #[test]
    fn negative_base_resolves_only_bounded_current_indices() {
        let bytes: Vec<u8> = [2u16, 3, 4]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            resolved_indices(&bytes, ir::IndexFormat::Uint16, -2, 3).unwrap(),
            [0, 1, 2]
        );
        assert!(resolved_indices(&bytes, ir::IndexFormat::Uint16, -3, 3).is_err());
        assert!(resolved_indices(&bytes, ir::IndexFormat::Uint16, -1, 3).is_err());
        assert!(resolved_indices(&[0xff; 4], ir::IndexFormat::Uint32, i32::MAX, u64::MAX).is_err());
        assert!(resolved_indices(&[0], ir::IndexFormat::Uint16, 0, 10).is_err());
    }
    #[test]
    fn buffer_capacity_never_wraps_or_counts_trailing_padding_as_a_record() {
        assert_eq!(vertex_capacity(85, 5, 36).unwrap(), 2);
        assert!(vertex_capacity(5, 6, 36).is_err());
        assert!(vertex_capacity(u64::MAX, 0, 0).is_err());
    }
    #[test]
    fn nonfinite_and_out_of_range_float_colors_are_rejected() {
        let d = descriptor(
            36,
            vec![
                ir::VertexAttribute::new(0, ir::VertexFormat::Float32x3, 0),
                ir::VertexAttribute::new(1, ir::VertexFormat::Float32x3, 20),
            ],
            ir::FragmentProgram::VertexColor,
        );
        let mut record = vec![0; 36];
        record[20..24].copy_from_slice(&2f32.to_le_bytes());
        assert!(normalize_vertex(&record, &d).is_err());
        record[20..24].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(normalize_vertex(&record, &d).is_err());
    }
}

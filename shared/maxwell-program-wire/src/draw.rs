// SPDX-License-Identifier: GPL-2.0-only
//! Canonical, address-free programmable draw metadata.
//!
//! This wire format carries context-local object tokens and byte ranges, never
//! GPU addresses. Parsing verifies representation; the kernel must separately
//! authorize every token and validate the referenced program and resource ranges.

pub const MAGIC: u32 = 0x444d_4753; // SGMD
pub const VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 256;
pub const MAX_PROGRAMS: usize = 2;
pub const MAX_STREAMS: usize = 8;
pub const MAX_ATTRIBUTES: usize = 16;
pub const MAX_UNIFORMS: usize = 64;
/// Each graphics stage has sixteen independent texture/storage units.
pub const MAX_IMAGES: usize = 32;
pub const MAX_TARGETS: usize = 8;

pub const PROGRAM_SIZE: usize = 32;
pub const STREAM_SIZE: usize = 40;
pub const ATTRIBUTE_SIZE: usize = 16;
pub const UNIFORM_SIZE: usize = 48;
pub const IMAGE_SIZE: usize = 64;
pub const TARGET_SIZE: usize = 48;
/// An image record describes a read-only typed buffer instead of an image.
/// `base_level` is its byte offset and `level_count` its byte size.
pub const SAMPLER_BUFFER: u32 = 1 << 15;
pub const SAMPLER_LOGICAL_SHIFT: u32 = 16;
pub const SAMPLER_DIMENSION_SHIFT: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Magic,
    Version,
    Size,
    Reserved,
    Count,
    Section,
    State,
    Duplicate,
    Range,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub token: u64,
    pub offset: u64,
    pub size: u64,
}

impl Range {
    pub fn end(self) -> Result<u64, Error> {
        if self.token == 0 || self.size == 0 {
            return Err(Error::Range);
        }
        self.offset.checked_add(self.size).ok_or(Error::Range)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Program {
    /// Vertex = 0, fragment = 4, matching SP_SELECT and the program package.
    pub stage: u32,
    pub range: Range,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    pub range: Range,
    pub stride: u32,
    /// Zero means per-vertex; one means per-instance.
    pub divisor: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attribute {
    pub location: u32,
    pub stream: u32,
    pub offset: u32,
    /// SGFX canonical vertex format (not a raw hardware method value).
    pub format: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Uniform {
    pub stage: u32,
    pub slot: u32,
    pub range: Range,
    /// Inline data is selected by token == 0. Offset is relative to inline data.
    pub inline_offset: u32,
    pub inline_size: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Image {
    pub token: u64,
    pub base_level: u32,
    pub level_count: u32,
    pub base_layer: u32,
    pub layer_count: u32,
    /// Canonical packed sampler flags; see `sampler_valid`.
    pub sampler: u32,
    pub min_lod: u32,
    pub max_lod: u32,
    pub lod_bias: u32,
    pub border_color: [u32; 4],
    pub stage: u32,
    pub slot: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub token: u64,
    pub level: u32,
    pub layer: u32,
    pub blend_enable: u32,
    pub write_mask: u32,
    /// Factors: 0 zero, 1 one, 2 source color, 3 inverse source color,
    /// 4 destination color, 5 inverse destination color, 6 source alpha,
    /// 7 inverse source alpha, 8 destination alpha, 9 inverse destination
    /// alpha, 10 constant color, 11 inverse constant color, 12 constant
    /// alpha, 13 inverse constant alpha, 14 source alpha saturation.
    pub src_color: u32,
    pub dst_color: u32,
    pub color_op: u32,
    pub src_alpha: u32,
    pub dst_alpha: u32,
    pub alpha_op: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct State {
    /// 0 triangles, 1 triangle strip, 2 triangle fan.
    pub topology: u32,
    pub count: u32,
    pub instances: u32,
    pub first: u32,
    pub base_vertex: i32,
    pub first_instance: u32,
    /// 0 none, 1 u16, 2 u32.
    pub index_format: u32,
    pub index: Range,
    pub viewport: [f32; 6],
    pub scissor: [u32; 4],
    /// Front-face bit 0, cull mode bits 1..2; all other bits reserved.
    pub raster: u32,
    /// Enable bit 0, write bit 1, compare function bits 2..4.
    pub depth: u32,
    pub blend_constant: [f32; 4],
    pub depth_token: u64,
    pub depth_level: u32,
    pub depth_layer: u32,
}

#[derive(Clone, Copy)]
struct Section {
    offset: usize,
    count: usize,
    stride: usize,
}

impl Section {
    fn bytes<'a>(self, data: &'a [u8], index: usize) -> Option<&'a [u8]> {
        if index >= self.count {
            return None;
        }
        let start = self.offset + index * self.stride;
        Some(&data[start..start + self.stride])
    }
}

pub struct Draw<'a> {
    data: &'a [u8],
    pub state: State,
    programs: Section,
    streams: Section,
    attributes: Section,
    uniforms: Section,
    images: Section,
    targets: Section,
    inline: &'a [u8],
}

fn u32_at(b: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(b[offset..offset + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(b[offset..offset + 8].try_into().unwrap())
}
fn range_at(b: &[u8], offset: usize) -> Range {
    Range {
        token: u64_at(b, offset),
        offset: u64_at(b, offset + 8),
        size: u64_at(b, offset + 16),
    }
}
fn floats<const N: usize>(b: &[u8], offset: usize) -> [f32; N] {
    core::array::from_fn(|i| f32::from_bits(u32_at(b, offset + i * 4)))
}
fn zero(b: &[u8]) -> Result<(), Error> {
    if b.iter().any(|v| *v != 0) {
        Err(Error::Reserved)
    } else {
        Ok(())
    }
}
fn stage(v: u32) -> bool {
    matches!(v, 0 | 4)
}

/// Flags: mag/min/mip linear bits 0/1/2, S/T/R wraps bits 3..8
/// (0 clamp, 1 repeat, 2 mirror), comparison enable bit 9/function 10..12.
pub fn sampler_valid(flags: u32) -> bool {
    flags
        & !(0x1fff
            | SAMPLER_BUFFER
            | (15 << SAMPLER_LOGICAL_SHIFT)
            | (7 << SAMPLER_DIMENSION_SHIFT))
        == 0
        && matches!((flags >> SAMPLER_LOGICAL_SHIFT) & 15, 0 | 2 | 3 | 4)
        && (flags >> SAMPLER_DIMENSION_SHIFT) <= 4
        && (3..=7).step_by(2).all(|shift| ((flags >> shift) & 3) < 3)
        && (flags & (1 << 9) != 0 || flags & (7 << 10) == 0)
}

impl<'a> Draw<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        if data.len() < HEADER_SIZE {
            return Err(Error::Truncated);
        }
        if u32_at(data, 0) != MAGIC {
            return Err(Error::Magic);
        }
        if u32_at(data, 4) != VERSION {
            return Err(Error::Version);
        }
        if u32_at(data, 8) as usize != data.len() {
            return Err(Error::Size);
        }
        zero(&data[12..16])?;
        zero(&data[40..44])?;
        zero(&data[208..HEADER_SIZE])?;
        let counts = [
            MAX_PROGRAMS,
            MAX_STREAMS,
            MAX_ATTRIBUTES,
            MAX_UNIFORMS,
            MAX_IMAGES,
            MAX_TARGETS,
        ];
        let strides = [
            PROGRAM_SIZE,
            STREAM_SIZE,
            ATTRIBUTE_SIZE,
            UNIFORM_SIZE,
            IMAGE_SIZE,
            TARGET_SIZE,
        ];
        let mut sections = [Section {
            offset: 0,
            count: 0,
            stride: 0,
        }; 6];
        let mut end = HEADER_SIZE;
        for i in 0..6 {
            let count = u32_at(data, 72 + i * 4) as usize;
            if count > counts[i] {
                return Err(Error::Count);
            }
            let offset = u32_at(data, 96 + i * 4) as usize;
            if offset != end {
                return Err(Error::Section);
            }
            end = end
                .checked_add(count.checked_mul(strides[i]).ok_or(Error::Size)?)
                .ok_or(Error::Size)?;
            if end > data.len() {
                return Err(Error::Truncated);
            }
            sections[i] = Section {
                offset,
                count,
                stride: strides[i],
            };
        }
        let inline_offset = u32_at(data, 120) as usize;
        let inline_len = u32_at(data, 124) as usize;
        if inline_offset != end || end.checked_add(inline_len) != Some(data.len()) {
            return Err(Error::Section);
        }
        let state = State {
            topology: u32_at(data, 16),
            count: u32_at(data, 20),
            instances: u32_at(data, 24),
            first: u32_at(data, 28),
            base_vertex: u32_at(data, 32) as i32,
            first_instance: u32_at(data, 36),
            index_format: u32_at(data, 44),
            index: range_at(data, 48),
            viewport: floats(data, 128),
            scissor: core::array::from_fn(|i| u32_at(data, 152 + 4 * i)),
            raster: u32_at(data, 168),
            depth: u32_at(data, 172),
            blend_constant: floats(data, 176),
            depth_token: u64_at(data, 192),
            depth_level: u32_at(data, 200),
            depth_layer: u32_at(data, 204),
        };
        if state.topology > 2
            || state.count == 0
            || state.instances == 0
            || state.count > 1_048_576
            || state.instances > 65_536
            || state.first.checked_add(state.count).is_none()
            || state.first_instance.checked_add(state.instances).is_none()
            || state.index_format > 2
            || state.raster & !7 != 0
            || (state.raster >> 1) == 3
            || state.depth & !31 != 0
            || !state
                .viewport
                .iter()
                .chain(state.blend_constant.iter())
                .all(|x| x.is_finite())
            || state.viewport[2] <= 0.
            || state.viewport[0] < 0.
            || state.viewport[1] < 0.
            || !(0. ..=1.).contains(&state.viewport[4])
            || !(0. ..=1.).contains(&state.viewport[5])
            || state.scissor[0].checked_add(state.scissor[2]).is_none()
            || state.scissor[1].checked_add(state.scissor[3]).is_none()
            || state.depth_token == 0
                && (state.depth != 0 || state.depth_level != 0 || state.depth_layer != 0)
        {
            return Err(Error::State);
        }
        if state.index_format == 0 {
            if state.index != Range::default() || state.base_vertex != 0 {
                return Err(Error::State);
            }
        } else {
            state.index.end()?;
            let width = if state.index_format == 1 { 2 } else { 4 };
            if state.index.offset % width != 0
                || u64::from(state.first + state.count)
                    .checked_mul(width)
                    .is_none_or(|n| n > state.index.size)
            {
                return Err(Error::Range);
            }
        }
        let draw = Self {
            data,
            state,
            programs: sections[0],
            streams: sections[1],
            attributes: sections[2],
            uniforms: sections[3],
            images: sections[4],
            targets: sections[5],
            inline: &data[inline_offset..],
        };
        if draw.program_count() != 2 || draw.target_count() == 0 && state.depth_token == 0 {
            return Err(Error::Count);
        }
        let mut stages = 0;
        for i in 0..draw.program_count() {
            let p = draw.program(i).unwrap();
            if !stage(p.stage) {
                return Err(Error::State);
            }
            let bit = 1 << p.stage;
            if stages & bit != 0 {
                return Err(Error::Duplicate);
            }
            stages |= bit;
            p.range.end()?;
            zero(&draw.programs.bytes(data, i).unwrap()[4..8])?;
        }
        for i in 0..draw.stream_count() {
            let s = draw.stream(i).unwrap();
            s.range.end()?;
            if s.stride == 0 || s.stride > 2048 || s.divisor > 1 {
                return Err(Error::State);
            }
            zero(&draw.streams.bytes(data, i).unwrap()[32..])?;
        }
        let mut locations = 0;
        for i in 0..draw.attribute_count() {
            let a = draw.attribute(i).unwrap();
            let size = vertex_format_size(a.format).ok_or(Error::State)?;
            if a.location >= 16
                || a.stream as usize >= draw.stream_count()
                || a.offset
                    .checked_add(size)
                    .is_none_or(|n| n > draw.stream(a.stream as usize).unwrap().stride)
            {
                return Err(Error::Range);
            }
            if locations & (1 << a.location) != 0 {
                return Err(Error::Duplicate);
            }
            locations |= 1 << a.location;
        }
        let mut uniform_slots = [0u32; 2];
        for i in 0..draw.uniform_count() {
            let u = draw.uniform(i).unwrap();
            if !stage(u.stage) || u.slot >= 32 {
                return Err(Error::State);
            }
            let s = usize::from(u.stage == 4);
            if uniform_slots[s] & (1 << u.slot) != 0 {
                return Err(Error::Duplicate);
            }
            uniform_slots[s] |= 1 << u.slot;
            if u.range.token == 0 {
                if u.range != Range::default()
                    || u.inline_size == 0
                    || u.inline_size > 65536
                    || (u.inline_offset as usize)
                        .checked_add(u.inline_size as usize)
                        .is_none_or(|n| n > draw.inline.len())
                {
                    return Err(Error::Range);
                }
            } else {
                u.range.end()?;
                if u.range.size > 65536 || u.inline_offset != 0 || u.inline_size != 0 {
                    return Err(Error::Range);
                }
            }
            zero(&draw.uniforms.bytes(data, i).unwrap()[40..])?;
        }
        for i in 0..draw.image_count() {
            let image = draw.image(i).unwrap();
            let lod = [
                f32::from_bits(image.min_lod),
                f32::from_bits(image.max_lod),
                f32::from_bits(image.lod_bias),
            ];
            if image.token == 0
                || !stage(image.stage)
                || image.slot >= 16
                || image.level_count == 0
                || image.layer_count == 0
                || image.base_level.checked_add(image.level_count).is_none()
                || image.base_layer.checked_add(image.layer_count).is_none()
                || !sampler_valid(image.sampler)
                || !lod.iter().all(|v| v.is_finite())
                || lod[0] < 0.
                || lod[1] < lod[0]
                || lod[1] > 15.
                || lod[2].abs() > 16.
                || !image
                    .border_color
                    .iter()
                    .all(|v| f32::from_bits(*v).is_finite())
            {
                return Err(Error::State);
            }
            if image.sampler & SAMPLER_BUFFER != 0
                && (image.base_layer != 0
                    || image.layer_count != 1
                    || image.base_level % 4 != 0
                    || image.level_count % 4 != 0
                    || image.sampler != SAMPLER_BUFFER)
            {
                return Err(Error::State);
            }
            for j in 0..i {
                let previous = draw.image(j).unwrap();
                if (image.stage, image.slot) == (previous.stage, previous.slot) {
                    return Err(Error::Duplicate);
                }
            }
        }
        for i in 0..draw.target_count() {
            let t = draw.target(i).unwrap();
            if t.token == 0
                || t.blend_enable > 1
                || t.write_mask & !15 != 0
                || [t.src_color, t.dst_color, t.src_alpha, t.dst_alpha]
                    .iter()
                    .any(|x| *x > 14)
                || t.color_op > 4
                || t.alpha_op > 4
            {
                return Err(Error::State);
            }
            for j in 0..i {
                let prev = draw.target(j).unwrap();
                if (t.token, t.level, t.layer) == (prev.token, prev.level, prev.layer) {
                    return Err(Error::Duplicate);
                }
            }
        }
        Ok(draw)
    }
    pub fn program_count(&self) -> usize {
        self.programs.count
    }
    pub fn stream_count(&self) -> usize {
        self.streams.count
    }
    pub fn attribute_count(&self) -> usize {
        self.attributes.count
    }
    pub fn uniform_count(&self) -> usize {
        self.uniforms.count
    }
    pub fn image_count(&self) -> usize {
        self.images.count
    }
    pub fn target_count(&self) -> usize {
        self.targets.count
    }
    pub fn inline_data(&self) -> &'a [u8] {
        self.inline
    }
    pub fn program(&self, i: usize) -> Option<Program> {
        let b = self.programs.bytes(self.data, i)?;
        Some(Program {
            stage: u32_at(b, 0),
            range: range_at(b, 8),
        })
    }
    pub fn stream(&self, i: usize) -> Option<Stream> {
        let b = self.streams.bytes(self.data, i)?;
        Some(Stream {
            range: range_at(b, 0),
            stride: u32_at(b, 24),
            divisor: u32_at(b, 28),
        })
    }
    pub fn attribute(&self, i: usize) -> Option<Attribute> {
        let b = self.attributes.bytes(self.data, i)?;
        Some(Attribute {
            location: u32_at(b, 0),
            stream: u32_at(b, 4),
            offset: u32_at(b, 8),
            format: u32_at(b, 12),
        })
    }
    pub fn uniform(&self, i: usize) -> Option<Uniform> {
        let b = self.uniforms.bytes(self.data, i)?;
        Some(Uniform {
            stage: u32_at(b, 0),
            slot: u32_at(b, 4),
            range: range_at(b, 8),
            inline_offset: u32_at(b, 32),
            inline_size: u32_at(b, 36),
        })
    }
    pub fn image(&self, i: usize) -> Option<Image> {
        let b = self.images.bytes(self.data, i)?;
        Some(Image {
            token: u64_at(b, 0),
            base_level: u32_at(b, 8),
            level_count: u32_at(b, 12),
            base_layer: u32_at(b, 16),
            layer_count: u32_at(b, 20),
            sampler: u32_at(b, 24),
            min_lod: u32_at(b, 28),
            max_lod: u32_at(b, 32),
            lod_bias: u32_at(b, 36),
            border_color: core::array::from_fn(|j| u32_at(b, 40 + j * 4)),
            stage: u32_at(b, 56),
            slot: u32_at(b, 60),
        })
    }
    pub fn target(&self, i: usize) -> Option<Target> {
        let b = self.targets.bytes(self.data, i)?;
        Some(Target {
            token: u64_at(b, 0),
            level: u32_at(b, 8),
            layer: u32_at(b, 12),
            blend_enable: u32_at(b, 16),
            write_mask: u32_at(b, 20),
            src_color: u32_at(b, 24),
            dst_color: u32_at(b, 28),
            color_op: u32_at(b, 32),
            src_alpha: u32_at(b, 36),
            dst_alpha: u32_at(b, 40),
            alpha_op: u32_at(b, 44),
        })
    }
}

/// 0..3 float32 scalar/vector, 4..7 uint32, 8..11 sint32,
/// 12 unorm8x4, 13 snorm8x4, 14 uint8x4, 15 sint8x4,
/// 16 unorm16x2, 17 snorm16x2, 18 float16x2, 19 float16x4.
pub fn vertex_format_size(format: u32) -> Option<u32> {
    match format {
        0..=11 => Some((format % 4 + 1) * 4),
        12..=18 => Some(4),
        19 | 20 => Some(8),
        21 => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn put(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn valid() -> Vec<u8> {
        let mut bytes = vec![0; HEADER_SIZE + 2 * PROGRAM_SIZE + TARGET_SIZE];
        let len = bytes.len();
        put(&mut bytes, 0, MAGIC);
        put(&mut bytes, 4, VERSION);
        put(&mut bytes, 8, len as u32);
        put(&mut bytes, 20, 3);
        put(&mut bytes, 24, 1);
        put(&mut bytes, 72, 2);
        put(&mut bytes, 92, 1);
        for (i, off) in [256, 320, 320, 320, 320, 320].into_iter().enumerate() {
            put(&mut bytes, 96 + i * 4, off);
        }
        put(&mut bytes, 120, len as u32);
        put(&mut bytes, 136, 16f32.to_bits());
        put(&mut bytes, 140, 16f32.to_bits());
        put(&mut bytes, 148, 1f32.to_bits());
        put(&mut bytes, 160, 16);
        put(&mut bytes, 164, 16);
        put(&mut bytes, 264, 1);
        put(&mut bytes, 280, 512);
        put(&mut bytes, 288, 4);
        put(&mut bytes, 296, 2);
        put(&mut bytes, 312, 512);
        put(&mut bytes, 320, 3);
        put(&mut bytes, 340, 15);
        bytes
    }

    #[test]
    fn parses_canonical_triangle() {
        let bytes = valid();
        let draw = Draw::parse(&bytes).unwrap();
        assert_eq!(draw.program_count(), 2);
        assert_eq!(draw.program(1).unwrap().stage, 4);
        assert_eq!(draw.target(0).unwrap().token, 3);
        assert_eq!(draw.state.viewport, [0., 0., 16., 16., 0., 1.]);
    }

    #[test]
    fn rejects_every_truncation_and_length_mismatch() {
        let bytes = valid();
        for end in 0..bytes.len() {
            assert!(Draw::parse(&bytes[..end]).is_err());
        }
        let mut bytes = bytes;
        bytes.push(0);
        assert!(matches!(Draw::parse(&bytes), Err(Error::Size)));
    }

    #[test]
    fn rejects_section_alias_reserved_nan_and_duplicate_stage() {
        for (offset, value, error) in [
            (100, 256, Error::Section),
            (12, 1, Error::Reserved),
            (128, f32::NAN.to_bits(), Error::State),
            (288, 0, Error::Duplicate),
            (272, u32::MAX, Error::Range),
        ] {
            let mut bytes = valid();
            put(&mut bytes, offset, value);
            // Overflow a range rather than just choosing a large valid offset.
            if offset == 272 {
                put(&mut bytes, 276, u32::MAX);
            }
            assert!(
                matches!(Draw::parse(&bytes),Err(e) if e == error),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn index_range_cannot_escape_authorized_view() {
        let mut bytes = valid();
        put(&mut bytes, 44, 1);
        put(&mut bytes, 48, 7);
        put(&mut bytes, 64, 6);
        assert!(Draw::parse(&bytes).is_ok());
        put(&mut bytes, 28, 1);
        assert!(matches!(Draw::parse(&bytes), Err(Error::Range)));
        put(&mut bytes, 28, 0);
        put(&mut bytes, 56, 1);
        assert!(matches!(Draw::parse(&bytes), Err(Error::Range)));
    }

    #[test]
    fn sampler_and_vertex_format_are_canonical() {
        for s in 0..3 {
            for t in 0..3 {
                for r in 0..3 {
                    assert!(sampler_valid((s << 3) | (t << 5) | (r << 7)));
                }
            }
        }
        assert!(!sampler_valid(3 << 3));
        assert!(!sampler_valid(1 << 14));
        assert!(!sampler_valid(1 << 10));
        assert!(sampler_valid((1 << 9) | (7 << 10)));
        assert_eq!(vertex_format_size(3), Some(16));
        assert_eq!(vertex_format_size(19), Some(8));
        assert_eq!(vertex_format_size(20), Some(8));
        assert_eq!(vertex_format_size(22), None);
    }
}

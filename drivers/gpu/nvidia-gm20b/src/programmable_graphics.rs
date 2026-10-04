// SPDX-License-Identifier: GPL-2.0-only
//! Capability-checked programmable graphics preparation and trusted Maxwell B
//! methods. Mesa e881540692daac6532cefec76699f7a025563767 nvc0_shader_state,
//! nvc0_vbo, nvc0_state_validate, nvc0_tex and nv50_state are the reference.
//! All code, uniforms, indices, TICs and TSCs live in a private immutable arena.

use crate::{method::*, program::Snapshot};
use alloc::{boxed::Box, vec::Vec};
use maxwell_program_wire::{
    Limits, Stage,
    draw::{self, Draw, Range},
};

const MAX_ARENA: usize = 8 * 1024 * 1024;
const MAX_METHOD_WORDS: usize = 240 * 1024;
const TIC_OFFSET: usize = 0;
const TSC_OFFSET: usize = draw::MAX_IMAGES * 32;
const UNIFORM_START: usize = 4096;
// The graphics frontend uses CB0..CB15, including the private CB15 table.
// A five-bit CB_BIND field does not make all 32 indices valid on GM20B:
// unbinding slot 18 (0x120) faults before the startup draw can execute.
const GRAPHICS_CB_COUNT: usize = 16;

/// Addresses and layout supplied only by the context's checked attachments.
#[derive(Clone, Copy)]
pub struct ImageView {
    /// Physical LOD0 base for sampled views; selected mip for attachments.
    pub address: u64,
    /// The selected view's interval, used for alias checking.
    pub range_address: u64,
    pub span_size: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub tiled: bool,
    pub tile_y: u8,
    pub array_pitch: u32,
    pub mip_levels: u32,
    pub layers: u32,
    pub cube: bool,
    pub depth: bool,
}

#[derive(Clone, Copy)]
pub enum ImageAccess {
    Sample,
    ColorTarget,
    Depth { write: bool },
}

pub trait Authority {
    /// The returned bytes are the immutable enqueue-time snapshot, not the
    /// public backing or a mutable GPU mapping.
    fn buffer_bytes(&self, range: Range) -> Result<&[u8], &'static str>;
    fn buffer_address(&self, range: Range) -> Result<u64, &'static str>;
    fn image(
        &self,
        token: u64,
        level: u32,
        level_count: u32,
        layer: u32,
        layer_count: u32,
        access: ImageAccess,
    ) -> Result<ImageView, &'static str>;
}

struct Patch {
    high: usize,
    offset: usize,
}
pub struct PreparedDraw {
    pub arena: Vec<u8>,
    words: Vec<u32>,
    patches: Vec<Patch>,
}
pub struct PublishedDraw {
    pub words: Vec<u32>,
}

impl PreparedDraw {
    pub fn publish(&self, address: u64) -> Result<PublishedDraw, &'static str> {
        let mut words = Vec::new();
        words
            .try_reserve_exact(self.words.len())
            .map_err(|_| "programmable method allocation failed")?;
        words.extend_from_slice(&self.words);
        for patch in &self.patches {
            let a = address
                .checked_add(patch.offset as u64)
                .ok_or("programmable arena address overflow")?;
            words[patch.high] = (a >> 32) as u32;
            words[patch.high + 1] = a as u32;
        }
        Ok(PublishedDraw { words })
    }
}

struct Builder {
    prepared: PreparedDraw,
}
impl Builder {
    fn new() -> Self {
        Self {
            prepared: PreparedDraw {
                arena: Vec::new(),
                words: Vec::new(),
                patches: Vec::new(),
            },
        }
    }
    fn alloc(&mut self, bytes: &[u8], alignment: usize) -> Result<usize, &'static str> {
        let start = self
            .prepared
            .arena
            .len()
            .checked_add(alignment - 1)
            .ok_or("programmable arena overflow")?
            & !(alignment - 1);
        let end = start
            .checked_add(bytes.len())
            .ok_or("programmable arena overflow")?;
        if end > MAX_ARENA {
            return Err("programmable arena budget exceeded");
        }
        self.prepared
            .arena
            .try_reserve(end - self.prepared.arena.len())
            .map_err(|_| "programmable arena allocation failed")?;
        self.prepared.arena.resize(end, 0);
        self.prepared.arena[start..end].copy_from_slice(bytes);
        Ok(start)
    }
    fn zero(&mut self, size: usize, alignment: usize) -> Result<usize, &'static str> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| "programmable arena allocation failed")?;
        bytes.resize(size, 0);
        self.alloc(&bytes, alignment)
    }
    fn method(&mut self, method: u32, data: &[u32]) -> Result<usize, &'static str> {
        if data.is_empty()
            || data.len() > 0x1fff
            || method & 3 != 0
            || method >= 0x8000
            || self
                .prepared
                .words
                .len()
                .checked_add(data.len() + 1)
                .is_none_or(|n| n > MAX_METHOD_WORDS)
        {
            return Err("programmable method budget exceeded");
        }
        self.prepared
            .words
            .try_reserve(data.len() + 1)
            .map_err(|_| "programmable method allocation failed")?;
        self.prepared
            .words
            .push(1 << 29 | (data.len() as u32) << 16 | method >> 2);
        let first = self.prepared.words.len();
        self.prepared.words.extend_from_slice(data);
        Ok(first)
    }
    fn one(&mut self, method: u32, value: u32) -> Result<(), &'static str> {
        self.method(method, &[value]).map(|_| ())
    }
    fn address(&mut self, method: u32, address: u64) -> Result<(), &'static str> {
        self.method(method, &[(address >> 32) as u32, address as u32])
            .map(|_| ())
    }
    fn private(&mut self, method: u32, offset: usize, tail: &[u32]) -> Result<(), &'static str> {
        let mut data = Vec::new();
        data.try_reserve(tail.len() + 2)
            .map_err(|_| "programmable patch allocation failed")?;
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(tail);
        let high = self.method(method, &data)?;
        self.prepared
            .patches
            .try_reserve(1)
            .map_err(|_| "programmable patch allocation failed")?;
        self.prepared.patches.push(Patch { high, offset });
        Ok(())
    }
    fn cb(&mut self, stage: u32, slot: u32, offset: usize, size: u32) -> Result<(), &'static str> {
        let first = self.method(CB_SIZE, &[size, 0, 0])?;
        self.prepared
            .patches
            .try_reserve(1)
            .map_err(|_| "programmable patch allocation failed")?;
        self.prepared.patches.push(Patch {
            high: first + 1,
            offset,
        });
        self.one(CB_BIND + stage * 0x20, slot << 4 | 1)
    }
}

fn copy(bytes: &[u8]) -> Result<Box<[u8]>, &'static str> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(bytes.len())
        .map_err(|_| "programmable snapshot allocation failed")?;
    result.extend_from_slice(bytes);
    Ok(result.into_boxed_slice())
}

fn image_overlaps(a: ImageView, b: ImageView) -> bool {
    a.range_address
        .checked_add(a.span_size)
        .zip(b.range_address.checked_add(b.span_size))
        .is_none_or(|(a_end, b_end)| a.range_address < b_end && b.range_address < a_end)
}

fn attribute_format(a: draw::Attribute) -> Result<u32, &'static str> {
    // nvc0_3d.xml.h typed formats, never accepted as raw user method words.
    let (size, kind) = match a.format {
        0..=11 => (
            [0x02400000, 0x00800000, 0x00400000, 0x00200000][(a.format % 4) as usize],
            match a.format / 4 {
                0 => 0x38000000,
                1 => 0x20000000,
                _ => 0x18000000,
            },
        ),
        12 => (0x01400000, 0x10000000),
        13 => (0x01400000, 0x08000000),
        14 => (0x01400000, 0x20000000),
        15 => (0x01400000, 0x18000000),
        16 => (0x01e00000, 0x10000000),
        17 => (0x01e00000, 0x08000000),
        18 => (0x01e00000, 0x38000000),
        19 => (0x00600000, 0x38000000),
        20 => (0x00600000, 0x18000000),
        21 => (0x06000000, 0x08000000),
        _ => return Err("programmable vertex format unsupported"),
    };
    Ok(size | kind | a.offset << 7 | a.stream)
}

pub fn indexed_bounds(
    bytes: &[u8],
    format: u32,
    first: u32,
    count: u32,
    base: i32,
) -> Result<(u32, u32), &'static str> {
    let width = match format {
        1 => 2,
        2 => 4,
        _ => return Err("programmable index format invalid"),
    };
    let start = (first as usize)
        .checked_mul(width)
        .ok_or("programmable index offset overflow")?;
    let end = (first as usize)
        .checked_add(count as usize)
        .and_then(|n| n.checked_mul(width))
        .ok_or("programmable index range overflow")?;
    let bytes = bytes
        .get(start..end)
        .ok_or("programmable indices exceed authorized view")?;
    let (mut min, mut max) = (u32::MAX, 0);
    for value in bytes.chunks_exact(width) {
        let index = if width == 2 {
            u16::from_le_bytes(value.try_into().unwrap()) as u32
        } else {
            u32::from_le_bytes(value.try_into().unwrap())
        };
        let resolved = i64::from(index) + i64::from(base);
        let resolved =
            u32::try_from(resolved).map_err(|_| "programmable signed index exceeds vertex view")?;
        min = min.min(resolved);
        max = max.max(resolved);
    }
    Ok((min, max))
}

fn sampler(image: draw::Image) -> [u32; 8] {
    let flags = image.sampler;
    let wrap = |shift: u32| match (flags >> shift) & 3 {
        0 => 2u32,
        1 => 0,
        _ => 1,
    };
    let mut tsc = [0u32; 8];
    tsc[0] = 0x26000 | wrap(3) | wrap(5) << 3 | wrap(7) << 6 | flags & 0x1e00;
    tsc[1] = if flags & 1 != 0 { 2 } else { 1 }
        | if flags & 2 != 0 { 0x20 } else { 0x10 }
        | if image.level_count == 1 || flags & draw::SAMPLER_BUFFER != 0 {
            0x40
        } else if flags & 4 != 0 {
            0xc0
        } else {
            0x80
        };
    tsc[1] |= (((f32::from_bits(image.lod_bias) * 256.).clamp(-4096., 3840.) as i32 as u32)
        & 0x1fff)
        << 12;
    tsc[2] = ((f32::from_bits(image.min_lod) * 256.) as u32 & 0xfff)
        | ((f32::from_bits(image.max_lod) * 256.) as u32 & 0xfff) << 12;
    tsc[4..8].copy_from_slice(&image.border_color);
    tsc
}

fn tic(image: ImageView, view: draw::Image) -> Result<[u32; 8], &'static str> {
    use maxwell_image_layout::descriptor::{self, Dimension, Texture};
    let logical = (view.sampler >> draw::SAMPLER_LOGICAL_SHIFT) & 15;
    if image.depth != (logical == 4) {
        return Err("programmable logical depth view mismatch");
    }
    let format = match logical {
        0 => 0,
        2 => 2,
        3 => 3,
        4 => 6,
        _ => return Err("programmable logical image format unsupported"),
    };
    let dimension = match view.sampler >> draw::SAMPLER_DIMENSION_SHIFT {
        0 => Dimension::D2,
        // The shared VirGL frontend promotes logical 1D samples/fetches to
        // 2D with Y=0.5/0, and 1D arrays to 2D arrays with the layer in Z.
        // Match those actual TEX/TLD dimensions while checking logical shape.
        1 if image.height == 1 => Dimension::D2,
        2 if image.height == 1 => Dimension::D2Array,
        3 => Dimension::D2Array,
        4 if image.cube => Dimension::Cube,
        _ => return Err("programmable texture dimension invalid"),
    };
    descriptor::texture(Texture {
        address: image.address,
        width: image.width,
        height: image.height,
        row_pitch: image.pitch,
        tile_y_log2: if image.tiled {
            Some(image.tile_y)
        } else {
            None
        },
        format,
        alpha_mask: false,
        mip_levels: image.mip_levels,
        first_mip: view.base_level,
        last_mip: view.base_level + view.level_count - 1,
        layers: image.layers,
        dimension,
    })
    .map_err(|_| "programmable texture descriptor invalid")
}

pub fn prepare(metadata: &[u8], authority: &impl Authority) -> Result<PreparedDraw, &'static str> {
    let draw = Draw::parse(metadata).map_err(|_| "invalid programmable draw metadata")?;
    let state = draw.state;
    let mut b = Builder::new();
    b.zero(UNIFORM_START, 4096)?;
    let mut cb_sizes = [[0u32; 32]; 2];
    let mut cb_offsets = [[0usize; 32]; 2];
    for i in 0..draw.uniform_count() {
        let u = draw.uniform(i).unwrap();
        if u.slot as usize >= GRAPHICS_CB_COUNT {
            return Err("programmable constant buffer slot unsupported");
        }
        let stage = usize::from(u.stage == 4);
        let bytes = if u.range.token == 0 {
            &draw.inline_data()
                [u.inline_offset as usize..(u.inline_offset + u.inline_size) as usize]
        } else {
            authority.buffer_bytes(u.range)?
        };
        if bytes.len() % 4 != 0
            || bytes.len() > 16384 && u.slot != 15
            || u.slot == 15 && bytes.len() > 128
        {
            return Err("programmable constant buffer size invalid");
        }
        cb_sizes[stage][u.slot as usize] = bytes.len() as u32;
        cb_offsets[stage][u.slot as usize] = b.alloc(bytes, 256)?;
    }
    // Auxiliary handles are kernel-generated, and every unbound slot is invalid.
    for stage in 0..2 {
        if cb_sizes[stage][15] == 0 {
            cb_offsets[stage][15] = b.zero(128, 256)?;
            cb_sizes[stage][15] = 128;
        }
        if cb_sizes[stage][15] < 96 {
            return Err("programmable auxiliary buffer too small");
        }
        let offset = cb_offsets[stage][15];
        b.prepared.arena[offset..offset + cb_sizes[stage][15] as usize].fill(0);
        b.prepared.arena[offset..offset + 4]
            .copy_from_slice(&(state.base_vertex as u32).to_le_bytes());
        b.prepared.arena[offset + 4..offset + 8]
            .copy_from_slice(&state.first_instance.to_le_bytes());
        b.prepared.arena[offset + 0x20..offset + 0x60].fill(0xff);
    }
    let mut resource_masks = [0u64; 2];
    let mut sampled = Vec::new();
    sampled
        .try_reserve_exact(draw.image_count())
        .map_err(|_| "programmable sampled view allocation failed")?;
    for i in 0..draw.image_count() {
        let view = draw.image(i).unwrap();
        let stage = usize::from(view.stage == 4);
        resource_masks[stage] |= 1u64 << view.slot;
        let desc = if view.sampler & draw::SAMPLER_BUFFER != 0 {
            let range = Range {
                token: view.token,
                offset: view.base_level as u64,
                size: view.level_count as u64,
            };
            let address = authority.buffer_address(range)?;
            let width = view.level_count / 4 - 1;
            // GM107 R32_UINT: UINT channels, X=R and other components constant.
            [
                0x0f | 4 << 7 | 4 << 10 | 4 << 13 | 4 << 16 | 2 << 19 | 6 << 22 | 6 << 25 | 7 << 28,
                address as u32,
                (address >> 32) as u32,
                0x10000 | (width >> 16),
                0xe3000000 | (width & 0xffff),
                0,
                0,
                0,
            ]
        } else {
            let image = authority.image(
                view.token,
                view.base_level,
                view.level_count,
                view.base_layer,
                view.layer_count,
                ImageAccess::Sample,
            )?;
            sampled.push(image);
            if view.sampler & (1 << 9) != 0 && !image.depth {
                return Err("programmable compare sampler requires depth");
            }
            tic(image, view)?
        };
        for (j, word) in desc.into_iter().enumerate() {
            b.prepared.arena[TIC_OFFSET + i * 32 + j * 4..TIC_OFFSET + i * 32 + j * 4 + 4]
                .copy_from_slice(&word.to_le_bytes());
        }
        for (j, word) in sampler(view).into_iter().enumerate() {
            b.prepared.arena[TSC_OFFSET + i * 32 + j * 4..TSC_OFFSET + i * 32 + j * 4 + 4]
                .copy_from_slice(&word.to_le_bytes());
        }
        let handle = (i as u32) | ((i as u32) << 20);
        let offset = cb_offsets[stage][15] + 0x20 + view.slot as usize * 4;
        b.prepared.arena[offset..offset + 4].copy_from_slice(&handle.to_le_bytes());
    }
    let mut stages: [Option<Snapshot>; 2] = [None, None];
    for i in 0..draw.program_count() {
        let p = draw.program(i).unwrap();
        let stage = usize::from(p.stage == 4);
        let shader = Snapshot::from_private_bytes(
            copy(authority.buffer_bytes(p.range)?)?,
            &Limits {
                cb_sizes: cb_sizes[stage],
                resource_mask: resource_masks[stage],
            },
        )
        .map_err(|_| "unsafe programmable Maxwell stage package")?;
        if shader.stage()
            != if stage == 0 {
                Stage::Vertex
            } else {
                Stage::Fragment
            }
        {
            return Err("programmable stage package mismatch");
        }
        stages[stage] = Some(shader);
    }
    let vs = stages[0].as_ref().unwrap();
    let fs = stages[1].as_ref().unwrap();
    if vs.attr_in()[2..].iter().any(|v| *v != 0) {
        return Err("programmable vertex inputs exceed sixteen attributes");
    }
    let mut locations = 0u32;
    for i in 0..draw.attribute_count() {
        locations |= 1 << draw.attribute(i).unwrap().location;
    }
    for location in 0..16 {
        let mask = (vs.attr_in()[location / 8] >> ((location % 8) * 4)) & 15;
        if mask != 0 && locations & (1 << location) == 0 {
            return Err("programmable shader vertex input missing");
        }
    }
    let color_mask = if draw.target_count() == 8 {
        u32::MAX
    } else {
        (1u32 << (draw.target_count() * 4)) - 1
    };
    if fs.fs_color_mask() & !color_mask != 0 {
        return Err("programmable fragment outputs exceed render targets");
    }
    for i in 0..128 {
        if fs.metadata().fs_inputs[i] != 0 && vs.attr_out()[i / 32] & (1 << (i % 32)) == 0 {
            return Err("programmable vertex/fragment interface mismatch");
        }
    }
    let mut code_offsets = [0usize; 2];
    for (i, shader) in [vs, fs].into_iter().enumerate() {
        let off = b.zero(shader.arena_len(), 4096)?;
        shader
            .materialize(&mut b.prepared.arena[off..off + shader.arena_len()])
            .map_err(|_| "programmable stage materialization failed")?;
        code_offsets[i] = off;
    }
    let (index_offset, last_vertex) = if state.index_format != 0 {
        let bytes = authority.buffer_bytes(state.index)?;
        let (_, last) = indexed_bounds(
            bytes,
            state.index_format,
            state.first,
            state.count,
            state.base_vertex,
        )?;
        let width = if state.index_format == 1 { 2usize } else { 4 };
        let start = state.first as usize * width;
        let size = state.count as usize * width;
        // Only the selected immutable indices are needed by DMA. Rebase the
        // private index view to zero without changing vertex ID or base bias.
        (
            Some((b.alloc(&bytes[start..start + size], 256)?, size)),
            last,
        )
    } else {
        (None, state.first + state.count - 1)
    };
    for i in 0..draw.stream_count() {
        let s = draw.stream(i).unwrap();
        let last = if s.divisor == 0 {
            last_vertex
        } else {
            state.first_instance + state.instances - 1
        };
        let required = (last as u64)
            .checked_add(1)
            .and_then(|n| n.checked_mul(s.stride as u64))
            .ok_or("programmable vertex range overflow")?;
        if required > s.range.size {
            return Err("programmable vertex fetch exceeds authorized stream");
        }
        authority.buffer_address(s.range)?;
    }
    let mut targets = Vec::new();
    targets
        .try_reserve_exact(draw.target_count())
        .map_err(|_| "programmable target allocation failed")?;
    for i in 0..draw.target_count() {
        let t = draw.target(i).unwrap();
        let image = authority.image(t.token, t.level, 1, t.layer, 1, ImageAccess::ColorTarget)?;
        if image.depth {
            return Err("programmable color target is depth");
        }
        if let Some(first) = targets.first() {
            let first: &ImageView = first;
            if (first.width, first.height) != (image.width, image.height) {
                return Err("programmable render target dimensions differ");
            }
        }
        if targets
            .iter()
            .chain(sampled.iter())
            .any(|other| image_overlaps(image, *other))
        {
            return Err("programmable render target aliases another view");
        }
        if state.scissor[0] + state.scissor[2] > image.width
            || state.scissor[1] + state.scissor[3] > image.height
            || image.width > 65535
            || image.height > 65535
        {
            return Err("programmable scissor outside framebuffer");
        }
        targets.push(image);
    }
    let depth = if state.depth_token != 0 {
        let image = authority.image(
            state.depth_token,
            state.depth_level,
            1,
            state.depth_layer,
            1,
            ImageAccess::Depth {
                write: state.depth & 2 != 0,
            },
        )?;
        if !image.depth
            || !image.tiled
            || targets
                .first()
                .is_some_and(|first| (image.width, image.height) != (first.width, first.height))
        {
            return Err("programmable depth target layout mismatch");
        }
        if state.depth & 2 != 0 && sampled.iter().any(|other| image_overlaps(image, *other)) {
            return Err("programmable depth target aliases sampled view");
        }
        Some(image)
    } else {
        None
    };
    // Every validation above precedes DMA. The following is only trusted method lowering.
    b.one(SERIALIZE, 0)?;
    b.private(CODE_ADDRESS_HIGH, 0, &[])?;
    b.one(INVALIDATE_SHADER_CACHES, INVALIDATE_SHADER_CACHE_READS)?;
    for stage in 0..6 {
        b.one(SP_SELECT + stage * 0x40, stage << 4)?;
    }
    for (i, stage) in [1u32, 5].into_iter().enumerate() {
        b.method(
            SP_SELECT + stage * 0x40,
            &[
                stage << 4 | 1,
                (code_offsets[i] + crate::program::ENTRY_OFFSET) as u32,
            ],
        )?;
        b.one(
            SP_GPR_ALLOC + stage * 0x40,
            stages[i].as_ref().unwrap().gprs(),
        )?;
    }
    b.method(0x0360, &[0x20164010, 0x20])?;
    for (stage_index, hardware_stage) in [0u32, 4].into_iter().enumerate() {
        for slot in 0..GRAPHICS_CB_COUNT {
            if cb_sizes[stage_index][slot] != 0 {
                b.cb(
                    hardware_stage,
                    slot as u32,
                    cb_offsets[stage_index][slot],
                    cb_sizes[stage_index][slot].next_multiple_of(256),
                )?;
            } else {
                b.one(CB_BIND + hardware_stage * 0x20, (slot as u32) << 4)?;
            }
        }
    }
    b.private(
        TIC_ADDRESS_HIGH,
        TIC_OFFSET,
        &[(draw.image_count().max(1) - 1) as u32],
    )?;
    b.private(
        TSC_ADDRESS_HIGH,
        TSC_OFFSET,
        &[(draw.image_count().max(1) - 1) as u32],
    )?;
    b.one(TIC_FLUSH, 0)?;
    b.one(TSC_FLUSH, 0)?;
    b.one(TEX_CACHE_CTL, 0)?;
    b.one(RT_CONTROL, (0o76543210 << 4) | draw.target_count() as u32)?;
    b.one(0x12e4, 1)?;
    b.one(0x0f90, 0)?;
    b.method(0x131c, &state.blend_constant.map(f32::to_bits))?;
    let factors = [
        0x4000, 0x4001, 0x4300, 0x4301, 0x4306, 0x4307, 0x4302, 0x4303, 0x4304, 0x4305, 0xc001,
        0xc002, 0xc003, 0xc004, 0x4308,
    ];
    let equations = [0x8006, 0x800a, 0x800b, 0x8007, 0x8008];
    for i in 0..8 {
        let t = draw.target(i);
        b.one(BLEND_ENABLE + i as u32 * 4, t.map_or(0, |t| t.blend_enable))?;
        let mask = t.map_or(0, |t| t.write_mask);
        b.one(
            COLOR_MASK + i as u32 * 4,
            (mask & 1) | ((mask >> 1) & 1) << 4 | ((mask >> 2) & 1) << 8 | ((mask >> 3) & 1) << 12,
        )?;
        if let Some(t) = t {
            let image = targets[i];
            b.method(
                RT_ADDRESS_HIGH + i as u32 * 0x40,
                &[
                    (image.address >> 32) as u32,
                    image.address as u32,
                    if image.tiled {
                        image.width
                    } else {
                        image.pitch
                    },
                    image.height,
                    0xcf,
                    if image.tiled {
                        (image.tile_y as u32) << 4
                    } else {
                        RT_TILE_MODE_LINEAR
                    },
                    1,
                    0,
                    0,
                ],
            )?;
            b.method(
                0x1e04 + i as u32 * 0x20,
                &[
                    equations[t.color_op as usize],
                    factors[t.src_color as usize],
                    factors[t.dst_color as usize],
                    equations[t.alpha_op as usize],
                    factors[t.src_alpha as usize],
                    factors[t.dst_alpha as usize],
                ],
            )?;
        }
    }
    let image = targets
        .first()
        .copied()
        .or(depth)
        .ok_or("programmable framebuffer attachment missing")?;
    if image.width > 65535
        || image.height > 65535
        || state.scissor[0] + state.scissor[2] > image.width
        || state.scissor[1] + state.scissor[3] > image.height
    {
        return Err("programmable scissor outside framebuffer");
    }
    let x_end = state.viewport[0] + state.viewport[2];
    let y_end = state.viewport[1] + state.viewport[3];
    if x_end > image.width as f32
        || state.viewport[1] > image.height as f32
        || !(0. ..=image.height as f32).contains(&y_end)
    {
        return Err("programmable viewport outside framebuffer");
    }
    b.method(
        SCREEN_SCISSOR_HORIZ,
        &[image.width << 16, image.height << 16],
    )?;
    if let Some(d) = depth {
        b.method(
            ZETA_ADDRESS_HIGH,
            &[
                (d.address >> 32) as u32,
                d.address as u32,
                0x0a,
                (d.tile_y as u32) << 4,
                d.array_pitch / 4,
            ],
        )?;
        b.method(ZETA_HORIZ, &[d.width, d.height, 0x10001])?;
        b.one(ZETA_BASE_LAYER, 0)?;
        b.one(ZETA_ENABLE, 1)?;
    } else {
        b.one(ZETA_ENABLE, 0)?;
    }
    b.one(DEPTH_TEST_ENABLE, state.depth & 1)?;
    b.one(DEPTH_WRITE_ENABLE, (state.depth >> 1) & 1)?;
    b.one(DEPTH_TEST_FUNC, 0x200 + (state.depth >> 2))?;
    let v = state.viewport;
    let (sx, sy, sz) = (v[2] * 0.5, v[3] * 0.5, (v[5] - v[4]) * 0.5);
    b.method(
        VIEWPORT_TRANSLATE_X,
        &[
            (v[0] + sx).to_bits(),
            (v[1] + sy).to_bits(),
            (v[4] + sz).to_bits(),
        ],
    )?;
    b.method(
        VIEWPORT_SCALE_X,
        &[sx.to_bits(), (-sy).to_bits(), sz.to_bits()],
    )?;
    b.method(VIEWPORT_HORIZ, &[image.width << 16, image.height << 16])?;
    b.one(VIEWPORT_SWIZZLE, 0x6420)?;
    b.method(DEPTH_RANGE_NEAR, &[v[4].to_bits(), v[5].to_bits()])?;
    let s = state.scissor;
    b.method(
        SCISSOR_ENABLE,
        &[1, (s[0] + s[2]) << 16 | s[0], (s[1] + s[3]) << 16 | s[1]],
    )?;
    b.one(CULL_FACE_ENABLE, u32::from(state.raster >> 1 != 0))?;
    b.one(
        CULL_FACE,
        if state.raster >> 1 == 1 { 0x404 } else { 0x405 },
    )?;
    b.one(
        FRONT_FACE,
        if state.raster & 1 != 0 { 0x900 } else { 0x901 },
    )?;
    for i in 0..16 {
        if let Some(stream) = draw.stream(i) {
            let address = authority.buffer_address(stream.range)?;
            b.method(
                VERTEX_ARRAY_FETCH + i as u32 * 0x10,
                &[
                    VERTEX_ARRAY_FETCH_ENABLE | stream.stride,
                    (address >> 32) as u32,
                    address as u32,
                    stream.divisor,
                ],
            )?;
            b.address(
                VERTEX_ARRAY_LIMIT_HIGH + i as u32 * 8,
                address + stream.range.size - 1,
            )?;
            b.one(VERTEX_ARRAY_PER_INSTANCE + i as u32 * 4, stream.divisor)?;
        } else {
            b.one(VERTEX_ARRAY_FETCH + i as u32 * 0x10, 0)?;
            b.one(VERTEX_ARRAY_PER_INSTANCE + i as u32 * 4, 0)?;
        }
        let mut format = VERTEX_ATTRIB_FORMAT_TYPE_FLOAT
            | VERTEX_ATTRIB_FORMAT_SIZE_32_32_32_32
            | VERTEX_ATTRIB_FORMAT_CONST;
        for j in 0..draw.attribute_count() {
            let a = draw.attribute(j).unwrap();
            if a.location == i as u32 {
                format = attribute_format(a)?;
            }
        }
        b.one(VERTEX_ATTRIB_FORMAT + i as u32 * 4, format)?;
    }
    b.one(VB_ELEMENT_BASE, state.base_vertex as u32)?;
    b.one(0x1118, state.base_vertex as u32)?; // VERTEX_ID_BASE: nvc0_draw_elements
    b.one(0x1438, state.first_instance)?; // VB_INSTANCE_BASE
    b.one(0x1644, 0)?; // PRIM_RESTART_ENABLE
    if let Some((offset, size)) = index_offset {
        let first = b.method(INDEX_ARRAY_START_HIGH, &[0, 0, 0, 0, state.index_format])?;
        b.prepared.patches.push(Patch {
            high: first,
            offset,
        });
        b.prepared.patches.push(Patch {
            high: first + 2,
            offset: offset + size - 1,
        });
    }
    let primitive = [4, 5, 6][state.topology as usize];
    for instance in 0..state.instances {
        b.one(
            VERTEX_BEGIN_GL,
            primitive | if instance == 0 { 0 } else { 0x04000000 },
        )?;
        b.method(
            if state.index_format == 0 {
                VERTEX_BUFFER_FIRST
            } else {
                INDEX_BATCH_FIRST
            },
            &[
                if state.index_format == 0 {
                    state.first
                } else {
                    0
                },
                state.count,
            ],
        )?;
        b.one(VERTEX_END_GL, 0)?;
    }
    b.zero(0, 4096)?;
    Ok(b.prepared)
}

/// Reproducible WGSL -> TGSI -> pinned NAK packages also pass the ordinary
/// package verifier and authority preparer; startup has no shader bypass.
pub const PROOF_VS: &[u8] =
    include_bytes!("../../../../shared/maxwell-program-wire/tests/fixtures/boot-vs.mxp");
pub const PROOF_FS: &[u8] =
    include_bytes!("../../../../shared/maxwell-program-wire/tests/fixtures/boot-fs.mxp");
pub struct ProofAuthority {
    pub vertex_address: u64,
    pub target_address: u64,
}
impl Authority for ProofAuthority {
    fn buffer_bytes(&self, range: Range) -> Result<&[u8], &'static str> {
        let bytes = match range.token {
            1 => PROOF_VS,
            2 => PROOF_FS,
            _ => return Err("proof buffer unavailable"),
        };
        if range.offset != 0 || range.size != bytes.len() as u64 {
            return Err("proof buffer range invalid");
        }
        Ok(bytes)
    }
    fn buffer_address(&self, range: Range) -> Result<u64, &'static str> {
        if range
            != (Range {
                token: 3,
                offset: 0,
                size: 24,
            })
        {
            return Err("proof vertex range invalid");
        }
        Ok(self.vertex_address)
    }
    fn image(
        &self,
        token: u64,
        level: u32,
        levels: u32,
        layer: u32,
        layers: u32,
        access: ImageAccess,
    ) -> Result<ImageView, &'static str> {
        if token != 4
            || level != 0
            || levels != 1
            || layer != 0
            || layers != 1
            || !matches!(access, ImageAccess::ColorTarget)
        {
            return Err("proof target view invalid");
        }
        Ok(ImageView {
            address: self.target_address,
            range_address: self.target_address,
            span_size: 4096,
            width: 16,
            height: 16,
            pitch: 256,
            tiled: false,
            tile_y: 0,
            array_pitch: 4096,
            mip_levels: 1,
            layers: 1,
            cube: false,
            depth: false,
        })
    }
}
pub fn proof_metadata() -> Result<Vec<u8>, &'static str> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(424)
        .map_err(|_| "proof metadata allocation failed")?;
    bytes.resize(424, 0);
    let mut put =
        |offset: usize, value: u32| bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    put(0, draw::MAGIC);
    put(4, draw::VERSION);
    put(8, 424);
    put(20, 3);
    put(24, 1);
    put(72, 2);
    put(76, 1);
    put(80, 1);
    put(92, 1);
    for (i, offset) in [256, 320, 360, 376, 376, 376].into_iter().enumerate() {
        put(96 + i * 4, offset);
    }
    put(120, 424);
    put(136, 16f32.to_bits());
    put(140, 16f32.to_bits());
    put(148, 1f32.to_bits());
    put(160, 16);
    put(164, 16);
    put(264, 1);
    put(280, PROOF_VS.len() as u32);
    put(288, 4);
    put(296, 2);
    put(312, PROOF_FS.len() as u32);
    put(320, 3);
    put(336, 24);
    put(344, 8);
    put(372, 1); // float32x2 attribute
    put(376, 4);
    put(396, 15);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn method_data(words: &[u32], wanted: u32) -> &[u32] {
        let mut offset = 0;
        while offset < words.len() {
            let header = words[offset];
            let method = (header & 0x1fff) * 4;
            let count = ((header >> 16) & 0x1fff) as usize;
            if method == wanted {
                return &words[offset + 1..offset + 1 + count];
            }
            offset += count + 1;
        }
        panic!("method {wanted:#x} missing");
    }
    #[test]
    fn immutable_index_snapshot_checks_signed_base_and_views() {
        let bytes = [1u16, 2, 3].map(u16::to_le_bytes).concat();
        assert_eq!(indexed_bounds(&bytes, 1, 0, 3, -1), Ok((0, 2)));
        assert!(indexed_bounds(&bytes, 1, 0, 3, -2).is_err());
        assert!(indexed_bounds(&bytes, 1, 1, 3, 0).is_err());
        let bytes = [u32::MAX].map(u32::to_le_bytes).concat();
        assert!(indexed_bounds(&bytes, 2, 0, 1, 1).is_err());
    }
    #[test]
    fn actual_wgsl_packages_prepare_private_sph_and_methods() {
        let authority = ProofAuthority {
            vertex_address: 0x100000,
            target_address: 0x200000,
        };
        let metadata = proof_metadata().unwrap();
        let prepared = prepare(&metadata, &authority).unwrap();
        assert!(prepared.arena.len() < 64 * 1024);
        let published = prepared.publish(0x300000).unwrap();
        assert!(!published.words.is_empty());
        // Decode the actual startup push: the old 32-slot reset emitted
        // CB_BIND=0x120 and faulted on GM20B before any rendering occurred.
        let mut offset = 0;
        let mut cb_bindings = [0u32; 2];
        while offset < published.words.len() {
            let header = published.words[offset];
            let method = (header & 0x1fff) * 4;
            let count = ((header >> 16) & 0x1fff) as usize;
            if let Some(stage) = [CB_BIND, CB_BIND + 4 * 0x20]
                .iter()
                .position(|&binding| method == binding)
            {
                assert_eq!(count, 1);
                let value = published.words[offset + 1];
                assert!(value >> 4 < 16, "unsupported CB binding {value:#x}");
                cb_bindings[stage] |= 1 << (value >> 4);
            }
            offset += count + 1;
        }
        assert_eq!(cb_bindings, [0xffff; 2]);
        for patch in &prepared.patches {
            let address = u64::from(published.words[patch.high]) << 32
                | u64::from(published.words[patch.high + 1]);
            assert_eq!(address, 0x300000 + patch.offset as u64);
            assert!((0x300000..0x300000 + prepared.arena.len() as u64).contains(&address));
        }
        let mut malformed = metadata.clone();
        malformed[336..340].copy_from_slice(&16u32.to_le_bytes());
        assert!(prepare(&malformed, &authority).is_err());
        let mut malformed = metadata;
        malformed[264..268].copy_from_slice(&9u32.to_le_bytes());
        assert!(prepare(&malformed, &authority).is_err());
    }

    #[test]
    fn sixteen_units_per_stage_have_distinct_private_handles_and_descriptors() {
        struct Textures(ProofAuthority);
        impl Authority for Textures {
            fn buffer_bytes(&self, r: Range) -> Result<&[u8], &'static str> {
                self.0.buffer_bytes(r)
            }
            fn buffer_address(&self, r: Range) -> Result<u64, &'static str> {
                self.0.buffer_address(r)
            }
            fn image(
                &self,
                t: u64,
                l: u32,
                n: u32,
                a: u32,
                k: u32,
                access: ImageAccess,
            ) -> Result<ImageView, &'static str> {
                if t == 10
                    && l == 0
                    && n == 1
                    && a == 0
                    && k == 1
                    && matches!(access, ImageAccess::Sample)
                {
                    Ok(ImageView {
                        address: 0x400000,
                        range_address: 0x400000,
                        span_size: 4096,
                        width: 16,
                        height: 16,
                        pitch: 256,
                        tiled: false,
                        tile_y: 0,
                        array_pitch: 4096,
                        mip_levels: 1,
                        layers: 1,
                        cube: false,
                        depth: false,
                    })
                } else {
                    self.0.image(t, l, n, a, k, access)
                }
            }
        }
        let authority = Textures(ProofAuthority {
            vertex_address: 0x100000,
            target_address: 0x200000,
        });
        let original = proof_metadata().unwrap();
        let mut bytes = original[..376].to_vec();
        bytes.resize(376 + 32 * draw::IMAGE_SIZE, 0);
        bytes.extend_from_slice(&original[376..]);
        let length = bytes.len() as u32;
        let mut put = |offset: usize, value: u32| {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes())
        };
        put(8, length);
        put(88, 32);
        put(116, 376 + 32 * draw::IMAGE_SIZE as u32);
        put(120, length);
        for i in 0..32 {
            let offset = 376 + i * draw::IMAGE_SIZE;
            put(offset, 10);
            put(offset + 12, 1);
            put(offset + 20, 1);
            put(offset + 56, if i < 16 { 0 } else { 4 });
            put(offset + 60, (i % 16) as u32);
        }
        let prepared = prepare(&bytes, &authority).unwrap();
        let read =
            |offset| u32::from_le_bytes(prepared.arena[offset..offset + 4].try_into().unwrap());
        assert_eq!(read(4096 + 0x20 + 15 * 4), 15 | 15 << 20);
        assert_eq!(read(4352 + 0x20 + 15 * 4), 31 | 31 << 20);
        assert_ne!(&prepared.arena[..1024], &prepared.arena[1024..2048]);
        let published = prepared.publish(0x300000).unwrap();
        let mut word = 0;
        let mut tables = 0;
        while word < published.words.len() {
            let header = published.words[word];
            let method = (header & 0x1fff) * 4;
            let count = ((header >> 16) & 0x1fff) as usize;
            if matches!(method, TIC_ADDRESS_HIGH | TSC_ADDRESS_HIGH) {
                assert_eq!(published.words[word + 3], 31);
                tables += 1;
            }
            word += count + 1;
        }
        assert_eq!(tables, 2);
    }

    #[test]
    fn viewport_sign_reversed_depth_and_source_alpha_match_sgfx_wire_semantics() {
        let authority = ProofAuthority {
            vertex_address: 0x100000,
            target_address: 0x200000,
        };
        let mut bytes = proof_metadata().unwrap();
        let put = |bytes: &mut [u8], offset: usize, value: u32| {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes())
        };
        put(&mut bytes, 132, 16f32.to_bits());
        put(&mut bytes, 140, (-16f32).to_bits());
        put(&mut bytes, 144, 1f32.to_bits());
        put(&mut bytes, 148, 0);
        put(&mut bytes, 392, 1);
        put(&mut bytes, 400, 6);
        put(&mut bytes, 404, 7);
        put(&mut bytes, 412, 1);
        put(&mut bytes, 416, 7);
        let published = prepare(&bytes, &authority)
            .unwrap()
            .publish(0x300000)
            .unwrap();
        assert_eq!(
            method_data(&published.words, VIEWPORT_SCALE_X),
            &[8f32.to_bits(), 8f32.to_bits(), (-0.5f32).to_bits()]
        );
        assert_eq!(
            method_data(&published.words, DEPTH_RANGE_NEAR),
            &[1f32.to_bits(), 0]
        );
        assert_eq!(
            method_data(&published.words, 0x1e04),
            &[0x8006, 0x4302, 0x4303, 0x8006, 0x4001, 0x4303]
        );
        put(&mut bytes, 132, 0);
        put(&mut bytes, 140, 0);
        assert!(prepare(&bytes, &authority).is_ok());
        put(&mut bytes, 132, 17f32.to_bits());
        assert!(prepare(&bytes, &authority).is_err());
    }

    #[test]
    fn typed_dimensions_and_lod0_base_are_preserved_for_one_layer_views() {
        let image = ImageView {
            address: 0x100000,
            range_address: 0x101000,
            span_size: 4096,
            width: 64,
            height: 1,
            pitch: 256,
            tiled: true,
            tile_y: 0,
            array_pitch: 8192,
            mip_levels: 3,
            layers: 1,
            cube: false,
            depth: false,
        };
        let base = draw::Image {
            token: 10,
            base_level: 1,
            level_count: 1,
            base_layer: 0,
            layer_count: 1,
            ..Default::default()
        };
        let d1 = tic(
            image,
            draw::Image {
                sampler: 1 << draw::SAMPLER_DIMENSION_SHIFT,
                ..base
            },
        )
        .unwrap();
        let d1_array = tic(
            image,
            draw::Image {
                sampler: 2 << draw::SAMPLER_DIMENSION_SHIFT,
                ..base
            },
        )
        .unwrap();
        let d2_array = tic(
            image,
            draw::Image {
                sampler: 3 << draw::SAMPLER_DIMENSION_SHIFT,
                ..base
            },
        )
        .unwrap();
        assert_eq!(d1[4] & 0x07800000, 0x00800000);
        assert_eq!(d1_array[4] & 0x07800000, 0x02800000);
        assert_eq!(d2_array[4] & 0x07800000, 0x02800000);
        assert_eq!(d2_array[1], 0x100000);
        assert_eq!(d2_array[4] & 0xffff, 63);
        assert_eq!(d2_array[7], 0x11);
        for dimension in [1, 2] {
            assert!(
                tic(
                    ImageView { height: 2, ..image },
                    draw::Image {
                        sampler: dimension << draw::SAMPLER_DIMENSION_SHIFT,
                        ..base
                    }
                )
                .is_err()
            );
        }
        let selected = ImageView {
            address: 0x101000,
            range_address: 0x101000,
            ..image
        };
        assert!(image_overlaps(image, selected));
    }
}

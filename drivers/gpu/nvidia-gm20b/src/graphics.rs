// SPDX-License-Identifier: GPL-2.0-only
//! Trusted Nouveau method templates for the SGFX fixed-program subset.
//! Mesa nvc0_screen, nvc0_shader_state, nvc0_surface, nvc0_tex and nvc0_vbo
//! at the shader pack's pinned revision are the primary reference. See NOTICE.

use crate::{
    fifo::Fifo,
    gmmu::{clean, pages, pages_aligned, store},
    gr::Gr,
    method::*,
};
use alloc::vec::Vec;
use maxwell_shader_pack::{PACK_SIZE, PipelineVariant, copy_pack};
use scarlet::{arch, device::gpu::GpuBackendSubmitError, mem::page::ContiguousPages, time};

pub const PROGRAM_VA: usize = 0x270000;
const AUX_VA: usize = 0x280000;
const DESCRIPTOR_VA: usize = 0x290000;
pub const CONTEXT_VA: usize = 0x2a0000;
const PATCH_VA: usize = 0x3a0000;
const VERTEX_VA: usize = 0x3b0000;
const IMAGE_VA: usize = 0x3c0000;
const TEXTURE_VA: usize = 0x3d0000;
const PUSH_VA: usize = 0x400000;
const TILE_VA: usize = 0x500000;
const TILE_WIDTH: usize = 16;
const TILE_HEIGHT: usize = 128;
const TILE_PITCH: usize = TILE_WIDTH * 4;
const TILE_SIZE: usize = TILE_PITCH * TILE_HEIGHT;
const FENCE_VA: u32 = 0x6000;
const PUSH_SIZE: usize = 0x100000;
const BGRA8: u32 = 0xcf; // G80_SURFACE_FORMAT_BGRA8_UNORM

pub struct Graphics {
    gpu_base: usize,
    programs: ContiguousPages,
    aux: ContiguousPages,
    descriptors: ContiguousPages,
    context: ContiguousPages,
    patches: ContiguousPages,
    push: ContiguousPages,
    vertex: ContiguousPages,
    image: ContiguousPages,
    texture: ContiguousPages,
    tile: ContiguousPages,
    sequence: u32,
}
impl Graphics {
    pub fn allocate(gpu_base: usize) -> Result<Self, &'static str> {
        Ok(Self {
            gpu_base,
            programs: pages(PACK_SIZE / 4096)?,
            aux: pages(1)?,
            descriptors: pages(1)?,
            context: pages(256)?,
            patches: pages(1)?,
            push: pages(PUSH_SIZE / 4096)?,
            vertex: pages(1)?,
            image: pages(1)?,
            texture: pages(1)?,
            tile: pages_aligned(TILE_SIZE / 4096, 8192)?,
            sequence: 0x53474700,
        })
    }
    pub fn mappings(&self) -> [(usize, &ContiguousPages); 9] {
        [
            (PROGRAM_VA, &self.programs),
            (AUX_VA, &self.aux),
            (DESCRIPTOR_VA, &self.descriptors),
            (CONTEXT_VA, &self.context),
            (PATCH_VA, &self.patches),
            (PUSH_VA, &self.push),
            (VERTEX_VA, &self.vertex),
            (IMAGE_VA, &self.image),
            (TEXTURE_VA, &self.texture),
        ]
    }
    pub fn tile_mapping(&self) -> (usize, &ContiguousPages) {
        (TILE_VA, &self.tile)
    }
    pub fn prepare(&self, gr: &Gr, size: u32) -> Result<(), &'static str> {
        gr.copy_golden(&self.context, size as usize)?;
        // Nouveau gf100_gr_chan_bind's firmware header and mmio patch table.
        // This initial engine serializes all contexts and uses one immutable
        // set of global buffers. Addresses never originate in userspace.
        let patch = [
            (0x418810, 0x80000000 | 0x200000 >> 12),
            (0x419848, 0x10000000 | 0x200000 >> 12),
            (0x419c2c, 0x10000000 | 0x200000 >> 12),
            (0x40800c, 0x250000 >> 8),
            (0x408010, 0x80000000),
            (0x419004, 0x250000 >> 8),
            (0x419008, 0),
            (0x4064cc, 0x80000000),
            (0x418e30, 0x80000000),
            (0x408004, 0x260000 >> 8),
            (0x408008, 0x80000018),
            (0x418e24, 0x260000 >> 8),
            (0x418e28, 0x80000018),
            (0x4064c8, 0x00c001c0),
        ];
        for (i, (reg, value)) in patch.into_iter().enumerate() {
            store(&self.patches, i * 2, reg);
            store(&self.patches, i * 2 + 1, value);
        }
        for (offset, value) in [
            (0xf4, 0),
            (0xf8, 0),
            (0x10, patch.len() as u32),
            (0x14, PATCH_VA as u32),
            (0x18, 0),
            (0x1c, 1),
            (0x20, 0),
            (0x28, 0),
            (0x2c, 0),
        ] {
            store(&self.context, offset / 4, value);
        }
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(self.programs.as_vaddr() as *mut u8, PACK_SIZE)
        };
        copy_pack(bytes)?;
        for (_, memory) in self.mappings() {
            clean(memory);
        }
        // Page allocation zeroes through the cached CPU alias. Retire those
        // dirty zero lines before the GPU first writes this tiled target.
        clean(&self.tile);
        Ok(())
    }
    pub fn execute(
        &mut self,
        fifo: &Fifo,
        gr: &Gr,
        operations: &[[u32; 64]],
    ) -> Result<(), GpuBackendSubmitError> {
        let started = time::current_time_ns();
        let mut push = Push::new();
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(GpuBackendSubmitError::DeviceLost(
                "graphics sequence exhausted",
            ))?;
        // All allocation and template lowering is completed before DMA. A bad
        // payload or exhausted staging budget must not poison a healthy queue.
        (|| -> Result<(), &'static str> {
            push.initialize()?;
            for op in operations {
                push.operation(op)?;
            }
            // Mesa QUERY_GET FENCE|SHORT|UNIT_ALL proves PGRAPH completion.
            push.method(
                0,
                QUERY_ADDRESS_HIGH,
                &[
                    0,
                    FENCE_VA,
                    sequence,
                    QUERY_GET_FENCE | QUERY_GET_SHORT | 0xf000,
                ],
            )?;
            push.method(0, 0x50, &[sequence])?;
            Ok(())
        })()
        .map_err(GpuBackendSubmitError::Rejected)?;
        self.sequence = sequence;
        let encoded = time::current_time_ns();
        unsafe {
            core::ptr::copy_nonoverlapping(
                push.words.as_ptr(),
                self.push.as_vaddr() as *mut u32,
                push.words.len(),
            );
        }
        arch::clean_dcache_to_poc_range(self.push.as_vaddr(), push.words.len() * 4);
        let published = time::current_time_ns();
        let result = fifo.graphics(PUSH_VA, push.words.len() as u32, CONTEXT_VA, self.sequence);
        let retired = time::current_time_ns();
        let count = self.sequence - 0x53474700;
        if count <= 4 {
            scarlet::println!(
                "gm20b: graphics={} words={} encode_us={} publish_us={} fifo_us={}",
                count,
                push.words.len(),
                encoded.saturating_sub(started) / 1000,
                published.saturating_sub(encoded) / 1000,
                retired.saturating_sub(published) / 1000
            );
        }
        if result.is_err() {
            gr.diagnose();
        }
        result.map_err(GpuBackendSubmitError::DeviceLost)?;
        gr.idle().map_err(GpuBackendSubmitError::DeviceLost)?;
        gr.check_execution()
            .map_err(GpuBackendSubmitError::DeviceLost)?;
        if operations.iter().any(|operation| operation[52] == 0x40) {
            crate::gmmu::flush_ltc_at(self.gpu_base).map_err(GpuBackendSubmitError::DeviceLost)?;
        }
        Ok(())
    }
    /// Exercise every canonical shader pair, texture descriptor, indexed draw,
    /// source-over blending, scissor and 902D copy before exposing execution.
    pub fn verify(&mut self, fifo: &Fifo, gr: &Gr) -> Result<u32, &'static str> {
        for i in 0..4096 / 4 {
            store(&self.texture, i, 0xff0000ff);
        }
        clean(&self.texture);
        let mut checksum = 0x811c9dc5u32;
        for variant in PipelineVariant::ALL {
            let (vs, fs) = variant.shaders();
            let textured = matches!(
                fs,
                maxwell_shader_pack::ShaderVariant::FsTextureRgba
                    | maxwell_shader_pack::ShaderVariant::FsTextureAlphaMask
                    | maxwell_shader_pack::ShaderVariant::FsTextureVertexColorRgba
                    | maxwell_shader_pack::ShaderVariant::FsTextureRgbIgnoreAlpha
            );
            let mask = matches!(fs, maxwell_shader_pack::ShaderVariant::FsTextureAlphaMask);
            let colored = matches!(
                vs,
                maxwell_shader_pack::ShaderVariant::VsStride40Pos4Color4
                    | maxwell_shader_pack::ShaderVariant::VsStride40Pos4Color4Uv2
                    | maxwell_shader_pack::ShaderVariant::VsStride28Pos4Color3
            );
            let stride = variant.stride() as usize;
            let positions = [[-0.75f32, -0.75], [0.75, -0.75], [0., 0.75]];
            for (i, pos) in positions.into_iter().enumerate() {
                let mut v = [0f32; 10];
                v[..4].copy_from_slice(&[pos[0], pos[1], 0., 1.]);
                if stride == 16 {
                    v[2] = 0.5;
                    v[3] = 0.5;
                }
                if colored {
                    v[4..8].fill(1.);
                }
                if stride == 24 {
                    v[4] = 0.5;
                    v[5] = 0.5;
                }
                if stride == 40 {
                    v[8] = 0.5;
                    v[9] = 0.5;
                }
                for (j, value) in v[..stride / 4].iter().enumerate() {
                    store(&self.vertex, (i * stride) / 4 + j, value.to_bits());
                }
            }
            clean(&self.vertex);
            let mut clear = [0; 64];
            clear[0] = 1;
            clear[2] = IMAGE_VA as u32;
            clear[10..13].copy_from_slice(&[16, 16, 256]);
            clear[13..17].copy_from_slice(&[0, 0, 16, 16]);
            clear[32..36].copy_from_slice(&[1f32.to_bits(), 0, 0, 1f32.to_bits()]);
            let mut draw = [0; 64];
            draw[0] = 2;
            draw[2] = IMAGE_VA as u32;
            draw[4] = VERTEX_VA as u32;
            draw[10..13].copy_from_slice(&[16, 16, 256]);
            draw[13..17].copy_from_slice(&[0, 0, 16, 16]);
            draw[17..21].copy_from_slice(&[0, 0, 16, 16]);
            draw[21] = variant as u32;
            draw[23] = stride as u32;
            draw[25] = 3;
            draw[28] = (stride * 3) as u32;
            for i in 0..4 {
                draw[32 + i * 5] = 1f32.to_bits();
            }
            let color = if textured {
                [1f32, 1., 1., 1.]
            } else {
                [0., 1., 0., 1.]
            };
            for (dst, v) in draw[48..52].iter_mut().zip(color) {
                *dst = v.to_bits();
            }
            if textured {
                draw[6] = TEXTURE_VA as u32;
                draw[29..32].copy_from_slice(&[16, 16, 256]);
                draw[22] = u32::from(mask) << 5;
            }
            self.execute(fifo, gr, &[clear, draw])
                .map_err(proof_error)?;
            arch::invalidate_dcache_to_poc_range(self.image.as_vaddr(), 4096);
            let pixel = |x: usize, y: usize| unsafe {
                core::ptr::read_volatile((self.image.as_vaddr() + y * 256 + x * 4) as *const u32)
            };
            let center = pixel(8, 8);
            let corner = pixel(0, 0);
            let expected = if mask {
                0xffffffff
            } else if textured {
                0xff0000ff
            } else {
                0xff00ff00
            };
            scarlet::println!(
                "gm20b: SGFX shader {:?} pixels center={:#010x} corner={:#010x}",
                variant,
                center,
                corner
            );
            if center != expected || corner != 0xffff0000 {
                return Err("SGFX shader draw/readback mismatch");
            }
            checksum = (checksum ^ center).wrapping_mul(0x01000193);
        }
        // Real indexed draws use a skipped prefix and a nonzero base vertex.
        // Vertex zero duplicates vertex one, so ignoring the base produces a
        // degenerate triangle rather than accidentally passing the pixel proof.
        for (i, p) in [[-0.75f32, -0.75], [-0.75, -0.75], [0.75, -0.75], [0., 0.75]]
            .into_iter()
            .enumerate()
        {
            store(&self.vertex, i * 4, p[0].to_bits());
            store(&self.vertex, i * 4 + 1, p[1].to_bits());
        }
        let indices16 = [u16::MAX, u16::MAX, 0, 1, 2];
        unsafe {
            core::ptr::copy_nonoverlapping(
                indices16.as_ptr(),
                (self.vertex.as_vaddr() + 0x800) as *mut u16,
                indices16.len(),
            );
        }
        for (i, value) in [u32::MAX, u32::MAX, 2, 0, 1].into_iter().enumerate() {
            store(&self.vertex, 0x900 / 4 + i, value);
        }
        clean(&self.vertex);
        let clear = probe_clear();
        let mut draw = probe_draw(PipelineVariant::Stride16Solid);
        draw[8] = (VERTEX_VA + 0x800) as u32;
        draw[22] = 1; // straight source-over
        draw[19] = 8; // clip the right half
        draw[24] = 2;
        draw[26] = 1; // u16
        draw[27] = 1;
        draw[28] = 64;
        draw[51] = 0.5f32.to_bits();
        self.execute(fifo, gr, &[clear, draw])
            .map_err(proof_error)?;
        arch::invalidate_dcache_to_poc_range(self.image.as_vaddr(), 4096);
        let blended = self.pixel(6, 8);
        let clipped = self.pixel(9, 8);
        scarlet::println!(
            "gm20b: SGFX indexed-u16 blend/scissor pixels={:#010x}/{:#010x}",
            blended,
            clipped
        );
        if blended & 0xff0000ff != 0xff000000
            || !matches!((blended >> 8) & 0xff, 127 | 128)
            || !matches!((blended >> 16) & 0xff, 127 | 128)
            || clipped != 0xffff0000
        {
            return Err("SGFX indexed/source-over/scissor readback mismatch");
        }
        checksum = (checksum ^ blended).wrapping_mul(0x01000193);
        draw[8] = (VERTEX_VA + 0x900) as u32;
        draw[22] = 0;
        draw[19] = 16;
        draw[26] = 2; // u32
        draw[51] = 1f32.to_bits();
        self.execute(fifo, gr, &[clear, draw])
            .map_err(proof_error)?;
        arch::invalidate_dcache_to_poc_range(self.image.as_vaddr(), 4096);
        if self.pixel(8, 8) != 0xff00ff00 || self.pixel(0, 0) != 0xffff0000 {
            return Err("SGFX indexed-u32 readback mismatch");
        }
        scarlet::println!("gm20b: SGFX indexed-u32 draw passed");

        // The textured proof also executes a linear sampler. Uniform blue
        // texels avoid making the first boot depend on interpolation rounding.
        for (i, p) in [[-0.75f32, -0.75], [0.75, -0.75], [0., 0.75]]
            .into_iter()
            .enumerate()
        {
            for (j, value) in [p[0], p[1], 0.5, 0.5].into_iter().enumerate() {
                store(&self.vertex, i * 4 + j, value.to_bits());
            }
        }
        clean(&self.vertex);
        let mut linear = probe_draw(PipelineVariant::Stride16TextureRgba);
        linear[6] = TEXTURE_VA as u32;
        linear[22] = 2;
        linear[29..32].copy_from_slice(&[16, 16, 256]);
        linear[48..52].fill(1f32.to_bits());
        self.execute(fifo, gr, &[clear, linear])
            .map_err(proof_error)?;
        arch::invalidate_dcache_to_poc_range(self.image.as_vaddr(), 4096);
        if self.pixel(8, 8) != 0xff0000ff {
            return Err("SGFX linear sampler readback mismatch");
        }
        scarlet::println!("gm20b: SGFX linear sampler draw passed");
        // The same 16-GOB, kind-0xfe storage selected for the display
        // swapchain must survive PGRAPH writes, 902D reads/writes and TIC
        // sampling before the backend advertises Ready.
        let mut tiled_clear = probe_clear();
        tiled_clear[2] = TILE_VA as u32;
        tiled_clear[10..13].copy_from_slice(&[
            TILE_WIDTH as u32,
            TILE_HEIGHT as u32,
            TILE_PITCH as u32,
        ]);
        tiled_clear[15] = TILE_WIDTH as u32;
        tiled_clear[16] = TILE_HEIGHT as u32;
        tiled_clear[52] = 0x40;
        self.execute(fifo, gr, &[tiled_clear])
            .map_err(proof_error)?;
        arch::invalidate_dcache_to_poc_range(self.tile.as_vaddr(), TILE_SIZE);
        scarlet::println!(
            "gm20b: tiled proof paddr={:#x} pixel00={:#010x} pixel88={:#010x}",
            self.tile.as_paddr(),
            self.tile_pixel(0, 0),
            self.tile_pixel(8, 8)
        );
        let render_matches =
            self.tile_pixel(0, 0) == 0xffff0000 && self.tile_pixel(8, 8) == 0xffff0000;
        let mut sampled_tile = linear;
        sampled_tile[6] = TILE_VA as u32;
        sampled_tile[29..32].copy_from_slice(&[
            TILE_WIDTH as u32,
            TILE_HEIGHT as u32,
            TILE_PITCH as u32,
        ]);
        sampled_tile[53] = 0x40;
        self.execute(fifo, gr, &[clear, sampled_tile])
            .map_err(proof_error)?;
        arch::invalidate_dcache_to_poc_range(self.image.as_vaddr(), 4096);
        let sampled = self.pixel(8, 8);
        scarlet::println!("gm20b: tiled sampler center={:#010x}", sampled);
        let mut tile_to_linear = [0; 64];
        tile_to_linear[0] = 3;
        tile_to_linear[2] = IMAGE_VA as u32;
        tile_to_linear[4] = TILE_VA as u32;
        tile_to_linear[10..13].copy_from_slice(&[16, 16, 256]);
        tile_to_linear[13..17].copy_from_slice(&[0, 0, 16, 16]);
        tile_to_linear[17..21].copy_from_slice(&[0, 0, 16, 16]);
        tile_to_linear[29..32].copy_from_slice(&[
            TILE_WIDTH as u32,
            TILE_HEIGHT as u32,
            TILE_PITCH as u32,
        ]);
        tile_to_linear[53] = 0x40;
        self.execute(fifo, gr, &[tile_to_linear])
            .map_err(proof_error)?;
        arch::invalidate_dcache_to_poc_range(self.image.as_vaddr(), 4096);
        let copied = (self.pixel(0, 0), self.pixel(8, 8));
        scarlet::println!(
            "gm20b: tiled-to-linear pixels={:#010x}/{:#010x}",
            copied.0,
            copied.1
        );
        let mut linear_to_tile = tile_to_linear;
        linear_to_tile[2] = TILE_VA as u32;
        linear_to_tile[4] = TEXTURE_VA as u32;
        linear_to_tile[10..13].copy_from_slice(&[
            TILE_WIDTH as u32,
            TILE_HEIGHT as u32,
            TILE_PITCH as u32,
        ]);
        linear_to_tile[29..32].copy_from_slice(&[16, 16, 256]);
        linear_to_tile[52] = 0x40;
        linear_to_tile[53] = 0;
        self.execute(fifo, gr, &[linear_to_tile])
            .map_err(proof_error)?;
        arch::invalidate_dcache_to_poc_range(self.tile.as_vaddr(), TILE_SIZE);
        let copied_back = (self.tile_pixel(0, 0), self.tile_pixel(8, 8));
        scarlet::println!(
            "gm20b: linear-to-tiled pixels={:#010x}/{:#010x}",
            copied_back.0,
            copied_back.1
        );
        if !render_matches {
            return Err("SGFX tiled render/PTE readback mismatch");
        }
        if sampled != 0xffff0000 {
            return Err("SGFX tiled sampler mismatch");
        }
        if copied != (0xffff0000, 0xffff0000) {
            return Err("SGFX tiled-to-linear copy mismatch");
        }
        if copied_back != (0xff0000ff, 0xff0000ff) {
            return Err("SGFX linear-to-tiled copy mismatch");
        }
        scarlet::println!("gm20b: SGFX block-linear render/copy/sample passed");
        // A genuine 902D linear copy, also ordered by the PGRAPH fence.
        let mut copy = [0; 64];
        copy[0] = 3;
        copy[2] = IMAGE_VA as u32;
        copy[4] = TEXTURE_VA as u32;
        copy[10..13].copy_from_slice(&[16, 16, 256]);
        copy[13..17].copy_from_slice(&[0, 0, 16, 16]);
        copy[17..21].copy_from_slice(&[0, 0, 16, 16]);
        copy[29..32].copy_from_slice(&[16, 16, 256]);
        self.execute(fifo, gr, &[copy]).map_err(proof_error)?;
        arch::invalidate_dcache_to_poc_range(self.image.as_vaddr(), 4096);
        if unsafe { core::ptr::read_volatile(self.image.as_vaddr() as *const u32) } != 0xff0000ff {
            return Err("SGFX 902D copy/readback mismatch");
        }
        scarlet::println!(
            "gm20b: SGFX canonical shader pack and 902D copy passed; checksum={:#010x}",
            checksum
        );
        Ok(checksum)
    }

    fn pixel(&self, x: usize, y: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.image.as_vaddr() + y * 256 + x * 4) as *const u32) }
    }

    fn tile_pixel(&self, x: usize, y: usize) -> u32 {
        let xb = x * 4;
        let offset = (y / 128) * TILE_PITCH * 128
            + (xb / 64) * 512 * 16
            + ((y % 128) / 8) * 512
            + ((xb % 64) / 32) * 256
            + ((y % 8) / 2) * 64
            + ((xb % 32) / 16) * 32
            + (y % 2) * 16
            + xb % 16;
        unsafe { core::ptr::read_volatile((self.tile.as_vaddr() + offset) as *const u32) }
    }
}

fn probe_clear() -> [u32; 64] {
    let mut w = [0; 64];
    w[0] = 1;
    w[2] = IMAGE_VA as u32;
    w[10..13].copy_from_slice(&[16, 16, 256]);
    w[13..17].copy_from_slice(&[0, 0, 16, 16]);
    w[32..36].copy_from_slice(&[1f32.to_bits(), 0, 0, 1f32.to_bits()]);
    w
}

fn probe_draw(variant: PipelineVariant) -> [u32; 64] {
    let mut w = [0; 64];
    w[0] = 2;
    w[2] = IMAGE_VA as u32;
    w[4] = VERTEX_VA as u32;
    w[10..13].copy_from_slice(&[16, 16, 256]);
    w[13..17].copy_from_slice(&[0, 0, 16, 16]);
    w[17..21].copy_from_slice(&[0, 0, 16, 16]);
    w[21] = variant as u32;
    w[23] = variant.stride();
    w[25] = 3;
    w[28] = 3 * variant.stride();
    for i in 0..4 {
        w[32 + i * 5] = 1f32.to_bits();
    }
    w[49] = 1f32.to_bits();
    w[51] = 1f32.to_bits();
    w
}

fn proof_error(error: GpuBackendSubmitError) -> &'static str {
    match error {
        GpuBackendSubmitError::Rejected(reason)
        | GpuBackendSubmitError::Unavailable(reason)
        | GpuBackendSubmitError::DeviceLost(reason) => reason,
    }
}

struct Push {
    words: Vec<u32>,
}
impl Push {
    fn new() -> Self {
        Self { words: Vec::new() }
    }
    fn packet(
        &mut self,
        opcode: u32,
        sub: u32,
        method: u32,
        data: &[u32],
    ) -> Result<(), &'static str> {
        if data.is_empty()
            || data.len() > 0x1fff
            || method & 3 != 0
            || method >= 0x8000
            || sub > 7
            || self
                .words
                .len()
                .checked_add(data.len() + 1)
                .is_none_or(|n| n > PUSH_SIZE / 4)
        {
            return Err("trusted graphics packet budget invalid");
        }
        self.words
            .try_reserve(data.len() + 1)
            .map_err(|_| "graphics push allocation failed")?;
        self.words
            .push((opcode << 29) | ((data.len() as u32) << 16) | (sub << 13) | (method >> 2));
        self.words.extend_from_slice(data);
        Ok(())
    }
    fn method(&mut self, sub: u32, method: u32, data: &[u32]) -> Result<(), &'static str> {
        self.packet(1, sub, method, data)
    }
    fn one(&mut self, method: u32, value: u32) -> Result<(), &'static str> {
        self.method(0, method, &[value])
    }
    fn address(&mut self, method: u32, address: u64) -> Result<(), &'static str> {
        self.method(0, method, &[(address >> 32) as u32, address as u32])
    }
    fn cb(&mut self, address: u64, offset: u32, data: &[u32]) -> Result<(), &'static str> {
        self.method(0, CB_SIZE, &[256, (address >> 32) as u32, address as u32])?;
        let mut payload = Vec::new();
        payload
            .try_reserve(data.len() + 1)
            .map_err(|_| "CB upload allocation failed")?;
        payload.push(offset);
        payload.extend_from_slice(data);
        self.packet(5, 0, CB_POS, &payload)
    }
    fn initialize(&mut self) -> Result<(), &'static str> {
        self.method(0, 0, &[0xb197])?;
        self.method(3, 0, &[0x902d])?;
        self.method(3, 0x260, &[0])?; // NVC0_2D_SINGLE_GPC
        self.method(3, 0x26c, &[1])?; // NV50_2D_COND_MODE_ALWAYS
        self.method(3, 0x29c, &[0])?; // COLOR_KEY_ENABLE
        self.method(3, 0x884, &[0x3f])?; // SET_PIXELS_FROM_MEMORY_CORRAL_SIZE
        self.method(3, 0x888, &[1])?; // SET_PIXELS_FROM_MEMORY_SAFE_OVERLAP
        self.method(3, 0x290, &[0])?;
        self.method(3, 0x2ac, &[3])?;
        for (m, v) in [
            (COND_MODE, COND_MODE_ALWAYS),
            (WATCHDOG_TIMER, 0x17),
            (ZETA_COMP_ENABLE, 0),
            (RT_CONTROL, 1),
            (CSAA_ENABLE, 0),
            (MULTISAMPLE_ENABLE, 0),
            (MULTISAMPLE_MODE, 0),
            (MULTISAMPLE_CTRL, 0),
            (BLEND_SEPARATE_ALPHA, 1),
            (BLEND_ENABLE_COMMON, 0),
            (0x12e4, 0), // BLEND_INDEPENDENT, use common blend equations
            (SHADE_MODEL, SHADE_MODEL_SMOOTH),
            (TEX_CB_INDEX, 15),
            (CALL_LIMIT_LOG, 8),
            (CACHE_SPLIT, CACHE_SPLIT_48K_SHARED_16K_L1),
            (SCREEN_Y_CONTROL, 0),
            (ZCULL_REGION, 0x3f),
            (CLIP_RECTS_EN, 0),
            (CLIPID_ENABLE, 0),
            (CLEAR_FLAGS, 0),
            (VIEWPORT_TRANSFORM_EN, 1),
            (VIEW_VOLUME_CLIP_CTRL, VIEW_VOLUME_CLIP_CTRL_UNK1_UNK1),
            (RASTERIZE_ENABLE, 1),
            (RT_SEPARATE_FRAG_DATA, 1),
            (LAYER, 0),
            (POINT_COORD_REPLACE, 0),
            (EDGEFLAG, 1),
            (ZETA_ENABLE, 0),
            (DEPTH_TEST_ENABLE, 0),
            (DEPTH_WRITE_ENABLE, 0),
            (ALPHA_TEST_ENABLE, 0),
            (STENCIL_ENABLE, 0),
            (LOGIC_OP_ENABLE, 0),
            (0x1234, 0), // LINKED_TSC, separate TIC/TSC handles in CB15
            (COLOR_MASK, 0x1111),
            (POLYGON_MODE_FRONT, 0x1b02),
            (POLYGON_MODE_BACK, 0x1b02),
            (LOCAL_BASE, 0xff000000),
            (INVALIDATE_SHADER_CACHES, INVALIDATE_SHADER_CACHE_READS),
        ] {
            self.one(m, v)?;
        }
        // Mesa nvc0_vbo emits VERTEX_ARRAY_FLUSH (0x142c) only before
        // GM107. It is absent from Maxwell B's class and raises ILLEGAL_MTHD.
        self.method(0, RT_COMP_ENABLE, &[0; 8])?;
        self.method(0, WINDOW_OFFSET_X, &[0, 0])?;
        // The documented Mesa GM200 branch, excluding old-Fermi methods.
        for (m, v) in [
            (0x10cc, 0xff),
            (0x10e0, 0xff),
            (0x10e4, 0xff),
            (0x10ec, 0xff),
            (0x10f0, 0xff),
            (0x074c, 0x3f),
            (0x16a8, 0x30003),
            (0x1794, 0x20002),
            (0x0218, 0x10),
            (0x10fc, 0x10),
            (0x1290, 0x10),
            (0x12d8, 0x10),
            (0x12dc, 0x10),
            (0x1140, 0x10),
            (0x1610, 0xe),
            (VERTEX_ID_GEN_MODE, 0x1000), // DRAW_ARRAYS_ADD_START
            (0x030c, 0),
            (0x0300, 3),
            (0x02d0, 0x3fffff),
            (0x0fdc, 1),
            (0x19c0, 1),
        ] {
            self.one(m, v)?;
        }
        self.address(CODE_ADDRESS_HIGH, PROGRAM_VA as u64)?;
        self.address(VERTEX_RUNOUT_ADDRESS_HIGH, (AUX_VA + 0xf00) as u64)?;
        self.method(0, TIC_ADDRESS_HIGH, &[0, DESCRIPTOR_VA as u32, 0])?;
        self.method(0, TSC_ADDRESS_HIGH, &[0, (DESCRIPTOR_VA + 0x200) as u32, 0])?;
        for stage in 0..6 {
            self.one(SP_SELECT + stage * 0x40, stage << 4)?;
        }
        for stage in [0, 4] {
            self.method(0, CB_SIZE, &[256, 0, AUX_VA as u32])?;
            self.one(CB_BIND + stage * 0x20, 1)?;
            self.method(0, CB_SIZE, &[256, 0, (AUX_VA + 0x400) as u32])?;
            self.one(CB_BIND + stage * 0x20, 0xf1)?;
        }
        self.one(0x0f90, 0)?; // independent color masks, nvc0 blend state
        Ok(())
    }
    fn target(&mut self, w: &[u32; 64]) -> Result<(), &'static str> {
        let tiled = w[52] == 0x40;
        self.method(
            0,
            RT_ADDRESS_HIGH,
            &[
                w[3],
                w[2],
                if tiled { w[10] } else { w[12] },
                w[11],
                BGRA8,
                if tiled { 0x40 } else { RT_TILE_MODE_LINEAR },
                1,
                0, // Mesa sets layer_stride only for array_size > 1.
                0,
            ],
        )?;
        self.method(
            0,
            SCREEN_SCISSOR_HORIZ,
            &[(w[15] << 16) | w[13], (w[16] << 16) | w[14]],
        )
    }
    fn operation(&mut self, w: &[u32; 64]) -> Result<(), &'static str> {
        match w[0] {
            1 => {
                self.target(w)?;
                self.one(SCISSOR_ENABLE, 0)?;
                self.method(0, CLEAR_COLOR, &w[32..36])?;
                self.one(CLEAR_BUFFERS, 0x3c)
            }
            2 => self.draw(w),
            3 => self.copy(w),
            _ => Err("unsupported canonical operation"),
        }
    }
    fn draw(&mut self, w: &[u32; 64]) -> Result<(), &'static str> {
        let variant = PipelineVariant::from_raw(w[21]).ok_or("invalid pipeline variant")?;
        let (vs, fs) = variant.shaders();
        self.target(w)?;
        self.one(SERIALIZE, 0)?; // retire previous draw before shared CB/descriptor writes
        self.cb(AUX_VA as u64, 0, &w[32..52])?;
        let sx = w[10] as f32 * 0.5;
        let sy = w[11] as f32 * 0.5;
        // SGFX upper-left viewport, depth -1..1 -> 0..1.
        self.method(
            0,
            VIEWPORT_TRANSLATE_X,
            &[sx.to_bits(), sy.to_bits(), 0.5f32.to_bits()],
        )?;
        self.method(
            0,
            VIEWPORT_SCALE_X,
            &[sx.to_bits(), (-sy).to_bits(), 0.5f32.to_bits()],
        )?;
        self.method(0, VIEWPORT_HORIZ, &[w[10] << 16, w[11] << 16])?;
        self.one(VIEWPORT_SWIZZLE, 0x6420)?;
        self.method(0, DEPTH_RANGE_NEAR, &[0, 1f32.to_bits()])?;
        self.method(
            0,
            SCISSOR_ENABLE,
            &[
                1,
                ((w[17] + w[19]) << 16) | w[17],
                ((w[18] + w[20]) << 16) | w[18],
            ],
        )?;
        let cull = (w[22] >> 2) & 7;
        self.one(CULL_FACE_ENABLE, u32::from(cull & 3 != 0))?;
        self.one(CULL_FACE, if cull & 3 == 1 { 0x404 } else { 0x405 })?;
        self.one(FRONT_FACE, if cull & 4 != 0 { 0x900 } else { 0x901 })?;
        self.one(BLEND_ENABLE, u32::from(w[22] & 1 != 0))?;
        // nvc0_blend_fac uses NV50_BLEND_FACTOR enums (0x4xxx), not the
        // similarly named OpenGL factor values. Preserve straight alpha.
        self.method(
            0,
            BLEND_EQUATION_RGB,
            &[0x8006, 0x4302, 0x4303, 0x8006, 0x4001],
        )?;
        self.one(BLEND_FUNC_DST_ALPHA, 0x4303)?;
        for (stage, shader) in [(1, vs), (5, fs)] {
            self.method(
                0,
                SP_SELECT + stage * 0x40,
                &[(stage << 4) | 1, shader.start()],
            )?;
            self.one(SP_GPR_ALLOC + stage * 0x40, shader.gprs())?;
        }
        self.method(0, 0x0360, &[0x20164010, 0x20])?;
        let attr: &[(u32, u32)] = match variant {
            PipelineVariant::Stride16Solid => &[(0, 2)],
            PipelineVariant::Stride16TextureRgba | PipelineVariant::Stride16TextureAlphaMask => {
                &[(0, 2), (8, 2)]
            }
            PipelineVariant::Stride40VertexColor | PipelineVariant::Stride32VertexColor => {
                &[(0, 4), (16, 4)]
            }
            PipelineVariant::Stride40TextureVertexColorRgba => &[(0, 4), (16, 4), (32, 2)],
            PipelineVariant::Stride28VertexColor => &[(0, 4), (16, 3)],
            PipelineVariant::Stride24TextureRgba
            | PipelineVariant::Stride24TextureRgbIgnoreAlpha
            | PipelineVariant::Stride24TextureAlphaMask => &[(0, 4), (16, 2)],
            _ => &[(0, 4)],
        };
        self.method(
            0,
            VERTEX_ARRAY_FETCH,
            &[VERTEX_ARRAY_FETCH_ENABLE | w[23], w[5], w[4], 0],
        )?;
        let vertex = (u64::from(w[5]) << 32) | u64::from(w[4]);
        self.address(VERTEX_ARRAY_LIMIT_HIGH, vertex + u64::from(w[28]) - 1)?;
        self.one(VERTEX_ARRAY_PER_INSTANCE, 0)?;
        for i in 0..16 {
            self.one(
                VERTEX_ATTRIB_FORMAT + i * 4,
                if let Some(&(off, count)) = attr.get(i as usize) {
                    VERTEX_ATTRIB_FORMAT_TYPE_FLOAT
                        | match count {
                            2 => VERTEX_ATTRIB_FORMAT_SIZE_32_32,
                            3 => VERTEX_ATTRIB_FORMAT_SIZE_32_32_32,
                            _ => VERTEX_ATTRIB_FORMAT_SIZE_32_32_32_32,
                        }
                        | (off << 7)
                } else {
                    VERTEX_ATTRIB_FORMAT_TYPE_FLOAT
                        | VERTEX_ATTRIB_FORMAT_SIZE_32_32_32_32
                        | VERTEX_ATTRIB_FORMAT_CONST
                },
            )?;
            if i > 0 {
                self.one(VERTEX_ARRAY_FETCH + i * 0x10, 0)?;
            }
        }
        if w[29] != 0 {
            let channels = if w[22] & (1 << 5) != 0 {
                [7, 7, 7, 5]
            } else {
                [4, 3, 2, 5]
            };
            let tic0 = 8
                | (2 << 7)
                | (2 << 10)
                | (2 << 13)
                | (2 << 16)
                | (channels[0] << 19)
                | (channels[1] << 22)
                | (channels[2] << 25)
                | (channels[3] << 28);
            let tiled = w[53] == 0x40;
            let tic = [
                tic0,
                w[6],
                w[7] | if tiled { 0x00600000 } else { 0x00400000 },
                0x10000 | if tiled { 0x20 } else { w[31] >> 5 },
                (if tiled { 0xe0800000 } else { 0xe3800000 }) | (w[29] - 1),
                0x80000000 | (w[30] - 1),
                0,
                0,
            ];
            let tsc = [
                0x26000 | 2 | (2 << 3) | (2 << 6),
                if w[22] & 2 != 0 { 0x62 } else { 0x51 },
                0,
                0,
                0,
                0,
                0,
                0,
            ];
            self.cb(DESCRIPTOR_VA as u64, 0, &tic)?;
            self.cb((DESCRIPTOR_VA + 0x200) as u64, 0, &tsc)?;
            self.cb((AUX_VA + 0x400) as u64, 0x20, &[0])?;
            self.one(TIC_FLUSH, 0)?;
            self.one(TSC_FLUSH, 0)?;
            self.one(TEX_CACHE_CTL, 0)?;
        }
        self.one(VB_ELEMENT_BASE, w[27])?;
        if w[26] != 0 {
            let index = u64::from(w[8]) | (u64::from(w[9]) << 32);
            let bytes = u64::from(w[24] + w[25]) * (if w[26] == 1 { 2 } else { 4 });
            self.method(
                0,
                INDEX_ARRAY_START_HIGH,
                &[
                    w[9],
                    w[8],
                    ((index + bytes - 1) >> 32) as u32,
                    (index + bytes - 1) as u32,
                    w[26],
                ],
            )?;
        }
        self.one(VERTEX_BEGIN_GL, 4)?;
        self.method(
            0,
            if w[26] == 0 {
                VERTEX_BUFFER_FIRST
            } else {
                INDEX_BATCH_FIRST
            },
            &[w[24], w[25]],
        )?;
        self.one(VERTEX_END_GL, 0)
    }
    fn copy(&mut self, w: &[u32; 64]) -> Result<(), &'static str> {
        self.one(SERIALIZE, 0)?;
        for (m, addr, width, height, stride, tile_mode) in [
            (0x200, [w[3], w[2]], w[10], w[11], w[12], w[52]),
            (0x230, [w[5], w[4]], w[29], w[30], w[31], w[53]),
        ] {
            if tile_mode == 0x40 {
                self.method(3, m, &[BGRA8, 0, tile_mode, 1, 0])?;
                self.method(3, m + 0x18, &[width, height, addr[0], addr[1]])?;
            } else {
                self.method(3, m, &[BGRA8, 1])?;
                self.method(3, m + 0x14, &[stride, width, height, addr[0], addr[1]])?;
            }
        }
        self.method(3, 0x88c, &[0])?;
        let dst = [w[13], w[14], w[15], w[16]];
        self.method(3, 0x8b0, &dst)?;
        self.method(3, 0x8c0, &[0, 1, 0, 1])?;
        let src = [0, w[17], 0, w[18]];
        self.method(3, 0x8d0, &src)?;
        self.one(SERIALIZE, 0)?;
        self.one(INVALIDATE_SHADER_CACHES, INVALIDATE_SHADER_CACHE_READS)?;
        self.one(TEX_CACHE_CTL, 0)
    }
}

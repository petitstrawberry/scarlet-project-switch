// SPDX-License-Identifier: GPL-2.0-only
//! Canonical SGFX operations. The kernel validates these records and emits
//! Nouveau B197/902D methods; these are deliberately not raw GPU methods.

use crate::model::*;
use alloc::vec::Vec;
use maxwell_shader_pack::PipelineVariant;
use sgfx_core::ir::{Color, CompareFunction, IndexFormat, PixelRect};

#[derive(Clone, Copy)]
pub(crate) struct Surface {
    pub object: ObjectId,
    pub plane_offset: u64,
    pub plane_size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub tile_mode: u32,
    pub alpha_mask: bool,
}
#[derive(Clone, Copy)]
pub(crate) struct DepthDrawState {
    pub target: Surface,
    pub compare: CompareFunction,
    pub write_enabled: bool,
}
#[derive(Clone, Copy)]
pub(crate) struct IndexedDraw {
    pub index: ObjectId,
    pub index_offset: u64,
    pub index_size: u64,
    pub format: IndexFormat,
    pub index_count: u32,
    pub first_index: u32,
    pub base_vertex: u32,
    pub max_indices: u32,
    pub vertex_size: u64,
}
#[derive(Clone, Copy)]
pub(crate) enum DrawCall {
    NonIndexed {
        first_vertex: u32,
        vertex_count: u32,
    },
    Indexed(IndexedDraw),
}
#[derive(Clone, Copy)]
pub(crate) struct DrawState {
    pub variant: PipelineVariant,
    pub target: Surface,
    pub area: PixelRect,
    pub scissor: PixelRect,
    pub vertex: ObjectId,
    pub vertex_offset: u64,
    pub vertex_size: u64,
    pub stride: u32,
    pub attributes: &'static [(u32, u32)],
    pub uniforms: [u32; 20],
    pub texture: Option<Surface>,
    pub linear_sampler: bool,
    pub source_over: bool,
    pub cull: u32,
    pub depth: Option<DepthDrawState>,
    pub draw: DrawCall,
}

pub(crate) struct Emitter {
    artifact: RelocatableCommands,
    limit: usize,
}
impl Emitter {
    pub fn new(limit: u32) -> Self {
        Self {
            limit: limit as usize,
            artifact: RelocatableCommands {
                words: Vec::new(),
                fixups: Vec::new(),
                accesses: Vec::new(),
                generated_objects: Vec::new(),
            },
        }
    }
    fn record(&mut self, words: [u32; 64]) -> Result<u32, CompileError> {
        let start = self.artifact.words.len();
        if start.checked_add(64).is_none_or(|n| n > self.limit) {
            return Err(CompileError::CommandBudgetExceeded);
        }
        self.artifact
            .words
            .try_reserve(64)
            .map_err(|_| CompileError::OutOfMemory)?;
        self.artifact.words.extend_from_slice(&words);
        u32::try_from(start).map_err(|_| CompileError::Overflow)
    }
    fn address(
        &mut self,
        word: u32,
        object: ObjectRef,
        offset: u64,
        size: u64,
        access: Access,
    ) -> Result<(), CompileError> {
        if size == 0 {
            return Err(CompileError::OutOfBounds);
        }
        let end = offset.checked_add(size).ok_or(CompileError::Overflow)?;
        self.artifact
            .fixups
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        self.artifact.fixups.push(SymbolicAddress {
            word_offset: word,
            object,
            object_offset: offset,
            required_size: size,
            access,
            encoding: AddressEncoding::GpuVa64,
        });
        if let Some(a) = self
            .artifact
            .accesses
            .iter_mut()
            .find(|a| a.object == object)
        {
            let old_end = a.offset.checked_add(a.size).ok_or(CompileError::Overflow)?;
            a.offset = a.offset.min(offset);
            a.size = old_end.max(end) - a.offset;
            a.access = a.access | access;
        } else {
            self.artifact
                .accesses
                .try_reserve(1)
                .map_err(|_| CompileError::OutOfMemory)?;
            self.artifact.accesses.push(ResourceAccess {
                object,
                offset,
                size,
                access,
            });
        }
        Ok(())
    }
    fn surface(&mut self, word: u32, s: Surface, access: Access) -> Result<(), CompileError> {
        if s.tile_mode != 0 && s.tile_mode != 0x40 {
            return Err(CompileError::UnsupportedFeature);
        }
        self.address(
            word,
            ObjectRef::External(s.object),
            s.plane_offset,
            s.plane_size,
            access,
        )
    }
    pub fn clear(
        &mut self,
        target: Surface,
        rect: PixelRect,
        color: Color,
    ) -> Result<(), CompileError> {
        let mut w = [0; 64];
        w[0] = 1;
        layout(&mut w, 10, target);
        rectangle(&mut w, 13, rect);
        for (dst, v) in w[32..36].iter_mut().zip(color.components()) {
            *dst = v.to_bits();
        }
        let base = self.record(w)?;
        self.surface(base + 2, target, Access::WRITE)
    }
    pub fn copy(
        &mut self,
        source: Surface,
        src: PixelRect,
        destination: Surface,
        dst: PixelRect,
    ) -> Result<(), CompileError> {
        let mut w = [0; 64];
        w[0] = 3;
        layout(&mut w, 10, destination);
        rectangle(&mut w, 13, dst);
        rectangle(&mut w, 17, src);
        layout(&mut w, 29, source);
        let base = self.record(w)?;
        self.surface(base + 2, destination, Access::WRITE)?;
        self.surface(base + 4, source, Access::READ)
    }
    pub fn upload_buffer(
        &mut self,
        _destination: ObjectId,
        _offset: u64,
        size: u64,
        data: &[u8],
    ) -> Result<(), CompileError> {
        if size != data.len() as u64 {
            return Err(CompileError::OutOfBounds);
        }
        if size == 0 {
            return Ok(());
        }
        // The initial opaque dialect executes graphics and image copies.
        // The backend drains earlier work and performs WriteBuffer/WriteTexture
        // through generic CPU mappings and image upload capabilities. Arbitrary
        // byte ranges must not be lowered as aligned 902D image surfaces.
        Err(CompileError::UnsupportedFeature)
    }
    pub fn draw(&mut self, s: DrawState) -> Result<(), CompileError> {
        if s.stride != s.variant.stride() || s.attributes.is_empty() {
            return Err(CompileError::InvalidResource);
        }
        let mut w = [0; 64];
        w[0] = 2;
        layout(&mut w, 10, s.target);
        rectangle(&mut w, 13, s.area);
        rectangle(&mut w, 17, s.scissor);
        w[21] = s.variant as u32;
        w[22] = u32::from(s.source_over) | (u32::from(s.linear_sampler) << 1) | (s.cull << 2);
        w[23] = s.stride;
        w[28] = u32::try_from(s.vertex_size).map_err(|_| CompileError::Overflow)?;
        if let Some(t) = s.texture {
            layout(&mut w, 29, t);
            w[22] |= u32::from(t.alpha_mask) << 5;
        }
        w[32..52].copy_from_slice(&s.uniforms);
        if let Some(depth) = s.depth {
            if s.target.tile_mode != 0x40
                || depth.target.tile_mode != 0x40
                || depth.target.width != s.target.width
                || depth.target.height != s.target.height
            {
                return Err(CompileError::InvalidResource);
            }
            w[56] = depth.target.width;
            w[57] = depth.target.height;
            w[58] = depth.target.stride;
            w[59] = depth.target.tile_mode;
            // Zero means no depth attachment. The comparison vocabulary is
            // portable; only the trusted kernel translates it to B197 enums.
            w[60] = match depth.compare {
                CompareFunction::Never => 1,
                CompareFunction::Less => 2,
                CompareFunction::Equal => 3,
                CompareFunction::LessEqual => 4,
                CompareFunction::Greater => 5,
                CompareFunction::NotEqual => 6,
                CompareFunction::GreaterEqual => 7,
                CompareFunction::Always => 8,
            };
            w[61] = u32::from(depth.write_enabled);
        }
        match s.draw {
            DrawCall::NonIndexed {
                first_vertex,
                vertex_count,
            } => {
                w[24] = first_vertex;
                w[25] = vertex_count;
            }
            DrawCall::Indexed(i) => {
                if i.max_indices == 0 || i.vertex_size != s.vertex_size {
                    return Err(CompileError::OutOfBounds);
                }
                w[24] = i.first_index;
                w[25] = i.index_count;
                w[26] = match i.format {
                    IndexFormat::Uint16 => 1,
                    IndexFormat::Uint32 => 2,
                };
                w[27] = i.base_vertex;
            }
        }
        let base = self.record(w)?;
        self.surface(base + 2, s.target, Access::READ | Access::WRITE)?;
        self.address(
            base + 4,
            ObjectRef::External(s.vertex),
            s.vertex_offset,
            s.vertex_size,
            Access::READ,
        )?;
        if let Some(t) = s.texture {
            self.surface(base + 6, t, Access::READ)?;
        }
        if let DrawCall::Indexed(i) = s.draw {
            self.address(
                base + 8,
                ObjectRef::External(i.index),
                i.index_offset,
                i.index_size,
                Access::READ,
            )?;
        }
        if let Some(depth) = s.depth {
            self.surface(
                base + 54,
                depth.target,
                if depth.write_enabled {
                    Access::READ | Access::WRITE
                } else {
                    Access::READ
                },
            )?;
        }
        Ok(())
    }
    pub fn begin_draw_batch(&mut self) -> Result<(), CompileError> {
        Ok(())
    }
    pub fn continue_draw_batch(&mut self) -> Result<(), CompileError> {
        Ok(())
    }
    pub fn end_draw_batch(&mut self) -> Result<(), CompileError> {
        Ok(())
    }
    pub fn clear_depth(
        &mut self,
        target: Surface,
        rect: PixelRect,
        value: f32,
    ) -> Result<(), CompileError> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) || target.tile_mode != 0x40 {
            return Err(CompileError::InvalidResource);
        }
        let mut w = [0; 64];
        w[0] = 4;
        layout(&mut w, 10, target);
        rectangle(&mut w, 13, rect);
        w[32] = value.to_bits();
        let base = self.record(w)?;
        self.surface(base + 2, target, Access::WRITE)
    }
    pub fn finish(mut self) -> Result<RelocatableCommands, CompileError> {
        self.artifact.fixups.sort_unstable_by_key(|f| f.word_offset);
        Ok(self.artifact)
    }
}
fn layout(w: &mut [u32; 64], at: usize, s: Surface) {
    w[at] = s.width;
    w[at + 1] = s.height;
    w[at + 2] = s.stride;
    w[if at == 10 { 52 } else { 53 }] = s.tile_mode;
}
fn rectangle(w: &mut [u32; 64], at: usize, r: PixelRect) {
    w[at] = r.x();
    w[at + 1] = r.y();
    w[at + 2] = r.width();
    w[at + 3] = r.height();
}

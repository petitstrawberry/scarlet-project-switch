//! Checked GM20B uncompressed image layouts shared by allocation and transfers.
//!
//! Array layers contain complete mip chains. Block-linear mip packing follows
//! Mesa NIL `image.rs`/`tiling.rs` and nvc0 `nvc0_miptree.c`: each LOD clamps
//! block height to its extent and each layer is aligned to the base tile size.
//! `clamp_mips = false` preserves the existing forced-H4 single-level targets.
#![no_std]

pub mod descriptor;
pub mod wire;

pub const MAX_DIMENSION: u32 = 16_384;
pub const MAX_MIP_LEVELS: usize = 15;
pub const MAX_ARRAY_LAYERS: u32 = 2048;
pub const NVIDIA_COLOR_MODIFIER_BASE: u64 = 0x0300_0000_000f_e010;
pub const NVIDIA_DEPTH_MODIFIER_BASE: u64 = 0x0300_0000_0007_b010;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidDescriptor,
    Overflow,
    OutOfBounds,
    LayoutMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutKind {
    Linear,
    BlockLinear { base_y_log2: u8, clamp_mips: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub bytes_per_pixel: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Level {
    pub offset: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub row_pitch: u32,
    /// Hardware block-height field; zero also names a tiled H0 level.
    pub tile_y_log2: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub descriptor: Descriptor,
    pub kind: LayoutKind,
    pub levels: [Level; MAX_MIP_LEVELS],
    pub array_pitch: u32,
    pub total_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subresource {
    pub offset: u64,
    pub level: Level,
    pub layer: u32,
    pub mip_level: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transfer {
    pub offset: u64,
    pub row_pitch: u32,
    pub array_pitch: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

fn align(value: u64, alignment: u64) -> Result<u64, Error> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or(Error::Overflow)
}

/// The texture unit performs this clamp independently at every LOD.
pub fn tile_y_log2(height: u32, maximum: u8) -> u8 {
    let gobs = height.div_ceil(8).max(1);
    let required = (32 - (gobs - 1).leading_zeros()) as u8;
    maximum.min(required)
}

pub fn color_modifier(tile_y_log2: u8) -> Result<u64, Error> {
    if tile_y_log2 > 4 {
        return Err(Error::InvalidDescriptor);
    }
    Ok(NVIDIA_COLOR_MODIFIER_BASE | u64::from(tile_y_log2))
}

pub fn modifier_tile_y(modifier: u64) -> Option<u8> {
    let y = (modifier & 0xf) as u8;
    ((modifier & !0xf == NVIDIA_COLOR_MODIFIER_BASE
        || modifier & !0xf == NVIDIA_DEPTH_MODIFIER_BASE)
        && y <= 4)
        .then_some(y)
}

pub fn plan(descriptor: Descriptor, kind: LayoutKind) -> Result<Layout, Error> {
    let d = descriptor;
    if d.width == 0
        || d.height == 0
        || d.width > MAX_DIMENSION
        || d.height > MAX_DIMENSION
        || d.array_layers == 0
        || d.array_layers > MAX_ARRAY_LAYERS
        || !matches!(d.bytes_per_pixel, 1 | 2 | 4)
        || d.mip_levels == 0
        || d.mip_levels > d.width.max(d.height).ilog2() + 1
        || d.mip_levels as usize > MAX_MIP_LEVELS
    {
        return Err(Error::InvalidDescriptor);
    }
    if let LayoutKind::BlockLinear {
        base_y_log2,
        clamp_mips,
    } = kind
    {
        if base_y_log2 > 4 || (!clamp_mips && (d.mip_levels != 1 || d.array_layers != 1)) {
            return Err(Error::InvalidDescriptor);
        }
    }
    // Pitch-linear descriptors are explicitly TWO_D_NO_MIPMAP. Arrays and
    // mip chains require the hardware's block-linear addressing contract.
    if kind == LayoutKind::Linear && (d.mip_levels != 1 || d.array_layers != 1) {
        return Err(Error::InvalidDescriptor);
    }
    let mut result = Layout {
        descriptor: d,
        kind,
        levels: [Level::default(); MAX_MIP_LEVELS],
        array_pitch: 0,
        total_size: 0,
    };
    let mut cursor = 0;
    for mip in 0..d.mip_levels {
        let width = (d.width >> mip).max(1);
        let height = (d.height >> mip).max(1);
        let (pitch_alignment, y) = match kind {
            LayoutKind::Linear => (256, 0),
            LayoutKind::BlockLinear {
                base_y_log2,
                clamp_mips,
            } => (
                64,
                if clamp_mips {
                    tile_y_log2(height, base_y_log2)
                } else {
                    base_y_log2
                },
            ),
        };
        let pitch = align(
            u64::from(width) * u64::from(d.bytes_per_pixel),
            pitch_alignment,
        )?;
        let storage_height = if kind == LayoutKind::Linear {
            u64::from(height)
        } else {
            align(u64::from(height), 8 << y)?
        };
        let size = pitch.checked_mul(storage_height).ok_or(Error::Overflow)?;
        result.levels[mip as usize] = Level {
            offset: cursor,
            size,
            width,
            height,
            row_pitch: u32::try_from(pitch).map_err(|_| Error::Overflow)?,
            tile_y_log2: y,
        };
        cursor = cursor.checked_add(size).ok_or(Error::Overflow)?;
    }
    let layer_alignment = match kind {
        LayoutKind::Linear => 1,
        LayoutKind::BlockLinear { .. } => 512 << result.levels[0].tile_y_log2,
    };
    // Preserve exact legacy single-level allocation sizes; multilayer chains
    // need a tile-aligned stride because hardware calculates it implicitly.
    let array_pitch = if d.array_layers > 1 {
        align(cursor, layer_alignment)?
    } else {
        cursor
    };
    result.array_pitch = u32::try_from(array_pitch).map_err(|_| Error::Overflow)?;
    result.total_size = array_pitch
        .checked_mul(u64::from(d.array_layers))
        .ok_or(Error::Overflow)?;
    Ok(result)
}

impl Layout {
    pub fn subresource(&self, mip_level: u32, layer: u32) -> Result<Subresource, Error> {
        if mip_level >= self.descriptor.mip_levels || layer >= self.descriptor.array_layers {
            return Err(Error::OutOfBounds);
        }
        let level = self.levels[mip_level as usize];
        let offset = u64::from(layer)
            .checked_mul(u64::from(self.array_pitch))
            .and_then(|o| o.checked_add(level.offset))
            .ok_or(Error::Overflow)?;
        Ok(Subresource {
            offset,
            level,
            layer,
            mip_level,
        })
    }

    pub fn transfer(
        &self,
        mip_level: u32,
        layer: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<Transfer, Error> {
        let sub = self.subresource(mip_level, layer)?;
        if width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|e| e > sub.level.width)
            || y.checked_add(height).is_none_or(|e| e > sub.level.height)
        {
            return Err(Error::OutOfBounds);
        }
        let offset = sub
            .offset
            .checked_add(u64::from(y) * u64::from(sub.level.row_pitch))
            .and_then(|o| o.checked_add(u64::from(x) * u64::from(self.descriptor.bytes_per_pixel)))
            .ok_or(Error::Overflow)?;
        Ok(Transfer {
            offset,
            row_pitch: sub.level.row_pitch,
            array_pitch: self.array_pitch,
            x,
            y,
            width,
            height,
        })
    }

    /// Identify the exact authorized subresource from a validated staging
    /// rectangle. Padding, another level's pitch, and partial-layer aliases
    /// must never select private GPU bytes outside the declared rectangle.
    pub fn resolve_transfer(&self, transfer: Transfer) -> Result<Subresource, Error> {
        if transfer.array_pitch != self.array_pitch {
            return Err(Error::LayoutMismatch);
        }
        let pixel_offset = u64::from(transfer.y) * u64::from(transfer.row_pitch)
            + u64::from(transfer.x) * u64::from(self.descriptor.bytes_per_pixel);
        let base = transfer
            .offset
            .checked_sub(pixel_offset)
            .ok_or(Error::LayoutMismatch)?;
        let layer =
            u32::try_from(base / u64::from(self.array_pitch)).map_err(|_| Error::OutOfBounds)?;
        let local = base % u64::from(self.array_pitch);
        let mip = self.levels[..self.descriptor.mip_levels as usize]
            .iter()
            .position(|l| l.offset == local && l.row_pitch == transfer.row_pitch)
            .ok_or(Error::LayoutMismatch)? as u32;
        let expected = self.transfer(
            mip,
            layer,
            transfer.x,
            transfer.y,
            transfer.width,
            transfer.height,
        )?;
        if expected != transfer {
            return Err(Error::LayoutMismatch);
        }
        self.subresource(mip, layer)
    }

    /// Private GPU byte address for a valid logical pixel byte.
    pub fn byte_offset(&self, sub: Subresource, x_byte: u32, y: u32) -> Result<u64, Error> {
        if self.subresource(sub.mip_level, sub.layer)? != sub
            || x_byte >= sub.level.width * self.descriptor.bytes_per_pixel
            || y >= sub.level.height
        {
            return Err(Error::OutOfBounds);
        }
        let local = if self.kind == LayoutKind::Linear {
            u64::from(y) * u64::from(sub.level.row_pitch) + u64::from(x_byte)
        } else {
            block_linear_offset(x_byte, y, sub.level.row_pitch, sub.level.tile_y_log2)?
        };
        sub.offset
            .checked_add(local)
            .filter(|&o| o < self.total_size)
            .ok_or(Error::OutOfBounds)
    }
}

pub fn block_linear_offset(x_byte: u32, y: u32, pitch: u32, tile_y_log2: u8) -> Result<u64, Error> {
    if tile_y_log2 > 4 || pitch == 0 || pitch & 63 != 0 || x_byte >= pitch {
        return Err(Error::InvalidDescriptor);
    }
    let block_height = 8u64 << tile_y_log2;
    let x = u64::from(x_byte);
    let y = u64::from(y);
    let block = ((y / block_height) * u64::from(pitch / 64) + x / 64) * (512 << tile_y_log2);
    Ok(block
        + ((y % block_height) / 8) * 512
        + ((x % 64) / 32) * 256
        + ((y % 8) / 2) * 64
        + ((x % 32) / 16) * 32
        + (y % 2) * 16
        + x % 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn desc(w: u32, h: u32, mips: u32, layers: u32) -> Descriptor {
        Descriptor {
            width: w,
            height: h,
            mip_levels: mips,
            array_layers: layers,
            bytes_per_pixel: 4,
        }
    }
    fn tiled() -> LayoutKind {
        LayoutKind::BlockLinear {
            base_y_log2: 4,
            clamp_mips: true,
        }
    }
    #[test]
    fn mip_chain_matches_mesa_array_packing() {
        let l = plan(desc(129, 65, 8, 6), tiled()).unwrap();
        assert_eq!(
            (
                l.levels[0].row_pitch,
                l.levels[0].size,
                l.levels[0].tile_y_log2
            ),
            (576, 73728, 4)
        );
        assert_eq!(
            (
                l.levels[1].offset,
                l.levels[1].size,
                l.levels[1].tile_y_log2
            ),
            (73728, 8192, 2)
        );
        assert_eq!(
            (
                l.levels[2].offset,
                l.levels[2].size,
                l.levels[2].tile_y_log2
            ),
            (81920, 2048, 1)
        );
        assert_eq!(l.array_pitch, 90112);
        assert_eq!(
            l.subresource(7, 5).unwrap().offset,
            u64::from(l.array_pitch) * 5 + l.levels[7].offset
        );
        assert_eq!(l.total_size, 540672);
    }
    #[test]
    fn preserves_forced_h4_legacy_targets() {
        let l = plan(
            desc(16, 1, 1, 1),
            LayoutKind::BlockLinear {
                base_y_log2: 4,
                clamp_mips: false,
            },
        )
        .unwrap();
        assert_eq!(l.array_pitch, 8192);
        assert_eq!(l.levels[0].tile_y_log2, 4);
        assert_eq!(plan(desc(16, 1, 1, 6), tiled()).unwrap().array_pitch, 512);
    }
    #[test]
    fn transfers_cannot_alias_padding_or_another_mip() {
        let l = plan(desc(129, 65, 8, 6), tiled()).unwrap();
        let t = l.transfer(2, 4, 1, 2, 3, 4).unwrap();
        assert_eq!(l.resolve_transfer(t).unwrap(), l.subresource(2, 4).unwrap());
        assert_eq!(
            l.resolve_transfer(Transfer {
                offset: t.offset + 1,
                ..t
            }),
            Err(Error::LayoutMismatch)
        );
        assert_eq!(
            l.resolve_transfer(Transfer {
                row_pitch: t.row_pitch + 64,
                ..t
            }),
            Err(Error::LayoutMismatch)
        );
        assert!(l.transfer(2, 4, u32::MAX, 0, 1, 1).is_err());
        assert!(l.transfer(8, 0, 0, 0, 1, 1).is_err());
        assert!(l.transfer(0, 6, 0, 0, 1, 1).is_err());
    }
    #[test]
    fn byte_offsets_are_disjoint_across_mips_and_layers() {
        extern crate alloc;
        let l = plan(desc(65, 33, 7, 6), tiled()).unwrap();
        let mut seen = alloc::vec![false; l.total_size as usize];
        for layer in 0..6 {
            for mip in 0..7 {
                let sub = l.subresource(mip, layer).unwrap();
                for y in 0..sub.level.height {
                    for x in 0..sub.level.width * 4 {
                        let o = l.byte_offset(sub, x, y).unwrap() as usize;
                        assert!(!seen[o]);
                        seen[o] = true;
                        assert!(o as u64 >= sub.offset && (o as u64) < sub.offset + sub.level.size);
                    }
                }
            }
        }
    }
    #[test]
    fn rejects_oversize_invalid_and_unrepresentable_descriptors() {
        assert!(plan(desc(0, 1, 1, 1), tiled()).is_err());
        assert!(plan(desc(16385, 1, 1, 1), tiled()).is_err());
        assert!(plan(desc(1, 1, 2, 1), tiled()).is_err());
        assert!(plan(desc(16384, 16384, 15, 2049), tiled()).is_err());
        assert!(plan(desc(16, 16, 2, 1), LayoutKind::Linear).is_err());
        assert!(color_modifier(5).is_err());
        assert_eq!(modifier_tile_y(0), None);
    }
}

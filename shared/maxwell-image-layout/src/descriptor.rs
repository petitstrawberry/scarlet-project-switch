//! GM107 TIC descriptors for capability-checked expanded SGFX images.
//! Fields follow pinned Mesa nvc0_tex.c and gm107_texture.xml.h.
use crate::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dimension {
    D1,
    D1Array,
    D2,
    D2Array,
    Cube,
}

#[derive(Clone, Copy, Debug)]
pub struct Texture {
    pub address: u64,
    pub width: u32,
    pub height: u32,
    pub row_pitch: u32,
    pub tile_y_log2: Option<u8>,
    /// 0 BGRA, 1 expanded RGBA, 2 R8 in alpha, 3 expanded RG8,
    /// 4/5 encoded sRGB BGRA/RGBA, 6 ZF32.
    pub format: u32,
    pub alpha_mask: bool,
    pub mip_levels: u32,
    pub first_mip: u32,
    pub last_mip: u32,
    pub layers: u32,
    pub dimension: Dimension,
}

pub fn texture(desc: Texture) -> Result<[u32; 8], Error> {
    let d = desc;
    if d.address >= 1 << 48
        || d.width == 0
        || d.width > 16384
        || d.height == 0
        || d.height > 16384
        || d.format > 6
        || d.mip_levels == 0
        || d.mip_levels > 15
        || d.first_mip > d.last_mip
        || d.last_mip >= d.mip_levels
        || d.layers == 0
        || d.layers > 2048
        || d.tile_y_log2.is_some_and(|y| y > 4)
        || (d.alpha_mask && d.format != 2)
    {
        return Err(Error::InvalidDescriptor);
    }
    if d.tile_y_log2.is_none()
        && (d.row_pitch == 0
            || d.row_pitch & 31 != 0
            || d.mip_levels != 1
            || d.layers != 1
            || d.dimension != Dimension::D2)
    {
        return Err(Error::InvalidDescriptor);
    }
    if matches!(d.dimension, Dimension::D1 | Dimension::D1Array) && d.height != 1
        || (d.dimension == Dimension::Cube && (d.width != d.height || d.layers != 6))
        || (matches!(d.dimension, Dimension::D1 | Dimension::D2) && d.layers != 1)
    {
        return Err(Error::InvalidDescriptor);
    }
    let (size, types, channels) = if d.format == 6 {
        // ZF32 is a distinct texture storage format, not an R32 reinterpretation.
        (0x2f, [7, 4, 4, 4], [2, 2, 2, 7])
    } else {
        let channels = match d.format {
            2 if d.alpha_mask => [7, 7, 7, 5],
            2 => [5, 6, 6, 7],
            3 => [4, 3, 6, 7],
            _ => [4, 3, 2, 5],
        };
        (8, [2, 2, 2, 2], channels)
    };
    let tic0 = size
        | (types[0] << 7)
        | (types[1] << 10)
        | (types[2] << 13)
        | (types[3] << 16)
        | (channels[0] << 19)
        | (channels[1] << 22)
        | (channels[2] << 25)
        | (channels[3] << 28);
    let texture_type = match d.dimension {
        Dimension::D1 => 0,
        Dimension::D1Array => 0x02000000,
        Dimension::D2 if d.tile_y_log2.is_none() => 0x03800000,
        Dimension::D2 => 0x00800000,
        Dimension::D2Array => 0x02800000,
        Dimension::Cube => 0x01800000,
    };
    let depth = if d.dimension == Dimension::Cube {
        d.layers / 6
    } else {
        d.layers
    };
    Ok([
        tic0,
        d.address as u32,
        (d.address >> 32) as u32
            | if d.tile_y_log2.is_some() {
                0x00600000
            } else {
                0x00400000
            },
        0x10000
            | ((d.mip_levels - 1) << 28)
            | if let Some(y) = d.tile_y_log2 {
                u32::from(y) << 3
            } else {
                d.row_pitch >> 5
            },
        0xe0000000
            | texture_type
            | (if matches!(d.format, 4 | 5) {
                0x00400000
            } else {
                0
            })
            | (d.width - 1),
        0x80000000 | ((depth - 1) << 16) | (d.height - 1),
        0,
        d.first_mip | (d.last_mip << 4),
    ])
}

/// Sampler clamp precision is 8 fractional bits, as in Mesa nv50_state.c.
pub fn sampler_lod(
    words: &mut [u32; 8],
    mip_levels: u32,
    linear_mip: bool,
    min: f32,
    max: f32,
) -> Result<(), Error> {
    if mip_levels == 0
        || mip_levels > 15
        || !min.is_finite()
        || !max.is_finite()
        || min < 0.0
        || max < min
    {
        return Err(Error::InvalidDescriptor);
    }
    words[1] = (words[1] & !0xc0)
        | if mip_levels == 1 {
            0x40
        } else if linear_mip {
            0xc0
        } else {
            0x80
        };
    words[2] =
        ((min.min(15.0) * 256.0) as u32 & 0xfff) | (((max.min(15.0) * 256.0) as u32 & 0xfff) << 12);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Texture {
        Texture {
            address: 0x100000,
            width: 64,
            height: 64,
            row_pitch: 256,
            tile_y_log2: Some(3),
            format: 0,
            alpha_mask: false,
            mip_levels: 7,
            first_mip: 0,
            last_mip: 6,
            layers: 1,
            dimension: Dimension::D2,
        }
    }
    #[test]
    fn mip_array_cube_fields_are_distinct() {
        let t = texture(base()).unwrap();
        assert_eq!(t[3] >> 28, 6);
        assert_eq!(t[3] & 0x38, 24);
        assert_eq!(t[7], 0x60);
        let a = texture(Texture {
            layers: 6,
            dimension: Dimension::D2Array,
            ..base()
        })
        .unwrap();
        assert_eq!((a[5] >> 16) & 0x3fff, 5);
        let c = texture(Texture {
            layers: 6,
            dimension: Dimension::Cube,
            ..base()
        })
        .unwrap();
        assert_eq!((c[5] >> 16) & 0x3fff, 0);
        assert_ne!(a[4], c[4]);
    }
    #[test]
    fn expanded_narrow_and_depth_keep_component_semantics() {
        let red = texture(Texture {
            format: 2,
            ..base()
        })
        .unwrap();
        assert_eq!((red[0] >> 19) & 7, 5);
        assert_eq!((red[0] >> 28) & 7, 7);
        let mask = texture(Texture {
            format: 2,
            alpha_mask: true,
            ..base()
        })
        .unwrap();
        assert_eq!((mask[0] >> 28) & 7, 5);
        let rg = texture(Texture {
            format: 3,
            ..base()
        })
        .unwrap();
        assert_eq!((rg[0] >> 25) & 7, 6);
        let depth = texture(Texture {
            format: 6,
            ..base()
        })
        .unwrap();
        assert_eq!(depth[0] & 0x7f, 0x2f);
        assert_eq!((depth[0] >> 7) & 7, 7);
        let srgb = texture(Texture {
            format: 4,
            ..base()
        })
        .unwrap();
        assert_ne!(srgb[4] & 0x00400000, 0);
    }
    #[test]
    fn h0_is_tiled_and_pitch_cannot_have_mips() {
        let t = texture(Texture {
            height: 1,
            tile_y_log2: Some(0),
            ..base()
        })
        .unwrap();
        assert_eq!(t[2] & 0xe00000, 0x600000);
        assert!(
            texture(Texture {
                tile_y_log2: None,
                ..base()
            })
            .is_err()
        );
        assert!(
            texture(Texture {
                layers: 5,
                dimension: Dimension::Cube,
                ..base()
            })
            .is_err()
        );
        assert!(
            texture(Texture {
                address: 1 << 48,
                ..base()
            })
            .is_err()
        );
    }
    #[test]
    fn sampler_lod_clamps_match_mesa() {
        let mut s = [0; 8];
        sampler_lod(&mut s, 7, true, 1.5, 6.0).unwrap();
        assert_eq!(s[1] & 0xc0, 0xc0);
        assert_eq!(s[2], (1536 << 12) | 384);
        assert!(sampler_lod(&mut s, 7, false, f32::NAN, 2.0).is_err());
    }
}

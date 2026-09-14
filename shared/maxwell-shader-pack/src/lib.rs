// SPDX-License-Identifier: GPL-2.0-only
//! Pinned Mesa Nouveau GM20B SASS, headers and fixed-program identities.
#![no_std]
pub const MESA_SHA: &str = "e881540692daac6532cefec76699f7a025563767";
pub const MESA_METADATA_SHA256: &str =
    "329c994c0e6101f149e66cc9b04f0d09a1b23a89fb082a6c387611a9ce825ffc";
pub const SHADER_ALIGNMENT: usize = 4096;
pub const SHADER_SIZE: usize = 4096;
pub const PACK_SIZE: usize = 13 * SHADER_ALIGNMENT;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ShaderVariant {
    VsStride16Pos2 = 0,
    VsStride16Pos2Uv2 = 1,
    VsStride40Pos4 = 2,
    VsStride40Pos4Color4 = 3,
    VsStride40Pos4Color4Uv2 = 4,
    FsSolid = 5,
    FsVertexColor = 6,
    FsTextureRgba = 7,
    FsTextureAlphaMask = 8,
    FsTextureVertexColorRgba = 9,
    VsStride24Pos4Uv2 = 10,
    VsStride28Pos4Color3 = 11,
    FsTextureRgbIgnoreAlpha = 12,
}
impl ShaderVariant {
    pub const ALL: [Self; 13] = [
        Self::VsStride16Pos2,
        Self::VsStride16Pos2Uv2,
        Self::VsStride40Pos4,
        Self::VsStride40Pos4Color4,
        Self::VsStride40Pos4Color4Uv2,
        Self::FsSolid,
        Self::FsVertexColor,
        Self::FsTextureRgba,
        Self::FsTextureAlphaMask,
        Self::FsTextureVertexColorRgba,
        Self::VsStride24Pos4Uv2,
        Self::VsStride28Pos4Color3,
        Self::FsTextureRgbIgnoreAlpha,
    ];
    pub const fn from_raw(raw: u16) -> Option<Self> {
        if raw < 13 {
            Some(Self::ALL[raw as usize])
        } else {
            None
        }
    }
    pub const fn raw(self) -> u16 {
        self as u16
    }
    pub const fn offset(self) -> usize {
        self as usize * SHADER_ALIGNMENT
    }
    pub const fn start(self) -> u32 {
        self.offset() as u32 + 0x30
    }
    pub const fn is_vertex(self) -> bool {
        matches!(
            self,
            Self::VsStride16Pos2
                | Self::VsStride16Pos2Uv2
                | Self::VsStride40Pos4
                | Self::VsStride40Pos4Color4
                | Self::VsStride40Pos4Color4Uv2
                | Self::VsStride24Pos4Uv2
                | Self::VsStride28Pos4Color3
        )
    }
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Self::VsStride16Pos2 => include_bytes!("../artifacts/gm20b/vs_stride16_pos2.bin"),
            Self::VsStride16Pos2Uv2 => {
                include_bytes!("../artifacts/gm20b/vs_stride16_pos2_uv2.bin")
            }
            Self::VsStride40Pos4 => include_bytes!("../artifacts/gm20b/vs_stride40_pos4.bin"),
            Self::VsStride40Pos4Color4 => {
                include_bytes!("../artifacts/gm20b/vs_stride40_pos4_color4.bin")
            }
            Self::VsStride40Pos4Color4Uv2 => {
                include_bytes!("../artifacts/gm20b/vs_stride40_pos4_color4_uv2.bin")
            }
            Self::FsSolid => include_bytes!("../artifacts/gm20b/fs_solid.bin"),
            Self::FsVertexColor => include_bytes!("../artifacts/gm20b/fs_vertex_color.bin"),
            Self::FsTextureRgba => include_bytes!("../artifacts/gm20b/fs_texture_rgba.bin"),
            Self::FsTextureAlphaMask => {
                include_bytes!("../artifacts/gm20b/fs_texture_alpha_mask.bin")
            }
            Self::FsTextureVertexColorRgba => {
                include_bytes!("../artifacts/gm20b/fs_texture_vertex_color_rgba.bin")
            }
            Self::VsStride24Pos4Uv2 => {
                include_bytes!("../artifacts/gm20b/vs_stride24_pos4_uv2.bin")
            }
            Self::VsStride28Pos4Color3 => {
                include_bytes!("../artifacts/gm20b/vs_stride28_pos4_color3.bin")
            }
            Self::FsTextureRgbIgnoreAlpha => {
                include_bytes!("../artifacts/gm20b/fs_texture_rgb_ignore_alpha.bin")
            }
        }
    }
    pub const fn header(self) -> &'static [u8; 80] {
        match self {
            Self::VsStride16Pos2 => {
                include_bytes!("../artifacts/gm20b/vs_stride16_pos2.header.bin")
            }
            Self::VsStride16Pos2Uv2 => {
                include_bytes!("../artifacts/gm20b/vs_stride16_pos2_uv2.header.bin")
            }
            Self::VsStride40Pos4 => {
                include_bytes!("../artifacts/gm20b/vs_stride40_pos4.header.bin")
            }
            Self::VsStride40Pos4Color4 => {
                include_bytes!("../artifacts/gm20b/vs_stride40_pos4_color4.header.bin")
            }
            Self::VsStride40Pos4Color4Uv2 => {
                include_bytes!("../artifacts/gm20b/vs_stride40_pos4_color4_uv2.header.bin")
            }
            Self::FsSolid => include_bytes!("../artifacts/gm20b/fs_solid.header.bin"),
            Self::FsVertexColor => include_bytes!("../artifacts/gm20b/fs_vertex_color.header.bin"),
            Self::FsTextureRgba => include_bytes!("../artifacts/gm20b/fs_texture_rgba.header.bin"),
            Self::FsTextureAlphaMask => {
                include_bytes!("../artifacts/gm20b/fs_texture_alpha_mask.header.bin")
            }
            Self::FsTextureVertexColorRgba => {
                include_bytes!("../artifacts/gm20b/fs_texture_vertex_color_rgba.header.bin")
            }
            Self::VsStride24Pos4Uv2 => {
                include_bytes!("../artifacts/gm20b/vs_stride24_pos4_uv2.header.bin")
            }
            Self::VsStride28Pos4Color3 => {
                include_bytes!("../artifacts/gm20b/vs_stride28_pos4_color3.header.bin")
            }
            Self::FsTextureRgbIgnoreAlpha => {
                include_bytes!("../artifacts/gm20b/fs_texture_rgb_ignore_alpha.header.bin")
            }
        }
    }
    pub const fn gprs(self) -> u32 {
        match self {
            Self::VsStride16Pos2 => 8,
            Self::VsStride16Pos2Uv2 => 8,
            Self::VsStride40Pos4 => 7,
            Self::VsStride40Pos4Color4 => 7,
            Self::VsStride40Pos4Color4Uv2 => 7,
            Self::FsSolid => 4,
            Self::FsVertexColor => 5,
            Self::FsTextureRgba => 4,
            Self::FsTextureAlphaMask => 4,
            Self::FsTextureVertexColorRgba => 8,
            Self::VsStride24Pos4Uv2 => 7,
            Self::VsStride28Pos4Color3 => 7,
            Self::FsTextureRgbIgnoreAlpha => 4,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum PipelineVariant {
    Stride16Solid = 0,
    Stride16TextureRgba = 1,
    Stride16TextureAlphaMask = 2,
    Stride40Solid = 3,
    Stride40VertexColor = 4,
    Stride40TextureVertexColorRgba = 5,
    Stride24Solid = 6,
    Stride24TextureRgba = 7,
    Stride24TextureRgbIgnoreAlpha = 8,
    Stride28VertexColor = 9,
    Stride32Solid = 10,
    Stride32VertexColor = 11,
    Stride24TextureAlphaMask = 12,
}
impl PipelineVariant {
    pub const ALL: [Self; 13] = [
        Self::Stride16Solid,
        Self::Stride16TextureRgba,
        Self::Stride16TextureAlphaMask,
        Self::Stride40Solid,
        Self::Stride40VertexColor,
        Self::Stride40TextureVertexColorRgba,
        Self::Stride24Solid,
        Self::Stride24TextureRgba,
        Self::Stride24TextureRgbIgnoreAlpha,
        Self::Stride28VertexColor,
        Self::Stride32Solid,
        Self::Stride32VertexColor,
        Self::Stride24TextureAlphaMask,
    ];
    pub const fn from_raw(raw: u32) -> Option<Self> {
        if raw < 13 {
            Some(Self::ALL[raw as usize])
        } else {
            None
        }
    }
    pub const fn shaders(self) -> (ShaderVariant, ShaderVariant) {
        const PAIRS: [(ShaderVariant, ShaderVariant); 13] = [
            (ShaderVariant::VsStride16Pos2, ShaderVariant::FsSolid),
            (
                ShaderVariant::VsStride16Pos2Uv2,
                ShaderVariant::FsTextureRgba,
            ),
            (
                ShaderVariant::VsStride16Pos2Uv2,
                ShaderVariant::FsTextureAlphaMask,
            ),
            (ShaderVariant::VsStride40Pos4, ShaderVariant::FsSolid),
            (
                ShaderVariant::VsStride40Pos4Color4,
                ShaderVariant::FsVertexColor,
            ),
            (
                ShaderVariant::VsStride40Pos4Color4Uv2,
                ShaderVariant::FsTextureVertexColorRgba,
            ),
            (ShaderVariant::VsStride40Pos4, ShaderVariant::FsSolid),
            (
                ShaderVariant::VsStride24Pos4Uv2,
                ShaderVariant::FsTextureRgba,
            ),
            (
                ShaderVariant::VsStride24Pos4Uv2,
                ShaderVariant::FsTextureRgbIgnoreAlpha,
            ),
            (
                ShaderVariant::VsStride28Pos4Color3,
                ShaderVariant::FsVertexColor,
            ),
            (ShaderVariant::VsStride40Pos4, ShaderVariant::FsSolid),
            (
                ShaderVariant::VsStride40Pos4Color4,
                ShaderVariant::FsVertexColor,
            ),
            (
                ShaderVariant::VsStride24Pos4Uv2,
                ShaderVariant::FsTextureAlphaMask,
            ),
        ];
        PAIRS[self as usize]
    }
    pub const fn stride(self) -> u32 {
        [16, 16, 16, 40, 40, 40, 24, 24, 24, 28, 32, 32, 24][self as usize]
    }
}
/// Materialize in zeroed kernel-owned memory, with SASS aligned to 0x80.
pub fn copy_pack(destination: &mut [u8]) -> Result<(), &'static str> {
    if destination.len() < PACK_SIZE {
        return Err("Maxwell shader pack backing too small");
    }
    destination[..PACK_SIZE].fill(0);
    for v in ShaderVariant::ALL {
        let base = v.offset();
        let code = v.bytes();
        destination[base + 0x30..base + 0x80].copy_from_slice(v.header());
        destination[base + 0x80..base + 0x80 + code.len()].copy_from_slice(code);
    }
    Ok(())
}

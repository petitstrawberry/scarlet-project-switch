// SPDX-License-Identifier: MIT
//! Selected method offsets from pinned Mesa Nouveau nvc0_3d.xml.h.
#![allow(dead_code)]

/// Native viewport registers produced only after portable-state validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeViewport {
    pub translate: [u32; 3],
    pub scale: [u32; 3],
    pub clip: [u32; 2],
    pub depth: [u32; 2],
}

/// Native blend registers derived from the small SGFX factor vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeBlend {
    pub enabled: u32,
    pub color_mask: u32,
    pub color: [u32; 3],
    pub alpha: [u32; 3],
}

/// Closed v2 blend/mask record. Native enums are sourced from Mesa
/// nvc0_blend_fac/nvc0_blend_eqn at e881540692daac6532cefec76699f7a025563767.
pub fn fixed_blend_record(words: &[u32], extended: bool) -> Result<NativeBlend, &'static str> {
    if !extended {
        return Err("fixed blend state requires GM20B v2 dialect");
    }
    if words.len() != 64
        || words[0] != 8
        || words[1..21]
            .iter()
            .chain(words[29..].iter())
            .any(|&word| word != 0)
        || words[21] > 15
        || words[22] > 1
        || [23, 24, 26, 27].iter().any(|&i| words[i] > 5)
        || words[25] > 2
        || words[28] > 2
    {
        return Err("fixed blend state invalid");
    }
    if words[22] == 0 && words[23..29] != [1, 0, 0, 1, 0, 0] {
        return Err("disabled fixed blend must use replacement factors");
    }
    const FACTOR: [u32; 6] = [0x4000, 0x4001, 0x4302, 0x4303, 0x4304, 0x4305];
    const EQUATION: [u32; 3] = [0x8006, 0x800a, 0x800b];
    let mask = words[21];
    Ok(NativeBlend {
        enabled: words[22],
        color_mask: (mask & 1) | ((mask & 2) << 3) | ((mask & 4) << 6) | ((mask & 8) << 9),
        color: [
            EQUATION[words[25] as usize],
            FACTOR[words[23] as usize],
            FACTOR[words[24] as usize],
        ],
        alpha: [
            EQUATION[words[28] as usize],
            FACTOR[words[26] as usize],
            FACTOR[words[27] as usize],
        ],
    })
}

pub const fn fixed_legacy_blend(source_over: bool) -> NativeBlend {
    NativeBlend {
        enabled: source_over as u32,
        color_mask: 0x1111,
        color: if source_over {
            [0x8006, 0x4302, 0x4303]
        } else {
            [0x8006, 0x4001, 0x4000]
        },
        alpha: if source_over {
            [0x8006, 0x4001, 0x4303]
        } else {
            [0x8006, 0x4001, 0x4000]
        },
    }
}

fn viewport_components_valid(v: [f32; 6]) -> bool {
    v.iter().all(|value| value.is_finite())
        && v[0] >= 0.0
        && v[1] >= 0.0
        && v[2] > 0.0
        && (v[0] + v[2]).is_finite()
        && (v[1] + v[3]).is_finite()
        && (0.0..=1.0).contains(&v[4])
        && (0.0..=1.0).contains(&v[5])
}

/// Closed v2 viewport record. It contains no address fields or GPU methods.
pub fn fixed_viewport_record(words: &[u32], extended: bool) -> Result<[f32; 6], &'static str> {
    if !extended {
        return Err("fixed viewport requires GM20B v2 dialect");
    }
    if words.len() != 64
        || words[0] != 7
        || words[1..32]
            .iter()
            .chain(words[38..].iter())
            .any(|&word| word != 0)
    {
        return Err("fixed viewport reserved words nonzero");
    }
    let viewport = core::array::from_fn(|i| f32::from_bits(words[32 + i]));
    if !viewport_components_valid(viewport) {
        return Err("fixed viewport components invalid");
    }
    Ok(viewport)
}

/// Submission-local grammar: each viewport applies to exactly one adjacent
/// fixed draw. Programmable operations and new submissions start independently.
pub struct FixedViewportState {
    pending: Option<[f32; 6]>,
    blend_pending: bool,
}

impl FixedViewportState {
    pub const fn new() -> Self {
        Self {
            pending: None,
            blend_pending: false,
        }
    }

    pub fn validate_operation(
        &mut self,
        words: &[u32],
        extended: bool,
    ) -> Result<(), &'static str> {
        if words.len() != 64 {
            return Err("fixed viewport operation size invalid");
        }
        if self.pending.is_some() && words[0] != 2 {
            return Err("fixed viewport must immediately precede a fixed draw");
        }
        if self.blend_pending && !matches!(words[0], 7 | 2) {
            return Err("fixed blend state must precede viewport or fixed draw");
        }
        match words[0] {
            8 => {
                fixed_blend_record(words, extended)?;
                self.blend_pending = true;
            }
            7 => self.pending = Some(fixed_viewport_record(words, extended)?),
            2 => {
                if let Some(viewport) = self.pending.take() {
                    fixed_viewport_native(viewport, words[10], words[11])
                        .ok_or("fixed viewport exceeds draw target")?;
                }
                self.blend_pending = false;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<(), &'static str> {
        if self.pending.is_some() || self.blend_pending {
            Err("fixed state missing following draw")
        } else {
            Ok(())
        }
    }
}

/// SGFX uses an upper-left framebuffer with clip-space depth -1..1. Preserve
/// the signed height and depth scale; sort only the hardware clipping limits.
/// Rectangle rounding and depth clamps follow Mesa nvc0_validate_viewport at
/// e881540692daac6532cefec76699f7a025563767, including inverted viewports.
pub fn fixed_viewport_native(v: [f32; 6], width: u32, height: u32) -> Option<NativeViewport> {
    if !viewport_components_valid(v)
        || width == 0
        || height == 0
        || width > 0xffff
        || height > 0xffff
        || v[0] + v[2] > width as f32
        || v[1].min(v[1] + v[3]) < 0.0
        || v[1].max(v[1] + v[3]) > height as f32
    {
        return None;
    }
    let sx = v[2] * 0.5;
    let sy = v[3] * 0.5;
    let sz = (v[5] - v[4]) * 0.5;
    // All endpoints are nonnegative and bounded to u16, so adding one half
    // before truncation implements Mesa's util_iround without libm.
    let x0 = (v[0] + 0.5) as u32;
    let x1 = (v[0] + v[2] + 0.5) as u32;
    let y0 = (v[1].min(v[1] + v[3]) + 0.5) as u32;
    let y1 = (v[1].max(v[1] + v[3]) + 0.5) as u32;
    Some(NativeViewport {
        translate: [
            (v[0] + sx).to_bits(),
            (v[1] + sy).to_bits(),
            (v[4] + sz).to_bits(),
        ],
        scale: [sx.to_bits(), (-sy).to_bits(), sz.to_bits()],
        clip: [((x1 - x0) << 16) | x0, ((y1 - y0) << 16) | y0],
        depth: [v[4].min(v[5]).to_bits(), v[4].max(v[5]).to_bits()],
    })
}

/// Allowed portable state bits in a canonical SGFX draw record.
/// Bit 0 is blend, bits 2..4 are cull/front-face and bit 5 is alpha-mask.
/// Bit 1 selects linear minification, bit 6 selects a different magnification
/// filter, and bits 7..8 / 9..10 encode ClampToEdge=0, Repeat=1, MirrorRepeat=2.
/// The differential filter bit preserves existing records with equal filters.
pub const DRAW_STATE_MASK: u32 = 0xfff;
pub const DRAW_SAMPLER_MASK: u32 = (1 << 1) | (1 << 6) | (3 << 7) | (3 << 9) | (1 << 11);

pub const fn draw_state_valid(flags: u32) -> bool {
    flags & !DRAW_STATE_MASK == 0
        && (flags >> 2) & 3 != 3
        && (flags >> 7) & 3 != 3
        && (flags >> 9) & 3 != 3
}

const fn sampler_wrap(mode: u32) -> u32 {
    match mode {
        0 => 2,
        1 => 0,
        _ => 1,
    }
}

/// Construct a bounded sampler descriptor from portable SGFX draw state.
///
/// Mesa nv50_state.c / g80_texture.xml.h at e881540692daac6532cefec76699f7a025563767:
/// TSC wrap fields start at bits 0/3/6, Repeat=0, Mirror=1, ClampToEdge=2;
/// mag/min filters occupy bits 0/4 of word 1, nearest=1 and linear=2.
/// Mip filtering remains disabled because the fixed image view has one level.
pub const fn draw_sampler_descriptor(flags: u32) -> Option<[u32; 8]> {
    if !draw_state_valid(flags) {
        return None;
    }
    let min_linear = flags & (1 << 1) != 0;
    let mag_linear = min_linear != (flags & (1 << 6) != 0);
    Some([
        0x26000 | sampler_wrap((flags >> 7) & 3) | (sampler_wrap((flags >> 9) & 3) << 3) | (2 << 6),
        0x40 | (if min_linear { 2 } else { 1 } << 4) | if mag_linear { 2 } else { 1 },
        0,
        0,
        0,
        0,
        0,
        0,
    ])
}

/// Resolve an index and signed base without allowing it outside the authorized
/// vertex relocation. Call only with index bytes from the immutable snapshot.
pub fn indexed_vertex_in_bounds(index: u32, base: i32, stride: u32, span: u64) -> bool {
    let resolved = i64::from(index) + i64::from(base);
    u64::try_from(resolved)
        .ok()
        .and_then(|value| value.checked_add(1))
        .and_then(|count| count.checked_mul(u64::from(stride)))
        .is_some_and(|bytes| stride != 0 && bytes <= span)
}

pub const COND_MODE: u32 = 0x1558;
pub const WATCHDOG_TIMER: u32 = 0x0de4;
pub const ZETA_COMP_ENABLE: u32 = 0x19cc;
pub const RT_CONTROL: u32 = 0x121c;
pub const CSAA_ENABLE: u32 = 0x15b4;
pub const MULTISAMPLE_ENABLE: u32 = 0x1534;
pub const MULTISAMPLE_MODE: u32 = 0x15d0;
pub const MULTISAMPLE_CTRL: u32 = 0x153c;
pub const BLEND_SEPARATE_ALPHA: u32 = 0x133c;
pub const BLEND_ENABLE_COMMON: u32 = 0x135c;
pub const SHADE_MODEL: u32 = 0x12d4;
pub const CALL_LIMIT_LOG: u32 = 0x0d64;
pub const CACHE_SPLIT: u32 = 0x0308;
pub const CODE_ADDRESS_HIGH: u32 = 0x1608;
pub const VERTEX_RUNOUT_ADDRESS_HIGH: u32 = 0x0f84;
pub const LOCAL_BASE: u32 = 0x077c;
pub const TIC_ADDRESS_HIGH: u32 = 0x1574;
pub const TSC_ADDRESS_HIGH: u32 = 0x155c;
pub const SCREEN_Y_CONTROL: u32 = 0x13ac;
pub const WINDOW_OFFSET_X: u32 = 0x0df8;
pub const ZCULL_REGION: u32 = 0x1590;
pub const CLIP_RECTS_EN: u32 = 0x194c;
pub const CLIPID_ENABLE: u32 = 0x197c;
pub const CLEAR_FLAGS: u32 = 0x10f8;
pub const VIEWPORT_TRANSFORM_EN: u32 = 0x192c;
pub const VIEW_VOLUME_CLIP_CTRL: u32 = 0x193c;
pub const RASTERIZE_ENABLE: u32 = 0x037c;
pub const RT_SEPARATE_FRAG_DATA: u32 = 0x0fac;
pub const LAYER: u32 = 0x15cc;
pub const POINT_COORD_REPLACE: u32 = 0x1604;
pub const POINT_RASTER_RULES: u32 = 0x165c;
pub const EDGEFLAG: u32 = 0x15e4;
pub const RT_ADDRESS_HIGH: u32 = 0x0800;
pub const SCREEN_SCISSOR_HORIZ: u32 = 0x0ff4;
pub const CLEAR_COLOR: u32 = 0x0d80;
pub const ZETA_ENABLE: u32 = 0x1538;
pub const CLEAR_DEPTH: u32 = 0x0d90;
pub const ZETA_ADDRESS_HIGH: u32 = 0x0fe0;
pub const ZETA_HORIZ: u32 = 0x1228;
pub const ZETA_BASE_LAYER: u32 = 0x179c;
pub const DEPTH_TEST_FUNC: u32 = 0x130c;
pub const CLEAR_BUFFERS: u32 = 0x19d0;
pub const SCISSOR_ENABLE: u32 = 0x0e00;
pub const VIEWPORT_TRANSLATE_X: u32 = 0x0a0c;
pub const VIEWPORT_SCALE_X: u32 = 0x0a00;
pub const VIEWPORT_HORIZ: u32 = 0x0c00;
pub const VIEWPORT_SWIZZLE: u32 = 0x0a18;
pub const DEPTH_RANGE_NEAR: u32 = 0x0c08;
pub const CULL_FACE_ENABLE: u32 = 0x1918;
pub const CULL_FACE: u32 = 0x1920;
pub const FRONT_FACE: u32 = 0x191c;
pub const POLYGON_MODE_FRONT: u32 = 0x0dac;
pub const POLYGON_MODE_BACK: u32 = 0x0db0;
pub const VERTEX_ARRAY_FETCH: u32 = 0x1c00;
pub const VERTEX_ARRAY_LIMIT_HIGH: u32 = 0x1f00;
pub const VERTEX_ATTRIB_FORMAT: u32 = 0x1160;
pub const VERTEX_ARRAY_PER_INSTANCE: u32 = 0x1880;
pub const VERTEX_BEGIN_GL: u32 = 0x1618;
pub const VERTEX_BUFFER_FIRST: u32 = 0x0d74;
pub const VERTEX_END_GL: u32 = 0x1614;
pub const VB_ELEMENT_BASE: u32 = 0x1434;
pub const INDEX_ARRAY_START_HIGH: u32 = 0x17c8;
pub const INDEX_BATCH_FIRST: u32 = 0x17dc;
pub const SP_SELECT: u32 = 0x2000;
pub const SP_START_ID: u32 = 0x2004;
pub const SP_GPR_ALLOC: u32 = 0x200c;
pub const CB_SIZE: u32 = 0x2380;
pub const CB_POS: u32 = 0x238c;
pub const CB_BIND: u32 = 0x2410;
pub const TEX_CACHE_CTL: u32 = 0x1338;
pub const TIC_FLUSH: u32 = 0x1334;
pub const TSC_FLUSH: u32 = 0x1330;
pub const QUERY_ADDRESS_HIGH: u32 = 0x1b00;
pub const RT_COMP_ENABLE: u32 = 0x19e0;
pub const COLOR_MASK: u32 = 0x1a00;
pub const BLEND_ENABLE: u32 = 0x1360;
pub const BLEND_EQUATION_RGB: u32 = 0x1340;
pub const BLEND_FUNC_SRC_RGB: u32 = 0x1344;
pub const BLEND_FUNC_DST_RGB: u32 = 0x1348;
pub const BLEND_EQUATION_ALPHA: u32 = 0x134c;
pub const BLEND_FUNC_SRC_ALPHA: u32 = 0x1350;
pub const BLEND_FUNC_DST_ALPHA: u32 = 0x1358;
pub const DEPTH_TEST_ENABLE: u32 = 0x12cc;
pub const DEPTH_WRITE_ENABLE: u32 = 0x12e8;
pub const ALPHA_TEST_ENABLE: u32 = 0x12ec;
pub const STENCIL_ENABLE: u32 = 0x1380;
pub const LOGIC_OP_ENABLE: u32 = 0x19c4;
pub const VERTEX_ARRAY_DIVISOR: u32 = 0x1c0c;
pub const VERTEX_ID_GEN_MODE: u32 = 0x164c;
pub const RT_TILE_MODE_LINEAR: u32 = 0x00001000;
pub const VERTEX_ARRAY_FETCH_ENABLE: u32 = 0x00001000;
pub const VERTEX_ATTRIB_FORMAT_TYPE_FLOAT: u32 = 0x38000000;
pub const VERTEX_ATTRIB_FORMAT_SIZE_32_32: u32 = 0x00800000;
pub const VERTEX_ATTRIB_FORMAT_SIZE_32_32_32: u32 = 0x00400000;
pub const VERTEX_ATTRIB_FORMAT_SIZE_32_32_32_32: u32 = 0x00200000;
pub const VERTEX_ATTRIB_FORMAT_CONST: u32 = 0x00000040;
pub const QUERY_GET_FENCE: u32 = 0x00000010;
pub const QUERY_GET_SHORT: u32 = 0x10000000;
pub const COND_MODE_ALWAYS: u32 = 0x00000001;
pub const SHADE_MODEL_SMOOTH: u32 = 0x00001d01;
pub const CACHE_SPLIT_48K_SHARED_16K_L1: u32 = 0x00000003;
pub const VIEW_VOLUME_CLIP_CTRL_UNK1_UNK1: u32 = 0x00000002;
pub const SERIALIZE: u32 = 0x0110; // nv_object.xml.h NV50_GRAPH_SERIALIZE
pub const TEX_CB_INDEX: u32 = 0x2608; // NVE4_3D_TEX_CB_INDEX
// NVIDIA clb197.h: instruction bit 0, data bit 4, constant bit 12.
// Bit 8 from older MEM_BARRIER templates is not defined for Maxwell B.
pub const INVALIDATE_SHADER_CACHES: u32 = 0x021c;
pub const INVALIDATE_SHADER_CACHE_READS: u32 = (1 << 0) | (1 << 4) | (1 << 12);

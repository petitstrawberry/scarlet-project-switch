// SPDX-License-Identifier: GPL-2.0-only

/// Normalized coordinates for validated NV12 metadata. Texture widths may
/// include row padding; bounds cover only texels belonging to the visible crop.
pub(crate) fn sampling(
    extent: [u32; 4],
    visible: [u32; 4],
    cosited: [bool; 2],
) -> ([f32; 8], [f32; 4]) {
    let [w, h, cw, ch] = extent.map(|v| v as f32);
    let [x, y, width, height] = visible;
    let transforms = [
        width as f32 / w,
        height as f32 / h,
        x as f32 / w,
        y as f32 / h,
        width as f32 / (2.0 * cw),
        height as f32 / (2.0 * ch),
        (x as f32 + if cosited[0] { 0.5 } else { 0.0 }) / (2.0 * cw),
        (y as f32 + if cosited[1] { 0.5 } else { 0.0 }) / (2.0 * ch),
    ];
    let chroma_bounds = [
        ((x / 2) as f32 + 0.5) / cw,
        ((y / 2) as f32 + 0.5) / ch,
        ((x + width).div_ceil(2) as f32 - 0.5) / cw,
        ((y + height).div_ceil(2) as f32 - 0.5) / ch,
    ];
    (transforms, chroma_bounds)
}

pub(crate) fn luma_bounds(transform: [f32; 4], extent: [u32; 2]) -> [f32; 4] {
    let [sx, sy, ox, oy] = transform;
    let hx = 0.5 / extent[0] as f32;
    let hy = 0.5 / extent[1] as f32;
    let min_x = ox + hx;
    let min_y = oy + hy;
    // A one-pixel crop has equal bounds; transform rounding can invert them.
    [
        min_x,
        min_y,
        (ox + sx - hx).max(min_x),
        (oy + sy - hy).max(min_y),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reconstruct a plane with constant visible pixels and poisoned padding.
    fn filtered(coord: f32, extent: u32, first: u32, end: u32) -> f32 {
        let pos = coord * extent as f32 - 0.5;
        let left = pos as i32 - i32::from(pos < 0.0);
        let fraction = pos - left as f32;
        let pixel = |x: i32| {
            if x >= first as i32 && x < end as i32 {
                1.0
            } else {
                0.0
            }
        };
        pixel(left) * (1.0 - fraction) + pixel(left + 1) * fraction
    }

    #[test]
    fn right_edge_previously_blended_with_nvdec_padding() {
        let (t, bounds) = sampling([2048, 1088, 1024, 544], [0, 0, 1920, 1080], [false; 2]);
        let u = (1919.0 + 0.5) / 1920.0;
        let coord = u * t[4] + t[6];
        // The final luma pixel mixes 25% of an invalid chroma texel.
        assert!((filtered(coord, 1024, 0, 960) - 0.75).abs() < 0.001);
        assert_eq!(
            filtered(coord.clamp(bounds[0], bounds[2]), 1024, 0, 960),
            1.0
        );
    }

    #[test]
    fn both_planes_stay_inside_visible_texels_at_every_edge() {
        for (extent, crop) in [
            ([2048, 1088, 1024, 544], [0, 0, 1920, 1080]),
            ([256, 96, 128, 48], [0, 0, 160, 90]),
            ([64, 32, 32, 16], [16, 16, 16, 16]),
            ([37, 29, 19, 15], [3, 5, 31, 21]),
            ([64, 32, 32, 16], [17, 9, 1, 1]),
            ([3, 3, 2, 2], [2, 2, 1, 1]),
        ] {
            for cosited in [[false, false], [true, false], [false, true], [true, true]] {
                let (t, cb) = sampling(extent, crop, cosited);
                let yb = luma_bounds(t[..4].try_into().unwrap(), [extent[0], extent[1]]);
                for output in [1, 16, 720, 1280, 3840] {
                    for uv in [0.0, 0.5 / output as f32, 1.0 - 0.5 / output as f32, 1.0] {
                        for axis in 0..2 {
                            let y = (uv * t[axis] + t[axis + 2]).clamp(yb[axis], yb[axis + 2]);
                            let c = (uv * t[axis + 4] + t[axis + 6]).clamp(cb[axis], cb[axis + 2]);
                            let start = crop[axis];
                            let end = start + crop[axis + 2];
                            assert!((filtered(y, extent[axis], start, end) - 1.0).abs() < 0.001);
                            assert!(
                                (filtered(c, extent[axis + 2], start / 2, end.div_ceil(2)) - 1.0)
                                    .abs()
                                    < 0.001
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn interior_and_chroma_siting_are_preserved() {
        let (mid, bounds) = sampling([64, 32, 32, 16], [16, 8, 16, 16], [false; 2]);
        let (co, co_bounds) = sampling([64, 32, 32, 16], [16, 8, 16, 16], [true; 2]);
        assert_eq!(bounds, co_bounds);
        for axis in 0..2 {
            let coord = 0.5 * mid[axis + 4] + mid[axis + 6];
            assert_eq!(coord.clamp(bounds[axis], bounds[axis + 2]), coord);
            assert_eq!(co[axis + 6] - mid[axis + 6], 0.25 / [32.0, 16.0][axis]);
        }
    }
}

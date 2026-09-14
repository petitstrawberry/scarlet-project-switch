// SPDX-License-Identifier: GPL-2.0-only
//! Uncompressed Tegra 16Bx2 storage. Linux drm_fourcc.h documents the GOBs;
//! Chromium minigbm bc4f023bfcc51cf9dcfcfec5bf4177b2e607dd68/tegra.c supplies
//! the independent pixel-order reference. Image coordinates are unchanged.

pub const WIDTH: usize = 1280;
pub const HEIGHT: usize = 720;
pub const STRIDE: usize = WIDTH * 4;
pub const BLOCK_HEIGHT_LOG2: u32 = 4;
const GOB_WIDTH: usize = 64;
const GOB_HEIGHT: usize = 8;
const GOB_SIZE: usize = GOB_WIDTH * GOB_HEIGHT;
const GOBS_PER_BLOCK: usize = 1 << BLOCK_HEIGHT_LOG2;
const BLOCK_ROWS: usize = GOB_HEIGHT * GOBS_PER_BLOCK;
const GOBS_PER_ROW: usize = STRIDE / GOB_WIDTH;
pub const SIZE: usize = STRIDE * HEIGHT.div_ceil(BLOCK_ROWS) * BLOCK_ROWS;
// DC surface kind is not the DRM modifier or the GPU page kind. The NVIDIA
// DC register selects BL_16B2 in bit 1 and block-height log2 in bits 7:4.
pub const SURFACE_KIND: u32 = 2 | (BLOCK_HEIGHT_LOG2 << 4);

pub fn pixel_offset(x: usize, y: usize) -> usize {
    let x_byte = x * 4;
    (y / BLOCK_ROWS) * STRIDE * BLOCK_ROWS
        + (x_byte / GOB_WIDTH) * GOB_SIZE * GOBS_PER_BLOCK
        + ((y % BLOCK_ROWS) / GOB_HEIGHT) * GOB_SIZE
        + ((x_byte % GOB_WIDTH) / 32) * 256
        + ((y % GOB_HEIGHT) / 2) * 64
        + ((x_byte % 32) / 16) * 32
        + (y % 2) * 16
        + x_byte % 16
}

/// Upload an ordinary packed image into inactive block-linear storage.
/// Source padding is skipped; the visible pixels retain their byte order.
/// Destination writes proceed in contiguous 16-byte sectors. Padding was
/// zeroed at allocation and is not fetched as part of the visible image.
pub fn upload(
    source: &[u8],
    source_stride: usize,
    destination: &mut [u8],
) -> Result<(), &'static str> {
    let required = source_stride
        .checked_mul(HEIGHT)
        .ok_or("Tegra block-linear source size overflow")?;
    if source_stride < STRIDE
        || source_stride & 63 != 0
        || source.len() < required
        || destination.len() < SIZE
    {
        return Err("Tegra block-linear upload buffer layout mismatch");
    }
    for block_y in 0..HEIGHT.div_ceil(BLOCK_ROWS) {
        for gob_x in 0..GOBS_PER_ROW {
            for gob_y in 0..GOBS_PER_BLOCK {
                let top = block_y * BLOCK_ROWS + gob_y * GOB_HEIGHT;
                if top >= HEIGHT {
                    break;
                }
                let base = ((block_y * GOBS_PER_ROW + gob_x) * GOBS_PER_BLOCK + gob_y) * GOB_SIZE;
                for half in 0..2 {
                    for pair in 0..4 {
                        for sector in 0..2 {
                            for row in 0..2 {
                                let y = top + pair * 2 + row;
                                if y >= HEIGHT {
                                    continue;
                                }
                                let x_byte = gob_x * GOB_WIDTH + half * 32 + sector * 16;
                                let input = y * source_stride + x_byte;
                                let output = base + half * 256 + pair * 64 + sector * 32 + row * 16;
                                destination[output..output + 16]
                                    .copy_from_slice(&source[input..input + 16]);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

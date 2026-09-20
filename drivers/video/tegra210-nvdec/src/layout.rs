// SPDX-License-Identifier: GPL-2.0-only
//! NVDEC2 TBL surfaces use two 64x8-byte GOBs per block (gob_height=0).

pub fn align(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

pub fn offset(x: usize, y: usize, pitch: usize) -> usize {
    ((y / 16) * (pitch / 64) + x / 64) * 1024
        + (y % 16 / 8) * 512
        + (x % 64 / 32) * 256
        + (y % 8 / 2) * 64
        + (x % 32 / 16) * 32
        + (y % 2) * 16
        + x % 16
}

/// Copy one visible plane from an NVDEC TBL surface. A 64-byte sector holds
/// 32 pixels in each of two rows, interleaved in 16-byte spans. Read the whole
/// aligned sector with integer loads instead of calling memcpy for every span
/// of noncacheable DMA memory. Cropped/unaligned edges retain the generic path.
pub fn linearize_plane(
    source: &[u8],
    output: &mut [u8],
    pitch: usize,
    width: usize,
    height: usize,
    crop_x: usize,
    crop_y: usize,
) {
    let size = width
        .checked_mul(height)
        .expect("NVDEC plane size overflow");
    let output = &mut output[..size];
    if width % 32 == 0
        && height % 2 == 0
        && crop_x % 32 == 0
        && crop_y % 2 == 0
        && (source.as_ptr() as usize | output.as_ptr() as usize) & 7 == 0
    {
        for y in (0..height).step_by(2) {
            for x in (0..width).step_by(32) {
                let at = offset(x + crop_x, y + crop_y, pitch);
                let sector = &source[at..at + 64];
                // SAFETY: slice bounds cover all eight words; TBL sectors are
                // 64-byte aligned and both slice bases are 8-byte aligned.
                // The two complete output rows are inside the checked slice.
                // Integer loads/stores avoid using kernel SIMD/FPU state.
                unsafe {
                    let words = sector.as_ptr().cast::<[u64; 8]>().read();
                    let row0 = output.as_mut_ptr().add(y * width + x).cast::<u64>();
                    let row1 = row0.add(width / 8);
                    row0.write(words[0]);
                    row0.add(1).write(words[1]);
                    row1.write(words[2]);
                    row1.add(1).write(words[3]);
                    row0.add(2).write(words[4]);
                    row0.add(3).write(words[5]);
                    row1.add(2).write(words[6]);
                    row1.add(3).write(words[7]);
                }
            }
        }
    } else {
        for y in 0..height {
            let mut x = 0;
            while x < width {
                let source_x = x + crop_x;
                let count = (16 - source_x % 16).min(width - x);
                let at = offset(source_x, y + crop_y, pitch);
                let dest = y * width + x;
                output[dest..dest + count].copy_from_slice(&source[at..at + count]);
                x += count;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    #[test]
    fn tbl_sectors_cover_padded_surface_once() {
        let mut seen = [false; 192 * 48];
        for y in 0..48 {
            for x in 0..192 {
                let i = offset(x, y, 192);
                assert!(!seen[i]);
                seen[i] = true;
            }
        }
        assert!(seen.into_iter().all(|x| x));
        assert_eq!(offset(64, 0, 192), 1024);
        assert_eq!(offset(0, 16, 192), 3072);
        assert_eq!(align(33, 16), 48);
    }

    #[test]
    fn linear_planes_match_byte_addressing_with_crop_and_unaligned_buffers() {
        for (width, height, crop_x, crop_y) in [
            (1920, 1080, 0, 0),
            (160, 45, 0, 0),
            (146, 77, 2, 3),
            (320, 180, 32, 2),
        ] {
            let pitch = align(width + crop_x, 256);
            let size = pitch * align(height + crop_y, 16);
            for shift in [0, 1] {
                let storage: Vec<u8> = (0..size + shift)
                    .map(|i| (i.wrapping_mul(73) ^ (i >> 8)) as u8)
                    .collect();
                let source = &storage[shift..];
                let mut result = vec![0xa5; width * height + shift + 8];
                linearize_plane(
                    source,
                    &mut result[shift..],
                    pitch,
                    width,
                    height,
                    crop_x,
                    crop_y,
                );
                for y in 0..height {
                    for x in 0..width {
                        assert_eq!(
                            result[shift + y * width + x],
                            source[offset(x + crop_x, y + crop_y, pitch)]
                        );
                    }
                }
                assert!(result[..shift].iter().all(|&v| v == 0xa5));
                assert!(result[shift + width * height..].iter().all(|&v| v == 0xa5));
            }
        }
    }
}

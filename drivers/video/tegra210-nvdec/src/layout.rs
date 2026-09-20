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

#[cfg(test)]
mod tests {
    use super::*;
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
}

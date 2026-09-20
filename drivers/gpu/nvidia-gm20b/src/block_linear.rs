// SPDX-License-Identifier: GPL-2.0-only
//! CPU staging for uncompressed kind 0xfe, GOB64x8, block-height log2 4.
//! Same GOB ordering as the Tegra DC scanout path. Depth kind 0x7b is not
//! CPU-accessible through this color conversion.

pub fn byte_offset(x_byte: usize, y: usize, pitch: usize) -> usize {
    let block = ((y / 128) * (pitch / 64) + x_byte / 64) * 8192;
    block
        + ((y % 128) / 8) * 512
        + ((x_byte % 64) / 32) * 256
        + ((y % 8) / 2) * 64
        + ((x_byte % 32) / 16) * 32
        + (y % 2) * 16
        + x_byte % 16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gob_sectors_and_block_boundaries() {
        assert_eq!(byte_offset(0, 0, 128), 0);
        assert_eq!(byte_offset(16, 0, 128), 32);
        assert_eq!(byte_offset(32, 0, 128), 256);
        assert_eq!(byte_offset(0, 1, 128), 16);
        assert_eq!(byte_offset(0, 2, 128), 64);
        assert_eq!(byte_offset(0, 8, 128), 512);
        assert_eq!(byte_offset(64, 0, 128), 8192);
        assert_eq!(byte_offset(0, 128, 128), 16384);
    }

    #[test]
    fn every_byte_is_unique_across_multiple_blocks() {
        let mut seen = [false; 3 * 64 * 256];
        for y in 0..256 {
            for x in 0..192 {
                let offset = byte_offset(x, y, 192);
                assert!(!seen[offset]);
                seen[offset] = true;
            }
        }
        assert!(seen.into_iter().all(|value| value));
    }
}

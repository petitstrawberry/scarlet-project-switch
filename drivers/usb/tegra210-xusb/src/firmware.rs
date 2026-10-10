// SPDX-License-Identifier: GPL-2.0-only
//! Checked, little-endian decoding of NVIDIA's 256-byte XUSB config table.

pub const HEADER_SIZE: usize = 256;
pub const IMEM_BLOCK_SIZE: u32 = 256;
pub const IMAGE: &[u8] = include_bytes!("../firmware/xusb.bin");

/// Metadata used by the Tegra210 ROM boot path. Offsets are relative to the
/// DFI payload immediately following the config table, not the full file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Firmware {
    pub image_len: usize,
    pub boot_tag: u32,
    pub boot_size: u32,
    pub code_tag_blocks: u32,
    pub code_size_blocks: u32,
    pub code_end_blocks: u32,
    pub created_time: u32,
    pub version: u32,
}

impl Firmware {
    pub fn parse(image: &[u8]) -> Result<Self, &'static str> {
        if image.len() < HEADER_SIZE {
            return Err("XUSB firmware config table truncated");
        }
        if &image[104..112] != b"XUSBFW\0\0" {
            return Err("XUSB firmware magic invalid");
        }
        let word =
            |offset: usize| u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap());
        let image_len = word(100) as usize;
        if image_len <= HEADER_SIZE || image_len > image.len() {
            return Err("XUSB firmware image length invalid");
        }
        let boot_tag = word(8);
        let boot_size = word(12);
        if boot_tag & (IMEM_BLOCK_SIZE - 1) != 0 {
            return Err("XUSB firmware boot tag is not block aligned");
        }
        if boot_size == 0 {
            return Err("XUSB firmware boot code empty");
        }
        let code_tag_blocks = blocks(boot_tag)?;
        let code_size_blocks = blocks(boot_size)?;
        // The hardware fields are 10-bit source offset and 8-bit count. Do
        // not silently mask a malformed image as the Linux loader does.
        if code_tag_blocks > 0x3ff || code_size_blocks > 0xff {
            return Err("XUSB firmware boot code exceeds L2IMEM fields");
        }
        let code_end_blocks = code_tag_blocks
            .checked_add(code_size_blocks)
            .ok_or("XUSB firmware boot block range overflow")?;
        let fetch_end = code_end_blocks
            .checked_mul(IMEM_BLOCK_SIZE)
            .and_then(|end| end.checked_add(HEADER_SIZE as u32))
            .ok_or("XUSB firmware boot fetch range overflow")?;
        // The ROM rounds both fields up independently. Validate the entire
        // DMA block range, including padding fetched beyond boot_size.
        if fetch_end as usize > image_len {
            return Err("XUSB firmware boot fetch outside image");
        }
        Ok(Self {
            image_len,
            boot_tag,
            boot_size,
            code_tag_blocks,
            code_size_blocks,
            code_end_blocks,
            created_time: word(44),
            version: word(72),
        })
    }

    pub const fn l2imem_size(self) -> u32 {
        (self.code_tag_blocks << 8) | (self.code_size_blocks << 24)
    }

    pub const fn autofill_range(self) -> u32 {
        self.code_tag_blocks | (self.code_end_blocks << 16)
    }
}

fn blocks(bytes: u32) -> Result<u32, &'static str> {
    bytes
        .checked_add(IMEM_BLOCK_SIZE - 1)
        .map(|rounded| rounded / IMEM_BLOCK_SIZE)
        .ok_or("XUSB firmware boot block rounding overflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

    fn image(len: usize, tag: u32, size: u32) -> Vec<u8> {
        let mut bytes = vec![0; len];
        bytes[8..12].copy_from_slice(&tag.to_le_bytes());
        bytes[12..16].copy_from_slice(&size.to_le_bytes());
        bytes[44..48].copy_from_slice(&0x12345678_u32.to_le_bytes());
        bytes[72..76].copy_from_slice(&0x50290003_u32.to_le_bytes());
        bytes[100..104].copy_from_slice(&(len as u32).to_le_bytes());
        bytes[104..112].copy_from_slice(b"XUSBFW\0\0");
        bytes
    }

    #[test]
    fn parses_unaligned_slice_and_little_endian_header() {
        let bytes = image(1024, 256, 257);
        let mut unaligned = vec![0];
        unaligned.extend_from_slice(&bytes);
        let metadata = Firmware::parse(&unaligned[1..]).unwrap();
        assert_eq!(metadata.image_len, 1024);
        assert_eq!(metadata.created_time, 0x12345678);
        assert_eq!(metadata.version, 0x50290003);
        assert_eq!(metadata.code_tag_blocks, 1);
        assert_eq!(metadata.code_size_blocks, 2);
        assert_eq!(metadata.code_end_blocks, 3);
        assert_eq!(metadata.l2imem_size(), (1 << 8) | (2 << 24));
        assert_eq!(metadata.autofill_range(), 1 | (3 << 16));
    }

    #[test]
    fn rejects_every_truncated_header() {
        let bytes = image(1024, 256, 256);
        for length in 0..HEADER_SIZE {
            assert!(Firmware::parse(&bytes[..length]).is_err());
        }
    }

    #[test]
    fn checks_declared_image_length_before_fetch() {
        let mut bytes = image(1024, 0, 256);
        for length in [0_u32, 255, 256, 1025, u32::MAX] {
            bytes[100..104].copy_from_slice(&length.to_le_bytes());
            assert!(Firmware::parse(&bytes).is_err());
        }
        // Trailing packaging bytes are ignored, rather than exposed to DMA.
        bytes[100..104].copy_from_slice(&512_u32.to_le_bytes());
        assert_eq!(Firmware::parse(&bytes).unwrap().image_len, 512);
    }

    #[test]
    fn rejects_empty_and_out_of_image_boot_code() {
        assert!(Firmware::parse(&image(1024, 256, 0)).is_err());
        assert!(Firmware::parse(&image(1024, 768, 1)).is_err());
        // The count rounds up, so checking raw tag+size misses the tail DMA.
        assert!(Firmware::parse(&image(1023, 256, 257)).is_err());
        assert!(Firmware::parse(&image(1024, 256, 257)).is_ok());
        assert!(Firmware::parse(&image(1024, 1, 256)).is_err());
    }

    #[test]
    fn rejects_arithmetic_overflow_and_register_field_truncation() {
        for (tag, size) in [
            (u32::MAX, 256),
            (0, u32::MAX),
            (1024 * 256, 256),
            (0, 256 * 256),
        ] {
            assert!(Firmware::parse(&image(1024, tag, size)).is_err());
        }
    }

    #[test]
    fn rejects_other_firmware_formats() {
        let mut bytes = image(1024, 256, 256);
        bytes[104] ^= 1;
        assert_eq!(Firmware::parse(&bytes), Err("XUSB firmware magic invalid"));
    }

    #[test]
    fn parses_bundled_tegra210_firmware() {
        let bytes = IMAGE;
        let metadata = Firmware::parse(bytes).unwrap();
        assert_eq!(metadata.image_len, bytes.len());
        assert_eq!(metadata.code_size_blocks, 5);
        assert_eq!(metadata.code_tag_blocks, 488);
    }
}

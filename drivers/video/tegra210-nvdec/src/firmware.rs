// SPDX-License-Identifier: GPL-2.0-only
//! NVIDIA Falcon v1 container, following Linux v6.12 tegra/falcon.c.

pub const IMAGE: &[u8] = include_bytes!("../firmware/nvdec.bin");

#[derive(Debug)]
pub struct Firmware {
    pub base: usize,
    pub code: usize,
    pub code_len: usize,
    pub data: usize,
    pub data_len: usize,
}

fn word(bytes: &[u8], at: usize) -> Result<usize, &'static str> {
    let value = bytes
        .get(at..at.checked_add(4).ok_or("NVDEC firmware overflow")?)
        .ok_or("NVDEC firmware field truncated")?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()) as usize)
}

impl Firmware {
    pub fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        if !matches!(word(bytes, 0)?, 0x10de | 0x10fe) || word(bytes, 4)? != 1 {
            return Err("NVDEC firmware container unsupported");
        }
        let size = word(bytes, 8)?;
        if size != bytes.len() {
            return Err("NVDEC firmware size mismatch");
        }
        let header = word(bytes, 12)?;
        let base = word(bytes, 16)?;
        let os_len = word(bytes, 20)?;
        if header > size.saturating_sub(16)
            || base & 255 != 0
            || base.checked_add(os_len) != Some(size)
        {
            return Err("NVDEC firmware OS range invalid");
        }
        let result = Self {
            base,
            code: word(bytes, header)?,
            code_len: word(bytes, header + 4)?,
            data: word(bytes, header + 8)?,
            data_len: word(bytes, header + 12)?,
        };
        for (offset, len) in [
            (result.code, result.code_len),
            (result.data, result.data_len),
        ] {
            if offset & 255 != 0
                || len == 0
                || len & 255 != 0
                || offset.checked_add(len).is_none_or(|end| end > os_len)
            {
                return Err("NVDEC firmware DMA range invalid");
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_real_image_and_rejects_truncated_dma() {
        let fw = Firmware::parse(IMAGE).unwrap();
        assert_eq!(
            (fw.base, fw.code, fw.code_len, fw.data, fw.data_len),
            (0x200, 0, 0x300, 0x1ee00, 0x400)
        );
        assert!(Firmware::parse(&IMAGE[..IMAGE.len() - 1]).is_err());
        let mut corrupt = IMAGE.to_vec();
        corrupt[0x108..0x10c].copy_from_slice(&0xffff_ff00u32.to_le_bytes());
        assert!(Firmware::parse(&corrupt).is_err());
    }
}

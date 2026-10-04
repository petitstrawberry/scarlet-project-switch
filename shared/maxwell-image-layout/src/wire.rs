//! Fixed, address-free image transfer metadata shared by userspace and GM20B.
//!
//! Attachment tokens identify context-local objects; rectangles and subresources
//! remain untrusted until the kernel checks them against those objects' layouts.
//! All multi-byte fields use little endian, independently of the host's endian.

/// Four-byte image command magic (`GM2I`) interpreted as little endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"GM2I");
pub const VERSION: u16 = 1;
pub const RECORD_SIZE: usize = 128;

const FLIP_X: u32 = 1;
const FLIP_Y: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidSize,
    UnsupportedVersion,
    InvalidField,
    ReservedNotZero,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Opcode {
    Upload = 1,
    Blit = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Filter {
    Nearest = 0,
    Linear = 1,
}

/// One upload or image blit. Rectangles are `[x, y, width, height]`.
///
/// Uploads read a linear, four-byte-per-pixel attachment at `source_offset`,
/// with `source_stride` bytes between rows. Blits select two image subresources
/// and use no raw byte offset or stride. Neither operation carries an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub opcode: Opcode,
    pub source_attachment: u64,
    pub destination_attachment: u64,
    pub source_offset: u64,
    pub source_stride: u32,
    pub source_mip: u32,
    pub source_layer: u32,
    pub destination_mip: u32,
    pub destination_layer: u32,
    pub source_rect: [u32; 4],
    pub destination_rect: [u32; 4],
    pub filter: Filter,
    pub flip_x: bool,
    pub flip_y: bool,
}

fn validate_rect(rect: [u32; 4]) -> Result<(), Error> {
    if rect[2] == 0 || rect[3] == 0 {
        return Err(Error::InvalidField);
    }
    rect[0].checked_add(rect[2]).ok_or(Error::Overflow)?;
    rect[1].checked_add(rect[3]).ok_or(Error::Overflow)?;
    Ok(())
}

impl Command {
    /// Validate structure and arithmetic. Object authority and image bounds are
    /// checked by the kernel after resolving both attachment tokens.
    pub fn validate(&self) -> Result<(), Error> {
        if self.source_attachment == 0 || self.destination_attachment == 0 {
            return Err(Error::InvalidField);
        }
        validate_rect(self.source_rect)?;
        validate_rect(self.destination_rect)?;
        match self.opcode {
            Opcode::Upload => {
                if self.source_mip != 0
                    || self.source_layer != 0
                    || self.source_rect[0] != 0
                    || self.source_rect[1] != 0
                    || self.source_rect[2..] != self.destination_rect[2..]
                    || self.filter != Filter::Nearest
                    || self.flip_x
                    || self.flip_y
                {
                    return Err(Error::InvalidField);
                }
                let span = self.upload_span()?;
                self.source_offset
                    .checked_add(span)
                    .ok_or(Error::Overflow)?;
            }
            Opcode::Blit => {
                if self.source_offset != 0 || self.source_stride != 0 {
                    return Err(Error::InvalidField);
                }
            }
        }
        Ok(())
    }

    /// Bytes read by an upload, from the first pixel to the last row's end.
    /// Includes inter-row padding but excludes padding after the last row.
    /// A blit has no upload span and returns `InvalidField`.
    pub fn upload_span(&self) -> Result<u64, Error> {
        if self.opcode != Opcode::Upload {
            return Err(Error::InvalidField);
        }
        validate_rect(self.source_rect)?;
        let row_bytes = self.source_rect[2].checked_mul(4).ok_or(Error::Overflow)?;
        if self.source_stride < row_bytes {
            return Err(Error::InvalidField);
        }
        u64::from(self.source_rect[3] - 1)
            .checked_mul(u64::from(self.source_stride))
            .and_then(|padding| padding.checked_add(u64::from(row_bytes)))
            .ok_or(Error::Overflow)
    }

    /// Encode exactly one record, clearing every reserved field.
    /// Invalid metadata leaves the destination unchanged.
    pub fn encode_into(&self, bytes: &mut [u8]) -> Result<(), Error> {
        if bytes.len() != RECORD_SIZE {
            return Err(Error::InvalidSize);
        }
        self.validate()?;
        bytes.fill(0);
        put_u32(bytes, 0, MAGIC);
        put_u16(bytes, 4, VERSION);
        put_u16(bytes, 6, self.opcode as u16);
        let flags = if self.flip_x { FLIP_X } else { 0 } | if self.flip_y { FLIP_Y } else { 0 };
        put_u32(bytes, 8, flags);
        put_u32(bytes, 12, self.filter as u32);
        put_u64(bytes, 16, self.source_attachment);
        put_u64(bytes, 24, self.destination_attachment);
        put_u64(bytes, 32, self.source_offset);
        put_u32(bytes, 40, self.source_stride);
        put_u32(bytes, 44, self.source_mip);
        put_u32(bytes, 48, self.source_layer);
        put_u32(bytes, 52, self.destination_mip);
        put_u32(bytes, 56, self.destination_layer);
        for i in 0..4 {
            put_u32(bytes, 64 + i * 4, self.source_rect[i]);
            put_u32(bytes, 80 + i * 4, self.destination_rect[i]);
        }
        Ok(())
    }

    /// Parse and structurally validate exactly one untrusted wire record.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != RECORD_SIZE {
            return Err(Error::InvalidSize);
        }
        if read_u32(bytes, 0) != MAGIC || read_u16(bytes, 4) != VERSION {
            return Err(Error::UnsupportedVersion);
        }
        let flags = read_u32(bytes, 8);
        if flags & !(FLIP_X | FLIP_Y) != 0
            || bytes[60..64].iter().any(|&byte| byte != 0)
            || bytes[96..].iter().any(|&byte| byte != 0)
        {
            return Err(Error::ReservedNotZero);
        }
        let opcode = match read_u16(bytes, 6) {
            1 => Opcode::Upload,
            2 => Opcode::Blit,
            _ => return Err(Error::InvalidField),
        };
        let filter = match read_u32(bytes, 12) {
            0 => Filter::Nearest,
            1 => Filter::Linear,
            _ => return Err(Error::InvalidField),
        };
        let command = Self {
            opcode,
            source_attachment: read_u64(bytes, 16),
            destination_attachment: read_u64(bytes, 24),
            source_offset: read_u64(bytes, 32),
            source_stride: read_u32(bytes, 40),
            source_mip: read_u32(bytes, 44),
            source_layer: read_u32(bytes, 48),
            destination_mip: read_u32(bytes, 52),
            destination_layer: read_u32(bytes, 56),
            source_rect: core::array::from_fn(|i| read_u32(bytes, 64 + i * 4)),
            destination_rect: core::array::from_fn(|i| read_u32(bytes, 80 + i * 4)),
            filter,
            flip_x: flags & FLIP_X != 0,
            flip_y: flags & FLIP_Y != 0,
        };
        command.validate()?;
        Ok(command)
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload() -> Command {
        Command {
            opcode: Opcode::Upload,
            source_attachment: 0x1020_3040_5060_7080,
            destination_attachment: 0x9080_7060_5040_3020,
            source_offset: 64,
            source_stride: 24,
            source_mip: 0,
            source_layer: 0,
            destination_mip: 3,
            destination_layer: 5,
            source_rect: [0, 0, 4, 3],
            destination_rect: [7, 9, 4, 3],
            filter: Filter::Nearest,
            flip_x: false,
            flip_y: false,
        }
    }
    fn blit() -> Command {
        Command {
            opcode: Opcode::Blit,
            source_offset: 0,
            source_stride: 0,
            source_mip: 2,
            source_layer: 6,
            source_rect: [1, 2, 3, 4],
            destination_rect: [10, 20, 6, 8],
            filter: Filter::Linear,
            flip_x: true,
            flip_y: true,
            ..upload()
        }
    }
    fn encode(command: Command) -> [u8; RECORD_SIZE] {
        let mut bytes = [0xff; RECORD_SIZE];
        command.encode_into(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn upload_round_trip_has_canonical_little_endian_layout() {
        let command = upload();
        let bytes = encode(command);
        assert_eq!(&bytes[..8], b"GM2I\x01\0\x01\0");
        assert_eq!(
            &bytes[16..24],
            &[0x80, 0x70, 0x60, 0x50, 0x40, 0x30, 0x20, 0x10]
        );
        assert_eq!(&bytes[60..64], &[0; 4]);
        assert_eq!(&bytes[96..], &[0; 32]);
        assert_eq!(Command::decode(&bytes), Ok(command));
        assert_eq!(command.upload_span(), Ok(64));
    }

    #[test]
    fn blit_round_trip_preserves_scaled_rectangles_and_flips() {
        let command = blit();
        let bytes = encode(command);
        assert_eq!(&bytes[8..12], &[3, 0, 0, 0]);
        assert_eq!(Command::decode(&bytes), Ok(command));
        assert_eq!(command.upload_span(), Err(Error::InvalidField));
    }

    #[test]
    fn every_reserved_byte_and_flag_is_rejected() {
        let bytes = encode(blit());
        for offset in (60..64).chain(96..RECORD_SIZE) {
            let mut corrupt = bytes;
            corrupt[offset] = 1;
            assert_eq!(Command::decode(&corrupt), Err(Error::ReservedNotZero));
        }
        for bit in 2..32 {
            let mut corrupt = bytes;
            put_u32(&mut corrupt, 8, 1 << bit);
            assert_eq!(Command::decode(&corrupt), Err(Error::ReservedNotZero));
        }
    }

    #[test]
    fn non_record_lengths_are_rejected_without_panics() {
        let bytes = encode(upload());
        for len in 0..RECORD_SIZE {
            assert_eq!(Command::decode(&bytes[..len]), Err(Error::InvalidSize));
            let mut short = [0; RECORD_SIZE];
            assert_eq!(
                upload().encode_into(&mut short[..len]),
                Err(Error::InvalidSize)
            );
        }
        assert_eq!(
            Command::decode(&[0; RECORD_SIZE + 1]),
            Err(Error::InvalidSize)
        );
    }

    #[test]
    fn unknown_opcode_filter_magic_and_version_are_rejected() {
        let bytes = encode(upload());
        for opcode in [0, 3, u16::MAX] {
            let mut corrupt = bytes;
            put_u16(&mut corrupt, 6, opcode);
            assert_eq!(Command::decode(&corrupt), Err(Error::InvalidField));
        }
        for filter in [2, u32::MAX] {
            let mut corrupt = bytes;
            put_u32(&mut corrupt, 12, filter);
            assert_eq!(Command::decode(&corrupt), Err(Error::InvalidField));
        }
        let mut corrupt = bytes;
        corrupt[0] ^= 1;
        assert_eq!(Command::decode(&corrupt), Err(Error::UnsupportedVersion));
        let mut corrupt = bytes;
        put_u16(&mut corrupt, 4, 2);
        assert_eq!(Command::decode(&corrupt), Err(Error::UnsupportedVersion));
    }

    #[test]
    fn invalid_upload_fields_and_empty_tokens_are_rejected() {
        for command in [
            Command {
                source_attachment: 0,
                ..upload()
            },
            Command {
                destination_attachment: 0,
                ..upload()
            },
            Command {
                source_stride: 15,
                ..upload()
            },
            Command {
                source_mip: 1,
                ..upload()
            },
            Command {
                source_layer: 1,
                ..upload()
            },
            Command {
                source_rect: [1, 0, 4, 3],
                ..upload()
            },
            Command {
                source_rect: [0, 1, 4, 3],
                ..upload()
            },
            Command {
                source_rect: [0, 0, 0, 3],
                ..upload()
            },
            Command {
                source_rect: [0, 0, 4, 0],
                ..upload()
            },
            Command {
                destination_rect: [7, 9, 5, 3],
                ..upload()
            },
            Command {
                destination_rect: [7, 9, 4, 4],
                ..upload()
            },
            Command {
                filter: Filter::Linear,
                ..upload()
            },
            Command {
                flip_x: true,
                ..upload()
            },
            Command {
                flip_y: true,
                ..upload()
            },
            Command {
                source_offset: 1,
                ..blit()
            },
            Command {
                source_stride: 1,
                ..blit()
            },
        ] {
            assert_eq!(command.validate(), Err(Error::InvalidField));
            let mut bytes = [0xaa; RECORD_SIZE];
            assert_eq!(command.encode_into(&mut bytes), Err(Error::InvalidField));
            assert_eq!(bytes, [0xaa; RECORD_SIZE]);
        }
    }

    #[test]
    fn rectangle_byte_width_and_source_range_overflows_are_rejected() {
        for command in [
            Command {
                source_rect: [u32::MAX, 0, 1, 1],
                ..blit()
            },
            Command {
                source_rect: [0, u32::MAX, 1, 1],
                ..blit()
            },
            Command {
                destination_rect: [u32::MAX, 0, 1, 1],
                ..blit()
            },
            Command {
                destination_rect: [0, u32::MAX, 1, 1],
                ..blit()
            },
            Command {
                source_rect: [0, 0, u32::MAX, 1],
                destination_rect: [0, 0, u32::MAX, 1],
                ..upload()
            },
            Command {
                source_offset: u64::MAX - 63,
                ..upload()
            },
        ] {
            assert_eq!(command.validate(), Err(Error::Overflow));
        }
        let command = Command {
            source_offset: u64::MAX - 64,
            ..upload()
        };
        assert_eq!(command.validate(), Ok(()));
    }

    #[test]
    fn decoder_validates_mutated_upload_stride_and_offset() {
        let mut bytes = encode(upload());
        put_u32(&mut bytes, 40, 15);
        assert_eq!(Command::decode(&bytes), Err(Error::InvalidField));
        let mut bytes = encode(upload());
        put_u64(&mut bytes, 32, u64::MAX);
        assert_eq!(Command::decode(&bytes), Err(Error::Overflow));
    }
}

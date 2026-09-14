//! Versioned userspace/kernel submit wire format for GM20B canonical SGFX operations.
//!
//! The outer Scarlet GPU queue submit remains backend-neutral and opaque. This
//! crate only encodes and structurally decodes the Maxwell payload carried inside
//! that envelope. Operation grammar, hardware method emission and resource authority checks remain in
//! the concrete kernel driver.

#![no_std]

use maxwell_shader_pack::{SHADER_SIZE, ShaderVariant};

/// Four-byte Maxwell submit magic (`GM2S`) interpreted as little endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"GM2S");
/// Wire ABI major version.
pub const VERSION_MAJOR: u16 = 1;
/// Wire ABI minor version.
/// Minor 1 assigns relocation byte 30 to `RelocationSource`; minor 0 required
/// it to be zero and therefore supports attachment sources only.
pub const VERSION_MINOR: u16 = 1;
/// Fixed v1 header size.
pub const HEADER_SIZE: usize = 64;
/// Fixed v1 resource record size.
pub const RESOURCE_SIZE: usize = 32;
/// Fixed v1 relocation record size.
pub const RELOCATION_SIZE: usize = 32;
/// Maximum GM20B payload accepted by Scarlet's opaque queue transport.
///
/// Commands and their resource authority must both fit this transport budget.
pub const MAX_SUBMIT_SIZE: usize = 2 * 1024 * 1024;
/// Fixed size of one canonical operation record.
pub const OPERATION_WORDS: usize = 64;
/// Maximum operations lowered into one bounded hardware pushbuffer.
pub const MAX_OPERATIONS: usize = 1_024;
pub const MAX_COMMAND_WORDS: usize = OPERATION_WORDS * MAX_OPERATIONS;
/// Maximum resources referenced by one v1 submission.
pub const MAX_RESOURCES: usize = 1_024;
/// Maximum relocations referenced by one v1 submission.
pub const MAX_RELOCATIONS: usize = 8_192;

/// Resource is read by the GPU.
pub const ACCESS_READ: u32 = 1 << 0;
/// Resource is written by the GPU.
pub const ACCESS_WRITE: u32 = 1 << 1;
/// All access bits understood by v1.
pub const ACCESS_MASK: u32 = ACCESS_READ | ACCESS_WRITE;

/// Structural wire-format failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An integer calculation overflowed.
    Overflow,
    /// The provided byte buffer is too small or larger than the v1 limit.
    InvalidSize,
    /// The magic or major ABI version is not supported.
    UnsupportedVersion,
    /// A field or record uses an unsupported value.
    InvalidField,
    /// Reserved v1 bytes or bits are non-zero.
    ReservedNotZero,
    /// Table offsets, sizes, ordering, or alignment are not canonical.
    InvalidTable,
    /// Relocations are not strictly ordered by commands word offset.
    RelocationsNotSorted,
    /// A relocation falls outside commands or its selected resource range.
    RelocationOutOfBounds,
    /// An address placeholder is not zero before kernel relocation.
    NonZeroPlaceholder,
}

/// Resource authority referenced by the Maxwell command stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resource {
    /// Context-local, generation-checked attachment token.
    pub attachment_token: u64,
    /// First byte within the attached object made visible to this submit.
    pub range_offset: u64,
    /// Number of visible bytes starting at `range_offset`.
    pub range_size: u64,
    /// `ACCESS_READ`, `ACCESS_WRITE`, or both.
    pub access: u32,
}

/// Encoding of a symbolic, capability-authorized object reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum AddressEncoding {
    GpuVa64 = 1,
}
impl AddressEncoding {
    const fn from_raw(value: u16) -> Result<Self, Error> {
        if value == 1 {
            Ok(Self::GpuVa64)
        } else {
            Err(Error::InvalidField)
        }
    }
    pub const fn word_count(self) -> u32 {
        2
    }
    fn placeholder_is_valid(self, words: &[u32]) -> bool {
        words == [0, 0]
    }
}

/// One kernel-applied symbolic address relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relocation {
    /// First address-placeholder dword in the inline commands table.
    pub commands_word_offset: u32,
    /// Strictly typed source of the relocated address.
    pub source: RelocationSource,
    /// Byte offset relative to the resource record's visible range.
    pub resource_offset: u64,
    /// Minimum valid byte range beginning at `resource_offset`.
    pub required_size: u64,
    /// Access required by the commands operation.
    pub access: u32,
    /// Address representation expected by the commands field.
    pub encoding: AddressEncoding,
}

/// Authority from which the kernel resolves one relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelocationSource {
    /// An attachment authorized by one resource table record.
    Attachment(u32),
    /// An immutable program in the kernel-owned canonical shader pack.
    CanonicalShader(ShaderVariant),
}

/// Borrowed data to encode as one canonical v1 payload.
#[derive(Clone, Copy, Debug)]
pub struct Submit<'a> {
    /// Inline, address-free commands dwords.
    pub commands: &'a [u32],
    /// Context-local resources used by the commands stream.
    pub resources: &'a [Resource],
    /// Symbolic addresses patched by the kernel.
    pub relocations: &'a [Relocation],
}

#[derive(Clone, Copy)]
struct Layout {
    total: usize,
    commands_offset: usize,
    resources_offset: usize,
    relocations_offset: usize,
}

const fn align_up(value: usize, alignment: usize) -> Result<usize, Error> {
    match value.checked_add(alignment - 1) {
        Some(value) => Ok(value & !(alignment - 1)),
        None => Err(Error::Overflow),
    }
}

fn layout(
    commands_count: usize,
    resource_count: usize,
    relocation_count: usize,
) -> Result<Layout, Error> {
    if commands_count == 0
        || commands_count % OPERATION_WORDS != 0
        || commands_count > MAX_COMMAND_WORDS
        || resource_count > MAX_RESOURCES
        || relocation_count > MAX_RELOCATIONS
    {
        return Err(Error::InvalidSize);
    }
    let commands_size = commands_count.checked_mul(4).ok_or(Error::Overflow)?;
    let resources_size = resource_count
        .checked_mul(RESOURCE_SIZE)
        .ok_or(Error::Overflow)?;
    let relocations_size = relocation_count
        .checked_mul(RELOCATION_SIZE)
        .ok_or(Error::Overflow)?;
    let commands_offset = HEADER_SIZE;
    let resources_offset = align_up(
        commands_offset
            .checked_add(commands_size)
            .ok_or(Error::Overflow)?,
        8,
    )?;
    let relocations_offset = resources_offset
        .checked_add(resources_size)
        .ok_or(Error::Overflow)?;
    let total = relocations_offset
        .checked_add(relocations_size)
        .ok_or(Error::Overflow)?;
    if total > MAX_SUBMIT_SIZE || total > u32::MAX as usize {
        return Err(Error::InvalidSize);
    }
    Ok(Layout {
        total,
        commands_offset,
        resources_offset,
        relocations_offset,
    })
}

/// Return the exact number of bytes needed to encode `submit`.
pub fn encoded_len(submit: Submit<'_>) -> Result<usize, Error> {
    Ok(layout(
        submit.commands.len(),
        submit.resources.len(),
        submit.relocations.len(),
    )?
    .total)
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut [u8], offset: usize, value: u64) {
    output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(input: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([input[offset], input[offset + 1]])
}

fn get_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ])
}

fn get_u64(input: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
        input[offset + 4],
        input[offset + 5],
        input[offset + 6],
        input[offset + 7],
    ])
}

fn validate_access(access: u32) -> Result<(), Error> {
    if access == 0 || access & !ACCESS_MASK != 0 {
        Err(Error::InvalidField)
    } else {
        Ok(())
    }
}

fn validate_resource(resource: Resource) -> Result<(), Error> {
    validate_access(resource.access)?;
    if resource.attachment_token == 0
        || resource.range_size == 0
        || resource
            .range_offset
            .checked_add(resource.range_size)
            .is_none()
    {
        return Err(Error::InvalidField);
    }
    Ok(())
}

fn validate_relocation(
    relocation: Relocation,
    previous_end: Option<u32>,
    commands: &[u32],
    resources: &[Resource],
) -> Result<(), Error> {
    validate_access(relocation.access)?;
    if previous_end.is_some_and(|previous| previous > relocation.commands_word_offset) {
        return Err(Error::RelocationsNotSorted);
    }
    match relocation.source {
        RelocationSource::Attachment(resource_index) => {
            let resource = resources
                .get(resource_index as usize)
                .ok_or(Error::RelocationOutOfBounds)?;
            if relocation.access & !resource.access != 0 || relocation.required_size == 0 {
                return Err(Error::RelocationOutOfBounds);
            }
            let range_end = relocation
                .resource_offset
                .checked_add(relocation.required_size)
                .ok_or(Error::Overflow)?;
            if range_end > resource.range_size {
                return Err(Error::RelocationOutOfBounds);
            }
        }
        RelocationSource::CanonicalShader(_) => {
            if relocation.resource_offset != 0
                || relocation.required_size != SHADER_SIZE as u64
                || relocation.access != ACCESS_READ
                || relocation.encoding != AddressEncoding::GpuVa64
            {
                return Err(Error::RelocationOutOfBounds);
            }
        }
    }
    let word_end = relocation
        .commands_word_offset
        .checked_add(relocation.encoding.word_count())
        .ok_or(Error::Overflow)?;
    if word_end as usize > commands.len() {
        return Err(Error::RelocationOutOfBounds);
    }
    let start = relocation.commands_word_offset as usize;
    if !relocation
        .encoding
        .placeholder_is_valid(&commands[start..word_end as usize])
    {
        return Err(Error::NonZeroPlaceholder);
    }
    Ok(())
}

/// Encode one canonical v1 payload into caller-owned storage.
///
/// The destination length must exactly equal [`encoded_len`].
pub fn encode(submit: Submit<'_>, output: &mut [u8]) -> Result<(), Error> {
    let layout = layout(
        submit.commands.len(),
        submit.resources.len(),
        submit.relocations.len(),
    )?;
    if output.len() != layout.total {
        return Err(Error::InvalidSize);
    }
    for resource in submit.resources {
        validate_resource(*resource)?;
    }
    let mut previous_end = None;
    for relocation in submit.relocations {
        validate_relocation(*relocation, previous_end, submit.commands, submit.resources)?;
        previous_end = relocation
            .commands_word_offset
            .checked_add(relocation.encoding.word_count());
    }

    output.fill(0);
    put_u32(output, 0, MAGIC);
    put_u16(output, 4, VERSION_MAJOR);
    put_u16(output, 6, VERSION_MINOR);
    put_u16(output, 8, HEADER_SIZE as u16);
    put_u16(output, 10, 0);
    put_u32(output, 12, layout.total as u32);
    put_u32(output, 16, layout.commands_offset as u32);
    put_u32(output, 20, submit.commands.len() as u32);
    put_u32(output, 24, layout.resources_offset as u32);
    put_u32(output, 28, submit.resources.len() as u32);
    put_u32(output, 32, layout.relocations_offset as u32);
    put_u32(output, 36, submit.relocations.len() as u32);

    for (index, word) in submit.commands.iter().enumerate() {
        put_u32(output, layout.commands_offset + index * 4, *word);
    }
    for (index, resource) in submit.resources.iter().enumerate() {
        let offset = layout.resources_offset + index * RESOURCE_SIZE;
        put_u64(output, offset, resource.attachment_token);
        put_u64(output, offset + 8, resource.range_offset);
        put_u64(output, offset + 16, resource.range_size);
        put_u32(output, offset + 24, resource.access);
    }
    for (index, relocation) in submit.relocations.iter().enumerate() {
        let offset = layout.relocations_offset + index * RELOCATION_SIZE;
        let (source_kind, source_index) = match relocation.source {
            RelocationSource::Attachment(index) => (0_u16, index),
            RelocationSource::CanonicalShader(variant) => (1, u32::from(variant.raw())),
        };
        put_u32(output, offset, relocation.commands_word_offset);
        put_u32(output, offset + 4, source_index);
        put_u64(output, offset + 8, relocation.resource_offset);
        put_u64(output, offset + 16, relocation.required_size);
        put_u32(output, offset + 24, relocation.access);
        put_u16(output, offset + 28, relocation.encoding as u16);
        put_u16(output, offset + 30, source_kind);
    }
    Ok(())
}

/// Structurally validated, borrowed v1 payload.
#[derive(Clone, Copy)]
pub struct DecodedSubmit<'a> {
    bytes: &'a [u8],
    layout: Layout,
    commands_count: usize,
    resource_count: usize,
    relocation_count: usize,
}

impl<'a> DecodedSubmit<'a> {
    /// Number of inline commands dwords.
    pub const fn commands_len(&self) -> usize {
        self.commands_count
    }

    /// Read one inline commands dword.
    pub fn commands_word(&self, index: usize) -> Option<u32> {
        (index < self.commands_count)
            .then(|| get_u32(self.bytes, self.layout.commands_offset + index * 4))
    }

    /// Number of resource records.
    pub const fn resource_len(&self) -> usize {
        self.resource_count
    }

    /// Decode one resource record.
    pub fn resource(&self, index: usize) -> Option<Resource> {
        if index >= self.resource_count {
            return None;
        }
        let offset = self.layout.resources_offset + index * RESOURCE_SIZE;
        Some(Resource {
            attachment_token: get_u64(self.bytes, offset),
            range_offset: get_u64(self.bytes, offset + 8),
            range_size: get_u64(self.bytes, offset + 16),
            access: get_u32(self.bytes, offset + 24),
        })
    }

    /// Number of relocation records.
    pub const fn relocation_len(&self) -> usize {
        self.relocation_count
    }

    /// Decode one relocation record.
    pub fn relocation(&self, index: usize) -> Option<Relocation> {
        if index >= self.relocation_count {
            return None;
        }
        let offset = self.layout.relocations_offset + index * RELOCATION_SIZE;
        let source = match get_u16(self.bytes, offset + 30) {
            0 => RelocationSource::Attachment(get_u32(self.bytes, offset + 4)),
            1 if get_u16(self.bytes, 6) >= 1 => RelocationSource::CanonicalShader(
                ShaderVariant::from_raw(u16::try_from(get_u32(self.bytes, offset + 4)).ok()?)?,
            ),
            _ => return None,
        };
        Some(Relocation {
            commands_word_offset: get_u32(self.bytes, offset),
            source,
            resource_offset: get_u64(self.bytes, offset + 8),
            required_size: get_u64(self.bytes, offset + 16),
            access: get_u32(self.bytes, offset + 24),
            encoding: AddressEncoding::from_raw(get_u16(self.bytes, offset + 28)).ok()?,
        })
    }
}

/// Decode and structurally validate a complete canonical v1 payload.
pub fn decode(bytes: &[u8]) -> Result<DecodedSubmit<'_>, Error> {
    if bytes.len() < HEADER_SIZE || bytes.len() > MAX_SUBMIT_SIZE {
        return Err(Error::InvalidSize);
    }
    if get_u32(bytes, 0) != MAGIC || get_u16(bytes, 4) != VERSION_MAJOR {
        return Err(Error::UnsupportedVersion);
    }
    if get_u16(bytes, 6) > VERSION_MINOR || get_u16(bytes, 8) as usize != HEADER_SIZE {
        return Err(Error::UnsupportedVersion);
    }
    if get_u16(bytes, 10) != 0 || bytes[40..HEADER_SIZE].iter().any(|byte| *byte != 0) {
        return Err(Error::ReservedNotZero);
    }
    if get_u32(bytes, 12) as usize != bytes.len() {
        return Err(Error::InvalidSize);
    }
    let commands_offset = get_u32(bytes, 16) as usize;
    let commands_count = get_u32(bytes, 20) as usize;
    let resources_offset = get_u32(bytes, 24) as usize;
    let resource_count = get_u32(bytes, 28) as usize;
    let relocations_offset = get_u32(bytes, 32) as usize;
    let relocation_count = get_u32(bytes, 36) as usize;
    let expected = layout(commands_count, resource_count, relocation_count)?;
    if commands_offset != expected.commands_offset
        || resources_offset != expected.resources_offset
        || relocations_offset != expected.relocations_offset
        || expected.total != bytes.len()
    {
        return Err(Error::InvalidTable);
    }
    let commands_padding_start = expected
        .commands_offset
        .checked_add(commands_count.checked_mul(4).ok_or(Error::Overflow)?)
        .ok_or(Error::Overflow)?;
    if bytes[commands_padding_start..expected.resources_offset]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(Error::ReservedNotZero);
    }
    let decoded = DecodedSubmit {
        bytes,
        layout: expected,
        commands_count,
        resource_count,
        relocation_count,
    };
    for index in 0..resource_count {
        let offset = expected.resources_offset + index * RESOURCE_SIZE;
        if get_u32(bytes, offset + 28) != 0 {
            return Err(Error::ReservedNotZero);
        }
        validate_resource(decoded.resource(index).ok_or(Error::InvalidTable)?)?;
    }
    let mut previous_end = None;
    for index in 0..relocation_count {
        let relocation = decoded.relocation(index).ok_or(Error::InvalidField)?;
        validate_access(relocation.access)?;
        if previous_end.is_some_and(|value| value > relocation.commands_word_offset) {
            return Err(Error::RelocationsNotSorted);
        }
        match relocation.source {
            RelocationSource::Attachment(resource_index) => {
                let resource = decoded
                    .resource(resource_index as usize)
                    .ok_or(Error::RelocationOutOfBounds)?;
                if relocation.access & !resource.access != 0 || relocation.required_size == 0 {
                    return Err(Error::RelocationOutOfBounds);
                }
                let range_end = relocation
                    .resource_offset
                    .checked_add(relocation.required_size)
                    .ok_or(Error::Overflow)?;
                if range_end > resource.range_size {
                    return Err(Error::RelocationOutOfBounds);
                }
            }
            RelocationSource::CanonicalShader(_) => {
                if relocation.resource_offset != 0
                    || relocation.required_size != SHADER_SIZE as u64
                    || relocation.access != ACCESS_READ
                    || relocation.encoding != AddressEncoding::GpuVa64
                {
                    return Err(Error::RelocationOutOfBounds);
                }
            }
        }
        let word_end = relocation
            .commands_word_offset
            .checked_add(relocation.encoding.word_count())
            .ok_or(Error::Overflow)?;
        if word_end as usize > commands_count {
            return Err(Error::RelocationOutOfBounds);
        }
        let mut placeholder = [0_u32; 2];
        for (destination, word) in placeholder
            .iter_mut()
            .zip(relocation.commands_word_offset..word_end)
        {
            *destination = decoded
                .commands_word(word as usize)
                .ok_or(Error::RelocationOutOfBounds)?;
        }
        if !relocation
            .encoding
            .placeholder_is_valid(&placeholder[..relocation.encoding.word_count() as usize])
        {
            return Err(Error::NonZeroPlaceholder);
        }
        previous_end = Some(word_end);
    }
    Ok(decoded)
}

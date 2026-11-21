// SPDX-License-Identifier: GPL-2.0-only
//! Versioned userspace/kernel native submit wire format for GM20B.
//!
//! The outer Scarlet GPU queue submit remains backend-neutral and opaque. This
//! crate only encodes and structurally decodes the native Maxwell method
//! stream carried inside that envelope. Pushbuffer grammar, hardware method
//! policy and resource authority checks remain in the concrete kernel driver.

#![no_std]

/// Four-byte Maxwell native submit magic (`GM2N`) interpreted as little endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"GM2N");
/// Wire ABI major version.
pub const VERSION_MAJOR: u16 = 1;
/// Wire ABI minor version.
/// Minor 0 authorizes attachment-backed method addresses only. Relocations
/// into CB descriptor content and native program attachments arrive with
/// later minors; the concrete kernel driver still gates their use.
pub const VERSION_MINOR: u16 = 0;
/// Scarlet dialect negotiation token: GM2B family, wire major, wire minor.
/// Query this token from the kernel; the local library version is not evidence
/// that a running kernel accepts native method streams.
pub const DIALECT_TOKEN: u64 =
    0x474d_324e_0000_0000 | ((VERSION_MAJOR as u64) << 16) | VERSION_MINOR as u64;
/// Fixed v1 header size.
pub const HEADER_SIZE: usize = 64;
/// Fixed v1 resource record size.
pub const RESOURCE_SIZE: usize = 32;
/// Fixed v1 relocation record size.
pub const RELOCATION_SIZE: usize = 32;
/// Maximum GM20B payload accepted by Scarlet's opaque queue transport.
pub const MAX_SUBMIT_SIZE: usize = 2 * 1024 * 1024;
/// Maximum method words in one native stream. This matches the trusted
/// pushbuffer budget previously lowered from canonical operations.
pub const MAX_METHOD_WORDS: usize = 0x40000;
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
    /// Relocations are not strictly ordered by method word offset.
    RelocationsNotSorted,
    /// A relocation falls outside the method stream or its selected resource.
    RelocationOutOfBounds,
    /// An address placeholder is not zero before kernel relocation.
    NonZeroPlaceholder,
}

/// Resource authority referenced by the Maxwell method stream.
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

/// Encoding used to patch one symbolic GPU address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum AddressEncoding {
    /// Two adjacent dwords holding high then low halves of a 64-bit GPU VA.
    GpuVa64HiLo = 1,
    /// One dword holding the low half of a GPU VA below 4 GiB.
    GpuVa32Lo = 2,
    /// TIC descriptor content: low dword plus a high dword whose low 22 bits
    /// carry VA bits 32..53 while descriptor flags stay preserved.
    GpuVa32LoHiFlags = 3,
}

impl AddressEncoding {
    const fn from_raw(value: u16) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::GpuVa64HiLo),
            2 => Ok(Self::GpuVa32Lo),
            3 => Ok(Self::GpuVa32LoHiFlags),
            _ => Err(Error::InvalidField),
        }
    }

    /// Number of method dwords occupied by this address.
    pub const fn word_count(self) -> u32 {
        match self {
            Self::GpuVa64HiLo | Self::GpuVa32LoHiFlags => 2,
            Self::GpuVa32Lo => 1,
        }
    }

    fn placeholder_is_valid(self, words: &[u32]) -> bool {
        match self {
            Self::GpuVa64HiLo => words == [0, 0],
            Self::GpuVa32Lo => words == [0],
            Self::GpuVa32LoHiFlags => {
                words.len() == 2 && words[0] == 0 && words[1] & 0x3f_ffff == 0
            }
        }
    }
}

/// One kernel-applied symbolic address relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relocation {
    /// First address-placeholder dword in the inline method table.
    pub method_word_offset: u32,
    /// Strictly typed source of the relocated address.
    pub source: RelocationSource,
    /// Byte offset relative to the resource record's visible range.
    pub resource_offset: u64,
    /// Minimum valid byte range beginning at `resource_offset`.
    pub required_size: u64,
    /// Access required by the method operation.
    pub access: u32,
    /// Address representation expected by the method field.
    pub encoding: AddressEncoding,
}

/// Authority from which the kernel resolves one relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelocationSource {
    /// An attachment authorized by one resource table record.
    Attachment(u32),
}

/// Borrowed data to encode as one canonical v1 payload.
#[derive(Clone, Copy, Debug)]
pub struct Submit<'a> {
    /// Inline, address-free Maxwell method dwords.
    pub methods: &'a [u32],
    /// Context-local resources used by the method stream.
    pub resources: &'a [Resource],
    /// Symbolic addresses patched by the kernel.
    pub relocations: &'a [Relocation],
}

#[derive(Clone, Copy)]
struct Layout {
    total: usize,
    methods_offset: usize,
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
    method_count: usize,
    resource_count: usize,
    relocation_count: usize,
) -> Result<Layout, Error> {
    if method_count == 0
        || method_count > MAX_METHOD_WORDS
        || resource_count > MAX_RESOURCES
        || relocation_count > MAX_RELOCATIONS
    {
        return Err(Error::InvalidSize);
    }
    let methods_size = method_count.checked_mul(4).ok_or(Error::Overflow)?;
    let resources_size = resource_count
        .checked_mul(RESOURCE_SIZE)
        .ok_or(Error::Overflow)?;
    let relocations_size = relocation_count
        .checked_mul(RELOCATION_SIZE)
        .ok_or(Error::Overflow)?;
    let methods_offset = HEADER_SIZE;
    let resources_offset =
        align_up(methods_offset.checked_add(methods_size).ok_or(Error::Overflow)?, 8)?;
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
        methods_offset,
        resources_offset,
        relocations_offset,
    })
}

/// Check wire table limits and compute the exact allocation size without
/// constructing or encoding payload tables. Content validation still belongs
/// to `encode` and must complete before submission is accepted.
pub fn encoded_len_for_counts(
    methods: usize,
    resources: usize,
    relocations: usize,
) -> Result<usize, Error> {
    Ok(layout(methods, resources, relocations)?.total)
}

/// Return the exact number of bytes needed to encode `submit`.
pub fn encoded_len(submit: Submit<'_>) -> Result<usize, Error> {
    Ok(layout(
        submit.methods.len(),
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
    methods: &[u32],
    resources: &[Resource],
) -> Result<(), Error> {
    validate_access(relocation.access)?;
    if previous_end.is_some_and(|previous| previous > relocation.method_word_offset) {
        return Err(Error::RelocationsNotSorted);
    }
    let RelocationSource::Attachment(resource_index) = relocation.source;
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
    let word_end = relocation
        .method_word_offset
        .checked_add(relocation.encoding.word_count())
        .ok_or(Error::Overflow)?;
    if word_end as usize > methods.len() {
        return Err(Error::RelocationOutOfBounds);
    }
    let start = relocation.method_word_offset as usize;
    if !relocation
        .encoding
        .placeholder_is_valid(&methods[start..word_end as usize])
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
        submit.methods.len(),
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
        validate_relocation(*relocation, previous_end, submit.methods, submit.resources)?;
        previous_end = relocation
            .method_word_offset
            .checked_add(relocation.encoding.word_count());
    }

    output.fill(0);
    put_u32(output, 0, MAGIC);
    put_u16(output, 4, VERSION_MAJOR);
    put_u16(output, 6, VERSION_MINOR);
    put_u16(output, 8, HEADER_SIZE as u16);
    put_u16(output, 10, 0);
    put_u32(output, 12, layout.total as u32);
    put_u32(output, 16, layout.methods_offset as u32);
    put_u32(output, 20, submit.methods.len() as u32);
    put_u32(output, 24, layout.resources_offset as u32);
    put_u32(output, 28, submit.resources.len() as u32);
    put_u32(output, 32, layout.relocations_offset as u32);
    put_u32(output, 36, submit.relocations.len() as u32);

    for (index, word) in submit.methods.iter().enumerate() {
        put_u32(output, layout.methods_offset + index * 4, *word);
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
        let RelocationSource::Attachment(resource_index) = relocation.source;
        put_u32(output, offset, relocation.method_word_offset);
        put_u32(output, offset + 4, resource_index);
        put_u64(output, offset + 8, relocation.resource_offset);
        put_u64(output, offset + 16, relocation.required_size);
        put_u32(output, offset + 24, relocation.access);
        put_u16(output, offset + 28, relocation.encoding as u16);
        put_u16(output, offset + 30, 0);
    }
    Ok(())
}

/// Structurally validated, borrowed v1 payload.
#[derive(Clone, Copy)]
pub struct DecodedSubmit<'a> {
    bytes: &'a [u8],
    layout: Layout,
    method_count: usize,
    resource_count: usize,
    relocation_count: usize,
}

impl<'a> DecodedSubmit<'a> {
    /// Number of inline method dwords.
    pub const fn method_len(&self) -> usize {
        self.method_count
    }

    /// Read one inline method dword.
    pub fn method_word(&self, index: usize) -> Option<u32> {
        (index < self.method_count)
            .then(|| get_u32(self.bytes, self.layout.methods_offset + index * 4))
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
        if get_u16(self.bytes, offset + 30) != 0 {
            return None;
        }
        Some(Relocation {
            method_word_offset: get_u32(self.bytes, offset),
            source: RelocationSource::Attachment(get_u32(self.bytes, offset + 4)),
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
    let method_count = get_u32(bytes, 20) as usize;
    let resource_count = get_u32(bytes, 28) as usize;
    let relocation_count = get_u32(bytes, 36) as usize;
    let expected = layout(method_count, resource_count, relocation_count)?;
    if expected.total != bytes.len() {
        return Err(Error::InvalidSize);
    }
    if get_u32(bytes, 16) as usize != expected.methods_offset
        || get_u32(bytes, 24) as usize != expected.resources_offset
        || get_u32(bytes, 32) as usize != expected.relocations_offset
    {
        return Err(Error::InvalidTable);
    }
    let methods_end = expected.methods_offset + method_count * 4;
    if bytes[methods_end..expected.resources_offset]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(Error::ReservedNotZero);
    }
    let decoded = DecodedSubmit {
        bytes,
        layout: expected,
        method_count,
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
        if previous_end.is_some_and(|previous| previous > relocation.method_word_offset) {
            return Err(Error::RelocationsNotSorted);
        }
        let RelocationSource::Attachment(resource_index) = relocation.source;
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
        let word_end = relocation
            .method_word_offset
            .checked_add(relocation.encoding.word_count())
            .ok_or(Error::Overflow)?;
        if word_end as usize > method_count {
            return Err(Error::RelocationOutOfBounds);
        }
        let mut placeholder = [0_u32; 2];
        for (destination, word) in placeholder
            .iter_mut()
            .zip(relocation.method_word_offset..word_end)
        {
            *destination = decoded
                .method_word(word as usize)
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

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{
        ACCESS_READ, ACCESS_WRITE, AddressEncoding, Error, Relocation, RelocationSource, Resource,
        Submit, decode, encode, encoded_len,
    };
    use std::vec;

    fn sample<'a>(
        methods: &'a [u32],
        resources: &'a [Resource],
        relocs: &'a [Relocation],
    ) -> Submit<'a> {
        Submit {
            methods,
            resources,
            relocations: relocs,
        }
    }

    fn attachment() -> Resource {
        Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ | ACCESS_WRITE,
        }
    }

    fn relocation(
        method_word_offset: u32,
        encoding: AddressEncoding,
    ) -> Relocation {
        Relocation {
            method_word_offset,
            source: RelocationSource::Attachment(0),
            resource_offset: 0,
            required_size: 4096,
            access: ACCESS_READ,
            encoding,
        }
    }

    #[test]
    fn dialect_token_reports_native_wire_version() {
        assert_eq!(super::DIALECT_TOKEN, 0x474d_324e_0001_0000);
        assert_eq!((super::DIALECT_TOKEN >> 16) as u16, super::VERSION_MAJOR);
        assert_eq!(super::DIALECT_TOKEN as u16, super::VERSION_MINOR);
    }

    #[test]
    fn native_payload_round_trips() {
        let methods = [0x2000_0000, 0, 0, 0x5000_0002, 0x40, 1];
        let resources = [attachment()];
        let relocs = [relocation(1, AddressEncoding::GpuVa64HiLo)];
        let submit = sample(&methods, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.method_len(), 6);
        assert_eq!(decoded.resource(0), Some(resources[0]));
        assert_eq!(decoded.relocation(0), Some(relocs[0]));
        assert_eq!(decoded.method_word(4), Some(0x40));
    }

    #[test]
    fn rejects_nonzero_placeholder_for_every_encoding() {
        for (encoding, words) in [
            (AddressEncoding::GpuVa64HiLo, [1u32, 0].as_slice()),
            (AddressEncoding::GpuVa64HiLo, [0, 1].as_slice()),
            (AddressEncoding::GpuVa32Lo, [1].as_slice()),
            (AddressEncoding::GpuVa32LoHiFlags, [1, 0].as_slice()),
            (AddressEncoding::GpuVa32LoHiFlags, [0, 1].as_slice()),
        ] {
            let mut methods = vec![0x2000_0000];
            methods.extend_from_slice(words);
            let resources = [attachment()];
            let relocs = [relocation(1, encoding)];
            let submit = sample(&methods, &resources, &relocs);
            let mut bytes = vec![0; encoded_len(submit).unwrap()];
            assert_eq!(encode(submit, &mut bytes), Err(Error::NonZeroPlaceholder));
        }
    }

    #[test]
    fn tic_flags_bits_are_preserved_by_the_placeholder_rule() {
        let methods = [0x2000_0000, 0, 1 << 22];
        let resources = [attachment()];
        let relocs = [relocation(1, AddressEncoding::GpuVa32LoHiFlags)];
        let submit = sample(&methods, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert_eq!(decode(&bytes).unwrap().method_word(2), Some(1 << 22));
    }

    #[test]
    fn rejects_unsorted_and_overlapping_relocations() {
        for offsets in [[2u32, 1], [1, 2]] {
            let methods = [0, 0, 0, 0];
            let resources = [attachment()];
            let relocs = [
                relocation(offsets[0], AddressEncoding::GpuVa64HiLo),
                relocation(offsets[1], AddressEncoding::GpuVa64HiLo),
            ];
            let submit = sample(&methods, &resources, &relocs);
            let mut bytes = vec![0; encoded_len(submit).unwrap()];
            assert_eq!(encode(submit, &mut bytes), Err(Error::RelocationsNotSorted));
        }
    }

    #[test]
    fn rejects_relocation_outside_its_resource_range() {
        let methods = [0, 0];
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ,
        }];
        let relocs = [Relocation {
            method_word_offset: 0,
            source: RelocationSource::Attachment(0),
            resource_offset: 4096,
            required_size: 8,
            access: ACCESS_READ,
            encoding: AddressEncoding::GpuVa64HiLo,
        }];
        let submit = sample(&methods, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        assert_eq!(encode(submit, &mut bytes), Err(Error::RelocationOutOfBounds));
    }

    #[test]
    fn rejects_relocation_access_not_granted_by_the_resource() {
        let methods = [0, 0];
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ,
        }];
        let mut reloc = relocation(0, AddressEncoding::GpuVa64HiLo);
        reloc.access = ACCESS_WRITE;
        let relocs = [reloc];
        let submit = sample(&methods, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        assert_eq!(encode(submit, &mut bytes), Err(Error::RelocationOutOfBounds));
    }

    #[test]
    fn rejects_empty_methods_and_unknown_relocation_source() {
        let resources = [attachment()];
        let submit = sample(&[], &resources, &[]);
        assert_eq!(encoded_len(submit), Err(Error::InvalidSize));

        let methods = [0, 0];
        let relocs = [relocation(0, AddressEncoding::GpuVa64HiLo)];
        let submit = sample(&methods, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        let relocations_offset =
            u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
        bytes[relocations_offset + 30..relocations_offset + 32]
            .copy_from_slice(&1_u16.to_le_bytes());
        assert!(matches!(decode(&bytes), Err(Error::InvalidField)));
    }

    #[test]
    fn decoder_is_total_for_single_byte_corruption() {
        let methods = [0x2000_0000, 0, 0];
        let resources = [attachment()];
        let relocs = [relocation(1, AddressEncoding::GpuVa64HiLo)];
        let submit = sample(&methods, &resources, &relocs);
        let mut canonical = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut canonical).unwrap();

        for index in 0..canonical.len() {
            for replacement in 0..=u8::MAX {
                let mut corrupted = canonical.clone();
                corrupted[index] = replacement;
                let _ = decode(&corrupted);
            }
        }
    }
}

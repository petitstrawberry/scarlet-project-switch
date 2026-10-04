// SPDX-License-Identifier: GPL-2.0-only
//! Untrusted Maxwell stage packages and a strict SM50/52 executable verifier.
//!
//! This format contains checked IO metadata, never a hardware program header.
//! The kernel must validate a private snapshot with actual bound-resource limits,
//! generate its header here, and keep both outside user-writable attachments.
#![no_std]

extern crate alloc;

pub mod draw;
mod opcodes;
mod verify;
pub use verify::{Validated, validate};

pub const MESA_SHA: &str = "e881540692daac6532cefec76699f7a025563767";
pub const VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 512;
pub const MAX_CODE_SIZE: usize = 64 * 1024;
pub const MAX_CB_SIZE: u32 = 64 * 1024;
pub const MAX_RESOURCES: usize = 64;
pub const FLAG_DEPTH: u16 = 1;
pub const FLAG_SAMPLE_MASK: u16 = 2;
pub const FLAG_KILL: u16 = 4;
pub const FLAG_FP64: u16 = 8;
const MAGIC: &[u8; 8] = b"MXPROG01";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    Vertex = 0,
    Fragment = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Format,
    Version,
    Size,
    Metadata,
    Instruction,
    Scheduling,
    ConstantBuffer,
    Resource,
    Attribute,
    Register,
    Branch,
    ControlStack,
    Fallthrough,
    Scratch,
}

/// The same fields used by pinned NAK's VtgIoInfo/FragmentIoInfo, restricted to
/// the two stages the kernel can install. Generic vectors cover 128 scalar IOs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub stage: Stage,
    pub gprs: u8,
    pub flags: u16,
    pub resource_mask: u64,
    pub cb_sizes: [u32; 32],
    pub attr_in: [u32; 4],
    pub attr_out: [u32; 4],
    pub sysvals_in_ab: u32,
    pub sysvals_in_c: u16,
    pub sysvals_in_d: u8,
    pub sysvals_out_ab: u32,
    pub sysvals_out_c: u16,
    pub sysvals_out_d: u8,
    pub store_req_start: u8,
    pub store_req_end: u8,
    pub fs_inputs: [u8; 128],
    pub fs_sysvals_d: [u8; 8],
    pub fs_color_mask: u32,
}

impl Metadata {
    pub const fn new(stage: Stage) -> Self {
        Self {
            stage,
            gprs: 4,
            flags: 0,
            resource_mask: 0,
            cb_sizes: [0; 32],
            attr_in: [0; 4],
            attr_out: [0; 4],
            sysvals_in_ab: if matches!(stage, Stage::Fragment) {
                1 << 31
            } else {
                0
            },
            sysvals_in_c: 0,
            sysvals_in_d: 0,
            sysvals_out_ab: 0,
            sysvals_out_c: 0,
            sysvals_out_d: 0,
            store_req_start: 0,
            store_req_end: 0,
            fs_inputs: [0; 128],
            fs_sysvals_d: [0; 8],
            fs_color_mask: 0,
        }
    }

    pub fn check(&self) -> Result<(), Error> {
        if self.gprs < 4
            || self.flags & !15 != 0
            || self
                .cb_sizes
                .iter()
                .any(|s| *s > MAX_CB_SIZE || *s % 4 != 0)
            || self
                .fs_inputs
                .iter()
                .chain(self.fs_sysvals_d.iter())
                .any(|x| *x > 3)
        {
            return Err(Error::Metadata);
        }
        if self.stage == Stage::Vertex {
            if self.flags & (FLAG_DEPTH | FLAG_SAMPLE_MASK | FLAG_KILL) != 0
                || self.fs_color_mask != 0
                || self
                    .fs_inputs
                    .iter()
                    .chain(self.fs_sysvals_d.iter())
                    .any(|x| *x != 0)
                || self.store_req_start > self.store_req_end
            {
                return Err(Error::Metadata);
            }
        } else if self.attr_in != [0; 4]
            || self.attr_out != [0; 4]
            || self.sysvals_out_ab != 0
            || self.sysvals_out_c != 0
            || self.sysvals_out_d != 0
            || self.sysvals_in_d != 0
            || self.store_req_start != 0
            || self.store_req_end != 0
            || self.sysvals_in_ab & (1 << 31) == 0
        {
            return Err(Error::Metadata);
        }
        Ok(())
    }

    /// Construct SPHv3 from metadata that has passed `validate`; no local/global
    /// memory, interlock, tessellation, geometry, or CRS spill is ever enabled.
    pub(crate) fn header(&self) -> [u32; 20] {
        let mut h = [0; 20];
        set_bits(
            &mut h,
            0,
            5,
            if self.stage == Stage::Vertex { 1 } else { 2 },
        );
        set_bits(&mut h, 5, 5, 3);
        set_bits(
            &mut h,
            10,
            4,
            if self.stage == Stage::Vertex { 1 } else { 5 },
        );
        set_bits(&mut h, 17, 4, 1);
        set_bits(&mut h, 27, 1, u32::from(self.flags & FLAG_FP64 != 0));
        set_bits(&mut h, 160, 32, self.sysvals_in_ab);
        if self.stage == Stage::Vertex {
            set_bits(&mut h, 140, 8, self.store_req_start as u32);
            set_bits(&mut h, 152, 8, self.store_req_end as u32);
            for i in 0..4 {
                set_bits(&mut h, 192 + i * 32, 32, self.attr_in[i]);
                set_bits(&mut h, 432 + i * 32, 32, self.attr_out[i]);
            }
            set_bits(&mut h, 336, 16, self.sysvals_in_c as u32);
            set_bits(&mut h, 392, 8, self.sysvals_in_d as u32);
            set_bits(&mut h, 400, 32, self.sysvals_out_ab);
            set_bits(&mut h, 576, 16, self.sysvals_out_c as u32);
            set_bits(&mut h, 632, 8, self.sysvals_out_d as u32);
        } else {
            set_bits(&mut h, 14, 1, 1);
            set_bits(&mut h, 15, 1, u32::from(self.flags & FLAG_KILL != 0));
            for i in 0..128 {
                set_bits(&mut h, 192 + i * 2, 2, self.fs_inputs[i] as u32);
            }
            set_bits(&mut h, 464, 16, self.sysvals_in_c as u32);
            for i in 0..8 {
                set_bits(&mut h, 560 + i * 2, 2, self.fs_sysvals_d[i] as u32);
            }
            set_bits(&mut h, 576, 32, self.fs_color_mask);
            set_bits(
                &mut h,
                608,
                1,
                u32::from(self.flags & FLAG_SAMPLE_MASK != 0),
            );
            set_bits(&mut h, 609, 1, u32::from(self.flags & FLAG_DEPTH != 0));
        }
        h
    }
}

fn set_bits(h: &mut [u32; 20], start: usize, count: usize, value: u32) {
    for i in 0..count {
        if value & (1 << i) != 0 {
            h[(start + i) / 32] |= 1 << ((start + i) % 32);
        }
    }
}

/// Authority supplied by the kernel after resolving upload/draw tokens. A
/// package cannot authorize resources by declaring its own larger limits.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub cb_sizes: [u32; 32],
    pub resource_mask: u64,
}

pub struct Program<'a> {
    pub metadata: Metadata,
    pub code: &'a [u8],
}

impl<'a> Program<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER_SIZE {
            return Err(Error::Truncated);
        }
        if &bytes[..8] != MAGIC || u16at(bytes, 10) as usize != HEADER_SIZE {
            return Err(Error::Format);
        }
        if u16at(bytes, 8) != VERSION {
            return Err(Error::Version);
        }
        let code_size = u32at(bytes, 12) as usize;
        if code_size == 0
            || code_size > MAX_CODE_SIZE
            || code_size % 32 != 0
            || bytes.len() != HEADER_SIZE + code_size
        {
            return Err(Error::Size);
        }
        let stage = match bytes[16] {
            0 => Stage::Vertex,
            1 => Stage::Fragment,
            _ => return Err(Error::Metadata),
        };
        let mut m = Metadata::new(stage);
        m.gprs = bytes[17];
        m.flags = u16at(bytes, 18);
        m.resource_mask = u64at(bytes, 24);
        for i in 0..32 {
            m.cb_sizes[i] = u32at(bytes, 32 + i * 4);
        }
        for i in 0..4 {
            m.attr_in[i] = u32at(bytes, 160 + i * 4);
            m.attr_out[i] = u32at(bytes, 176 + i * 4);
        }
        m.sysvals_in_ab = u32at(bytes, 192);
        m.sysvals_out_ab = u32at(bytes, 196);
        m.sysvals_in_c = u16at(bytes, 200);
        m.sysvals_out_c = u16at(bytes, 202);
        m.sysvals_in_d = bytes[204];
        m.sysvals_out_d = bytes[205];
        m.store_req_start = bytes[206];
        m.store_req_end = bytes[207];
        m.fs_inputs.copy_from_slice(&bytes[208..336]);
        m.fs_sysvals_d.copy_from_slice(&bytes[336..344]);
        m.fs_color_mask = u32at(bytes, 344);
        if bytes[20..24]
            .iter()
            .chain(bytes[348..HEADER_SIZE].iter())
            .any(|b| *b != 0)
        {
            return Err(Error::Format);
        }
        m.check()?;
        Ok(Self {
            metadata: m,
            code: &bytes[HEADER_SIZE..],
        })
    }

    pub fn encode_into(&self, bytes: &mut [u8]) -> Result<(), Error> {
        self.metadata.check()?;
        if self.code.is_empty()
            || self.code.len() > MAX_CODE_SIZE
            || self.code.len() % 32 != 0
            || bytes.len() != HEADER_SIZE + self.code.len()
        {
            return Err(Error::Size);
        }
        bytes.fill(0);
        bytes[..8].copy_from_slice(MAGIC);
        put16(bytes, 8, VERSION);
        put16(bytes, 10, HEADER_SIZE as u16);
        put32(bytes, 12, self.code.len() as u32);
        let m = &self.metadata;
        bytes[16] = m.stage as u8;
        bytes[17] = m.gprs;
        put16(bytes, 18, m.flags);
        put64(bytes, 24, m.resource_mask);
        for i in 0..32 {
            put32(bytes, 32 + i * 4, m.cb_sizes[i]);
        }
        for i in 0..4 {
            put32(bytes, 160 + i * 4, m.attr_in[i]);
            put32(bytes, 176 + i * 4, m.attr_out[i]);
        }
        put32(bytes, 192, m.sysvals_in_ab);
        put32(bytes, 196, m.sysvals_out_ab);
        put16(bytes, 200, m.sysvals_in_c);
        put16(bytes, 202, m.sysvals_out_c);
        bytes[204] = m.sysvals_in_d;
        bytes[205] = m.sysvals_out_d;
        bytes[206] = m.store_req_start;
        bytes[207] = m.store_req_end;
        bytes[208..336].copy_from_slice(&m.fs_inputs);
        bytes[336..344].copy_from_slice(&m.fs_sysvals_d);
        put32(bytes, 344, m.fs_color_mask);
        bytes[HEADER_SIZE..].copy_from_slice(self.code);
        Ok(())
    }
}
fn u16at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
pub(crate) fn u64at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
fn put16(b: &mut [u8], i: usize, v: u16) {
    b[i..i + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], i: usize, v: u32) {
    b[i..i + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], i: usize, v: u64) {
    b[i..i + 8].copy_from_slice(&v.to_le_bytes());
}

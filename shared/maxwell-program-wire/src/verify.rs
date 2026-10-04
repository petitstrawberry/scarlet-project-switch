// SPDX-License-Identifier: GPL-2.0-only
// Instruction encodings audited against Mesa's MIT-licensed
// src/nouveau/compiler/nak/sm50.rs at MESA_SHA (see lib.rs).
use crate::opcodes::{CbSource, find_alu_encoding};
use crate::{Error, FLAG_FP64, FLAG_KILL, Limits, MAX_CODE_SIZE, Metadata, Program, Stage, u64at};
use alloc::{vec, vec::Vec};

#[derive(Clone, Copy, Debug)]
pub struct Validated {
    pub header: [u32; 20],
    pub stage: Stage,
    pub gprs: u8,
    pub resource_mask: u64,
    pub cb_sizes: [u32; 32],
    pub max_control_depth: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Next,
    Exit,
    Branch(u16),
    Push(u8, u16),
    Pop(u8),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stack {
    entries: [u16; 16],
    len: u8,
}
impl Stack {
    const EMPTY: Self = Self {
        entries: [0; 16],
        len: 0,
    };
}

const fn mask(start: u8, len: u8) -> u64 {
    if len == 64 {
        u64::MAX
    } else {
        ((1u64 << len) - 1) << start
    }
}
fn field(w: u64, start: u8, len: u8) -> u64 {
    (w >> start) & ((1u64 << len) - 1)
}
fn exact(w: u64, opcode: u16, variables: u64, fixed_low: u64) -> bool {
    let fixed = ((opcode as u64) << 48 | fixed_low) & !variables;
    w & !variables == fixed
}
const PRED: u64 = mask(16, 4);
fn reg(w: u64, bit: u8, width: u8, m: &Metadata) -> Result<(), Error> {
    let r = field(w, bit, 8) as u16;
    if r == 255 || r + width as u16 <= m.gprs as u16 {
        Ok(())
    } else {
        Err(Error::Register)
    }
}
fn cb(w: u64, bit: u8, width: u32, m: &Metadata, limits: &Limits) -> Result<(), Error> {
    let offset = (field(w, bit, 14) as u32) * 4;
    let bank = field(w, bit + 14, 5) as usize;
    let end = offset.checked_add(width).ok_or(Error::ConstantBuffer)?;
    if end > m.cb_sizes[bank] || end > limits.cb_sizes[bank] {
        Err(Error::ConstantBuffer)
    } else {
        Ok(())
    }
}

/// Validate every instruction, including unreachable padding. Branch targets
/// cannot land on scheduler words. A second CFG pass proves typed reconvergence
/// stacks never underflow, merge ambiguously, or require local-memory spill.
pub fn validate(p: &Program<'_>, limits: &Limits) -> Result<Validated, Error> {
    p.metadata.check()?;
    if p.code.is_empty() || p.code.len() > MAX_CODE_SIZE || p.code.len() % 32 != 0 {
        return Err(Error::Size);
    }
    let m = &p.metadata;
    if m.resource_mask & !limits.resource_mask != 0
        || (0..32).any(|i| m.cb_sizes[i] > limits.cb_sizes[i])
    {
        return Err(Error::Resource);
    }
    let count = p.code.len() / 32 * 3;
    let mut flows = Vec::with_capacity(count);
    for bundle in 0..p.code.len() / 32 {
        let schedule = u64at(p.code, bundle * 32);
        if schedule >> 63 != 0 {
            return Err(Error::Scheduling);
        }
        for lane in 0..3 {
            let sched = (schedule >> (lane * 21)) & 0x1fffff;
            // Hardware has six scoreboard barriers; 7 means no barrier.
            if (sched >> 5) & 7 == 6 || (sched >> 8) & 7 == 6 {
                return Err(Error::Scheduling);
            }
            let ip = bundle * 32 + 8 + lane * 8;
            let w = u64at(p.code, ip);
            flows.push(decode(w, ip, p.code.len(), m, limits)?);
        }
    }
    let depth = prove_control(p.code, &flows)?;
    Ok(Validated {
        header: m.header(),
        stage: m.stage,
        gprs: m.gprs,
        resource_mask: m.resource_mask,
        cb_sizes: m.cb_sizes,
        max_control_depth: depth,
    })
}

fn decode(w: u64, ip: usize, size: usize, m: &Metadata, limits: &Limits) -> Result<Flow, Error> {
    if let Some(e) = find_alu_encoding(w) {
        for &(bit, width) in e.regs {
            reg(w, bit, width, m)?;
        }
        if e.fp64 && m.flags & FLAG_FP64 == 0 {
            return Err(Error::Metadata);
        }
        match e.cb {
            CbSource::None => (),
            CbSource::Word(bit) => cb(w, bit, 4, m, limits)?,
            CbSource::Double(bit) => cb(w, bit, 8, m, limits)?,
        }
        return Ok(Flow::Next);
    }
    // Pure scalar NOP/EXIT and structured reconvergence operations. Relative
    // offsets are signed24 byte offsets from the *next* instruction address.
    if exact(w, 0x50b0, PRED, 0xf00) {
        return Ok(Flow::Next);
    }
    if exact(w, 0xe300, PRED, 0xf) {
        return Ok(Flow::Exit);
    }
    for (opcode, kind) in [(0xe240, 0), (0xe290, 1), (0xe2a0, 2), (0xe2b0, 3)] {
        if exact(w, opcode, PRED | mask(20, 24), 0xf) {
            let raw = field(w, 20, 24) as i64;
            let rel = (raw << 40) >> 40;
            let target = ip as i64 + 8 + rel;
            if target < 8 || target >= size as i64 || target % 8 != 0 || target % 32 == 0 {
                return Err(Error::Branch);
            }
            let index = ((target as usize / 32) * 3 + (target as usize % 32) / 8 - 1) as u16;
            return Ok(if kind == 0 {
                Flow::Branch(index)
            } else {
                Flow::Push(kind, index)
            });
        }
    }
    for (opcode, kind) in [(0xf0f8, 1), (0xe340, 2), (0xe350, 3)] {
        if exact(w, opcode, PRED, 0xf) {
            return Ok(Flow::Pop(kind));
        }
    }
    if exact(w, 0xe330, PRED, 0xf) {
        if m.stage != Stage::Fragment || m.flags & FLAG_KILL == 0 {
            return Err(Error::Metadata);
        }
        return Ok(Flow::Next);
    }
    // Bound textures only: the immediate13-bit handle cannot be influenced by
    // a GPR or CB. The authorized descriptor table is generated per draw.
    let common = PRED | mask(0, 16) | mask(20, 15) | mask(36, 15);
    let textures = [
        (0x0380, common | mask(35, 1) | mask(54, 3), 0u8),
        (0xdc38, common | mask(35, 1) | mask(55, 1), 1),
        (0xc838, common | mask(54, 4), 2),
        (0xdf58, (common & !mask(50, 1)) | mask(35, 1), 3),
        (0xde38, (common & !mask(50, 1)) | mask(35, 1), 4),
    ];
    for (opcode, variables, kind) in textures {
        if exact(w, opcode, variables, 0) {
            let resource = field(w, 36, 13).checked_sub(8).ok_or(Error::Resource)?;
            if resource >= 64
                || m.resource_mask & (1 << resource) == 0
                || limits.resource_mask & (1 << resource) == 0
            {
                return Err(Error::Resource);
            }
            let dim = field(w, 28, 3);
            if dim == 5 || field(w, 31, 4) == 0 || kind == 2 && field(w, 54, 2) == 3 {
                return Err(Error::Instruction);
            }
            reg(w, 0, field(w, 31, 4).count_ones() as u8, m)?;
            reg(w, 8, 1, m)?;
            reg(w, 20, 1, m)?;
            return Ok(Flow::Next);
        }
    }
    // TXQ exposes dimensions/type/sample locations only, never opaque handles.
    if exact(
        w,
        0xdf48,
        PRED | mask(0, 16) | mask(22, 6) | mask(31, 4) | mask(36, 14),
        0,
    ) {
        if !matches!(field(w, 22, 6), 1 | 2 | 5) || field(w, 31, 4) == 0 {
            return Err(Error::Instruction);
        }
        let resource = field(w, 36, 13).checked_sub(8).ok_or(Error::Resource)?;
        if resource >= 64
            || m.resource_mask & (1 << resource) == 0
            || limits.resource_mask & (1 << resource) == 0
        {
            return Err(Error::Resource);
        }
        reg(w, 0, field(w, 31, 4).count_ones() as u8, m)?;
        reg(w, 8, 1, m)?;
        return Ok(Flow::Next);
    }
    // Immediate vertex/fragment IO. Dynamic/physical/patch accesses are absent
    // from this profile and rejected by their exact reserved-bit checks.
    for (opcode, write) in [(0xefd8, false), (0xeff0, true)] {
        if exact(
            w,
            opcode,
            PRED | mask(0, 8) | mask(20, 10) | mask(47, 2),
            0xff00 | (255u64 << 39) | if write { 1u64 << 32 } else { 0 },
        ) {
            if m.stage != Stage::Vertex {
                return Err(Error::Attribute);
            }
            let addr = field(w, 20, 10) as u16;
            let comps = field(w, 47, 2) as u8 + 1;
            reg(w, 0, comps, m)?;
            for i in 0..comps {
                let scalar = addr + i as u16 * 4;
                if !attribute(m, scalar, write, 0)
                    || write
                        && (scalar / 4 < m.store_req_start as u16
                            || scalar / 4 > m.store_req_end as u16)
                {
                    return Err(Error::Attribute);
                }
            }
            return Ok(Flow::Next);
        }
    }
    // IPA immediate attributes, no IDX addressing, and no dynamic offset. The
    // zero address GPR and discard predicate fields are prescribed by NAK.
    if exact(
        w,
        0xe000,
        PRED | mask(0, 8) | mask(20, 18) | mask(52, 4),
        0xff00 | (255u64 << 39) | (7u64 << 47),
    ) {
        if m.stage != Stage::Fragment || field(w, 52, 2) == 3 {
            return Err(Error::Attribute);
        }
        let addr = field(w, 28, 10) as u16;
        if !attribute(m, addr, false, field(w, 54, 2) as u8) {
            return Err(Error::Attribute);
        }
        reg(w, 0, 1, m)?;
        reg(w, 20, 1, m)?;
        return Ok(Flow::Next);
    }
    // Read only explicitly public, scalar system values. All other SR indices
    // (including internal warp addresses and opaque identifiers) are rejected.
    if exact(w, 0xf0c8, PRED | mask(0, 8) | mask(20, 8), 0) {
        let idx = field(w, 20, 8);
        // nak_private.h: LANE_ID, PRIM_TYPE, INVOCATION_ID, THREAD_KILL,
        // and lane masks. No virtual/physical warp or address-like values.
        if !matches!(idx, 0 | 0x10 | 0x11 | 0x13 | 0x38..=0x3c) {
            return Err(Error::Instruction);
        }
        reg(w, 0, 1, m)?;
        return Ok(Flow::Next);
    }
    // Fragment sample coverage/index/centroid readbacks have no memory operand.
    if exact(
        w,
        0xefe8,
        PRED | mask(0, 8) | mask(31, 3),
        0xff00 | (7u64 << 45),
    ) {
        if m.stage != Stage::Fragment || !matches!(field(w, 31, 3), 1..=5) {
            return Err(Error::Instruction);
        }
        reg(w, 0, 1, m)?;
        return Ok(Flow::Next);
    }
    if exact(w, 0x50d8, PRED | mask(0, 8) | mask(39, 4) | mask(45, 5), 0) {
        if field(w, 48, 2) > 2 {
            return Err(Error::Instruction);
        }
        reg(w, 0, 1, m)?;
        return Ok(Flow::Next);
    }
    // Every unspecified encoding fails closed. This includes LDC, all LD/ST,
    // surface atomics/stores, bindless textures, calls, indirect branches,
    // cache/barrier/membar operations, AL2P, ISBERD, and geometry output.
    Err(Error::Instruction)
}

fn attribute(m: &Metadata, addr: u16, write: bool, freq: u8) -> bool {
    if addr % 4 != 0 {
        return false;
    }
    if m.stage == Stage::Vertex {
        let (ab, c, d, attr) = if write {
            (
                m.sysvals_out_ab,
                m.sysvals_out_c,
                m.sysvals_out_d,
                &m.attr_out,
            )
        } else {
            (m.sysvals_in_ab, m.sysvals_in_c, m.sysvals_in_d, &m.attr_in)
        };
        if addr < 0x80 {
            ab & (1 << (addr / 4)) != 0
        } else if addr < 0x280 {
            let idx = (addr - 0x80) / 4;
            attr[idx as usize / 32] & (1 << (idx % 32)) != 0
        } else if (0x2c0..0x300).contains(&addr) {
            c & (1 << ((addr - 0x2c0) / 4)) != 0
        } else if (0x3a0..0x3c0).contains(&addr) {
            d & (1 << ((addr - 0x3a0) / 4)) != 0
        } else {
            false
        }
    } else {
        if write {
            return false;
        }
        if addr == 0x3fc {
            return freq == 2;
        } // FRONT_FACE has no SPH input-map bit.
        let imap = if (0x80..0x280).contains(&addr) {
            m.fs_inputs[(addr - 0x80) as usize / 4]
        } else if (0x3a0..0x3c0).contains(&addr) {
            m.fs_sysvals_d[(addr - 0x3a0) as usize / 4]
        } else {
            0
        };
        if addr < 0x80 {
            return m.sysvals_in_ab & (1 << (addr / 4)) != 0;
        }
        if (0x2c0..0x300).contains(&addr) {
            return m.sysvals_in_c & (1 << ((addr - 0x2c0) / 4)) != 0;
        }
        // Pass/PassMulW require a smooth input; Constant requires a flat input.
        imap != 0
            && if freq == 2 {
                imap == 1
            } else {
                imap == 2 || imap == 3
            }
    }
}

fn instruction_word(code: &[u8], index: usize) -> u64 {
    u64at(code, index / 3 * 32 + 8 + index % 3 * 8)
}
fn prove_control(code: &[u8], flows: &[Flow]) -> Result<u8, Error> {
    let mut states = vec![None; flows.len()];
    let mut work = Vec::with_capacity(flows.len());
    states[0] = Some(Stack::EMPTY);
    work.push(0usize);
    let mut maximum = 0;
    while let Some(ip) = work.pop() {
        let stack = states[ip].unwrap();
        let word = instruction_word(code, ip);
        let conditional = field(word, 16, 3) != 7;
        let never = field(word, 16, 4) == 15;
        if conditional || never {
            propagate(ip + 1, stack, &mut states, &mut work)?;
        }
        if never {
            continue;
        }
        match flows[ip] {
            Flow::Next => {
                if !conditional {
                    propagate(ip + 1, stack, &mut states, &mut work)?;
                }
            }
            Flow::Exit => (),
            Flow::Branch(target) => propagate(target as usize, stack, &mut states, &mut work)?,
            Flow::Push(kind, target) => {
                let mut next = stack;
                if next.len == 16 {
                    return Err(Error::ControlStack);
                }
                next.entries[next.len as usize] = (target << 2) | kind as u16;
                next.len += 1;
                maximum = maximum.max(next.len);
                propagate(ip + 1, next, &mut states, &mut work)?;
            }
            Flow::Pop(kind) => {
                let mut next = stack;
                let target = loop {
                    if next.len == 0 {
                        return Err(Error::ControlStack);
                    }
                    next.len -= 1;
                    let entry = next.entries[next.len as usize];
                    next.entries[next.len as usize] = 0;
                    let ty = (entry & 3) as u8;
                    if ty == kind {
                        break entry >> 2;
                    }
                    if kind == 1 || kind == 3 && ty != 1 || kind == 2 && ty == 2 {
                        return Err(Error::ControlStack);
                    }
                };
                propagate(target as usize, next, &mut states, &mut work)?;
            }
        }
    }
    Ok(maximum)
}
fn propagate(
    ip: usize,
    stack: Stack,
    states: &mut [Option<Stack>],
    work: &mut Vec<usize>,
) -> Result<(), Error> {
    let state = states.get_mut(ip).ok_or(Error::Fallthrough)?;
    match state {
        Some(old) if *old != stack => Err(Error::ControlStack),
        Some(_) => Ok(()),
        None => {
            *state = Some(stack);
            work.push(ip);
            Ok(())
        }
    }
}

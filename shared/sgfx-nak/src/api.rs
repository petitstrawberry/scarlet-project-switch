// Copyright © 2022 Collabora, Ltd.
// SPDX-License-Identifier: MIT
// SGFX adaptation: safe Rust compiler entrypoint without NIR/C/DRM.
use crate::ir::*;
use crate::sph;
use std::{
    env, fmt,
    panic::{self, AssertUnwindSafe},
    sync::OnceLock,
};

#[repr(u8)]
enum DebugFlags {
    Panic,
    Print,
    Serial,
    Spill,
    Annotate,
    NoUgpr,
    Cycles,
}

pub struct Debug {
    flags: u32,
}

impl Debug {
    fn new() -> Debug {
        let debug_var = "NAK_DEBUG";
        let debug_str = match env::var(debug_var) {
            Ok(s) => s,
            Err(_) => {
                return Debug { flags: 0 };
            }
        };

        let mut flags = 0;
        for flag in debug_str.split(',') {
            match flag.trim() {
                "panic" => flags |= 1 << DebugFlags::Panic as u8,
                "print" => flags |= 1 << DebugFlags::Print as u8,
                "serial" => flags |= 1 << DebugFlags::Serial as u8,
                "spill" => flags |= 1 << DebugFlags::Spill as u8,
                "annotate" => flags |= 1 << DebugFlags::Annotate as u8,
                "nougpr" => flags |= 1 << DebugFlags::NoUgpr as u8,
                "cycles" => flags |= 1 << DebugFlags::Cycles as u8,
                unk => eprintln!("Unknown NAK_DEBUG flag \"{}\"", unk),
            }
        }
        Debug { flags: flags }
    }
}

pub trait GetDebugFlags {
    fn debug_flags(&self) -> u32;

    fn panic(&self) -> bool {
        self.debug_flags() & (1 << DebugFlags::Panic as u8) != 0
    }

    fn print(&self) -> bool {
        self.debug_flags() & (1 << DebugFlags::Print as u8) != 0
    }

    fn serial(&self) -> bool {
        self.debug_flags() & (1 << DebugFlags::Serial as u8) != 0
    }

    fn spill(&self) -> bool {
        self.debug_flags() & (1 << DebugFlags::Spill as u8) != 0
    }

    fn annotate(&self) -> bool {
        self.debug_flags() & (1 << DebugFlags::Annotate as u8) != 0
    }

    fn no_ugpr(&self) -> bool {
        self.debug_flags() & (1 << DebugFlags::NoUgpr as u8) != 0
    }

    fn cycles(&self) -> bool {
        self.debug_flags() & (1 << DebugFlags::Cycles as u8) != 0
    }
}

pub static DEBUG: OnceLock<Debug> = OnceLock::new();

impl GetDebugFlags for OnceLock<Debug> {
    fn debug_flags(&self) -> u32 {
        self.get_or_init(Debug::new).flags
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsStage {
    Vertex,
    Fragment,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompileError {
    UnsupportedShaderModel(u8),
    UnsupportedStage,
    InvalidIr(&'static str),
    Backend(String),
}
impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedShaderModel(sm) => {
                write!(f, "unsupported SM{sm}; supported: SM50, SM52")
            }
            Self::UnsupportedStage => write!(f, "only vertex and fragment shaders are supported"),
            Self::InvalidIr(reason) => write!(f, "invalid NAK graphics IR: {reason}"),
            Self::Backend(reason) => write!(f, "NAK backend failed: {reason}"),
        }
    }
}
impl std::error::Error for CompileError {}

#[derive(Debug)]
pub struct ShaderMetadata {
    pub stage: GraphicsStage,
    pub sm: u8,
    /// Hardware register allocation including the minimum required by launch.
    pub num_gprs: u8,
    pub num_control_barriers: u8,
    pub scratch_bytes: u32,
    pub crs_bytes: u32,
    /// Exact final backend metadata and caller-specified graphics IO reflection.
    pub info: ShaderInfo,
}

#[derive(Debug)]
pub struct CompiledShader {
    /// Maxwell SASS, including one scheduling word per group of three instructions.
    pub code: Vec<u32>,
    /// Maxwell SPHv3, twenty 32-bit words (80 bytes).
    pub header: [u32; 20],
    pub metadata: ShaderMetadata,
    pub assembly: String,
}

pub fn graphics_shader_info(stage: GraphicsStage) -> ShaderInfo {
    let (stage, io) = match stage {
        GraphicsStage::Vertex => (
            ShaderStageInfo::Vertex(VertexShaderInfo {
                isbe_space_sharing_enable: false,
            }),
            ShaderIoInfo::Vtg(VtgIoInfo {
                sysvals_in: SysValInfo::default(),
                sysvals_in_d: 0,
                sysvals_out: SysValInfo::default(),
                sysvals_out_d: 0,
                attr_in: [0; 4],
                attr_out: [0; 4],
                store_req_start: u8::MAX,
                store_req_end: 0,
                clip_enable: 0,
                cull_enable: 0,
                xfb: None,
            }),
        ),
        GraphicsStage::Fragment => (
            ShaderStageInfo::Fragment(FragmentShaderInfo {
                uses_kill: false,
                does_interlock: false,
                post_depth_coverage: false,
                early_fragment_tests: false,
                uses_sample_shading: false,
            }),
            ShaderIoInfo::Fragment(FragmentIoInfo {
                // Mandatory fragment input bit: omitting it causes a hardware trap.
                sysvals_in: SysValInfo { ab: 1 << 31, c: 0 },
                sysvals_in_d: [sph::PixelImap::Unused; 8],
                attr_in: [sph::PixelImap::Unused; 128],
                barycentric_attr_in: [0; 4],
                reads_sample_mask: false,
                writes_color: 0,
                writes_sample_mask: false,
                writes_depth: false,
            }),
        ),
    };
    ShaderInfo {
        max_warps_per_sm: 0,
        num_gprs: 0,
        num_control_barriers: 0,
        num_instrs: 0,
        num_static_cycles: 0,
        num_spills_to_mem: 0,
        num_fills_from_mem: 0,
        num_spills_to_reg: 0,
        num_fills_from_reg: 0,
        slm_size: 0,
        max_crs_depth: 0,
        uses_global_mem: false,
        writes_global_mem: false,
        uses_fp64: false,
        stage,
        io,
    }
}

fn validate(s: &Shader<'_>) -> Result<GraphicsStage, CompileError> {
    if !matches!(s.sm.sm(), 50 | 52) {
        return Err(CompileError::UnsupportedShaderModel(s.sm.sm()));
    }
    let stage = match (&s.info.stage, &s.info.io) {
        (ShaderStageInfo::Vertex(_), ShaderIoInfo::Vtg(io)) => {
            if io.xfb.is_some() {
                return Err(CompileError::InvalidIr(
                    "transform feedback is not supported",
                ));
            }
            GraphicsStage::Vertex
        }
        (ShaderStageInfo::Fragment(info), ShaderIoInfo::Fragment(io)) => {
            if info.does_interlock {
                return Err(CompileError::InvalidIr(
                    "fragment interlock is not supported",
                ));
            }
            if io.sysvals_in.ab & (1 << 31) == 0 {
                return Err(CompileError::InvalidIr(
                    "mandatory fragment input bit 31 is missing",
                ));
            }
            GraphicsStage::Fragment
        }
        (ShaderStageInfo::Vertex(_) | ShaderStageInfo::Fragment(_), _) => {
            return Err(CompileError::InvalidIr(
                "stage and IO metadata do not match",
            ));
        }
        _ => return Err(CompileError::UnsupportedStage),
    };
    if s.functions.len() != 1 {
        return Err(CompileError::InvalidIr("exactly one function is required"));
    }
    if s.functions[0].blocks.len() == 0 {
        return Err(CompileError::InvalidIr("function is empty"));
    }
    if s.functions[0].blocks.iter().any(|b| b.instrs.is_empty()) {
        return Err(CompileError::InvalidIr("empty basic block"));
    }
    validate_io(s)?;
    Ok(stage)
}

fn attr_mask_contains(ab: u32, c: u16, d: u8, attr: &[u32; 4], addr: u16) -> bool {
    match addr {
        0x000..0x080 => ab & (1 << (addr / 4)) != 0,
        0x080..0x280 => {
            let idx = usize::from((addr - 0x80) / 4);
            attr[idx / 32] & (1 << (idx % 32)) != 0
        }
        0x2c0..0x300 => c & (1 << ((addr - 0x2c0) / 4)) != 0,
        0x3a0..0x3c0 => d & (1 << ((addr - 0x3a0) / 4)) != 0,
        _ => false,
    }
}

fn fragment_attr_declared(io: &FragmentIoInfo, addr: u16) -> bool {
    match addr {
        0x000..0x080 => io.sysvals_in.ab & (1 << (addr / 4)) != 0,
        0x080..0x280 => io.attr_in[usize::from((addr - 0x80) / 4)] != sph::PixelImap::Unused,
        0x2c0..0x300 => io.sysvals_in.c & (1 << ((addr - 0x2c0) / 4)) != 0,
        0x3a0..0x3c0 => io.sysvals_in_d[usize::from((addr - 0x3a0) / 4)] != sph::PixelImap::Unused,
        _ => false,
    }
}

fn attr_addresses(addr: u16, comps: u8) -> Result<std::ops::Range<u16>, CompileError> {
    if addr % 4 != 0 || !(1..=4).contains(&comps) {
        return Err(CompileError::InvalidIr(
            "attribute address or component count is invalid",
        ));
    }
    let end = addr
        .checked_add(u16::from(comps) * 4)
        .ok_or(CompileError::InvalidIr("attribute address overflows"))?;
    Ok(addr..end)
}

/// Check that the program cannot access an attribute or output omitted from its SPH.
/// Unused declared inputs may remain after DCE, as in upstream Mesa.
fn validate_io(s: &Shader<'_>) -> Result<(), CompileError> {
    for f in &s.functions {
        for b in &f.blocks {
            for instr in &b.instrs {
                match (&s.info.io, &instr.op) {
                    (ShaderIoInfo::Vtg(io), Op::ALd(op)) => {
                        if op.patch || op.phys || op.output || !op.offset.is_zero() {
                            return Err(CompileError::InvalidIr(
                                "dynamic, patch or physical vertex input is not supported",
                            ));
                        }
                        for addr in attr_addresses(op.addr, op.comps)?.step_by(4) {
                            if !attr_mask_contains(
                                io.sysvals_in.ab,
                                io.sysvals_in.c,
                                io.sysvals_in_d,
                                &io.attr_in,
                                addr,
                            ) {
                                return Err(CompileError::InvalidIr(
                                    "vertex input is absent from IO reflection",
                                ));
                            }
                        }
                    }
                    (ShaderIoInfo::Vtg(io), Op::ASt(op)) => {
                        if op.patch || op.phys || !op.offset.is_zero() {
                            return Err(CompileError::InvalidIr(
                                "dynamic, patch or physical vertex output is not supported",
                            ));
                        }
                        for addr in attr_addresses(op.addr, op.comps)?.step_by(4) {
                            if !attr_mask_contains(
                                io.sysvals_out.ab,
                                io.sysvals_out.c,
                                io.sysvals_out_d,
                                &io.attr_out,
                                addr,
                            ) {
                                return Err(CompileError::InvalidIr(
                                    "vertex output is absent from IO reflection",
                                ));
                            }
                            if addr / 4 < u16::from(io.store_req_start)
                                || addr / 4 > u16::from(io.store_req_end)
                            {
                                return Err(CompileError::InvalidIr(
                                    "vertex output is outside its store request range",
                                ));
                            }
                        }
                    }
                    (ShaderIoInfo::Fragment(io), Op::Ipa(op)) => {
                        // FACE is a hardware pseudo-attribute, read via IPA.CONSTANT,
                        // and intentionally has no SPH input-map bit (nak_private.h).
                        let face = op.addr == 0x3fc && op.freq == InterpFreq::Constant;
                        if op.addr % 4 != 0 || (!face && !fragment_attr_declared(io, op.addr)) {
                            return Err(CompileError::InvalidIr(
                                "fragment input is absent from IO reflection",
                            ));
                        }
                    }
                    (ShaderIoInfo::Fragment(io), Op::RegOut(op)) => {
                        let targets = (0..8)
                            .filter(|i| io.writes_color & (0xf << (i * 4)) != 0)
                            .count();
                        let count = targets * 4
                            + usize::from(io.writes_sample_mask || io.writes_depth)
                            + usize::from(io.writes_depth);
                        if op.srcs.len() != count {
                            return Err(CompileError::InvalidIr(
                                "fragment register outputs disagree with IO reflection",
                            ));
                        }
                    }
                    (ShaderIoInfo::Fragment(_), Op::ASt(_) | Op::ALd(_))
                    | (ShaderIoInfo::Vtg(_), Op::Ipa(_) | Op::RegOut(_) | Op::Kill(_)) => {
                        return Err(CompileError::InvalidIr(
                            "attribute operation is invalid for the shader stage",
                        ));
                    }
                    (_, Op::Kill(_)) => {
                        if !matches!(&s.info.stage, ShaderStageInfo::Fragment(info) if info.uses_kill)
                        {
                            return Err(CompileError::InvalidIr(
                                "fragment discard is absent from stage reflection",
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

macro_rules! pass {
    ($s:expr, $pass:ident) => {
        $s.$pass();
        if DEBUG.print() {
            eprintln!("NAK IR after {}:\n{}", stringify!($pass), $s);
        }
    };
}

/// Optimize, legalize, allocate registers, schedule and encode pure NAK graphics IR.
///
/// The frontend must populate IO metadata and structured control-flow depth.
/// Unsupported IR is returned as an error; no generated program is fabricated.
pub fn compile_graphics_ir(mut s: Shader<'_>) -> Result<CompiledShader, CompileError> {
    let stage = validate(&s)?;
    panic::catch_unwind(AssertUnwindSafe(|| {
        pass!(s, opt_bar_prop);
        pass!(s, opt_uniform_instrs);
        pass!(s, opt_copy_prop);
        pass!(s, opt_prmt);
        pass!(s, opt_lop);
        pass!(s, opt_copy_prop);
        pass!(s, opt_dce);
        pass!(s, opt_out);
        pass!(s, legalize);
        pass!(s, opt_dce);
        pass!(s, opt_instr_sched_prepass);
        pass!(s, assign_regs);
        pass!(s, lower_par_copies);
        pass!(s, lower_copy_swap);
        pass!(s, opt_crs);
        s.remove_annotations();
        pass!(s, opt_instr_sched_postpass);
        pass!(s, calc_instr_deps);
        s.gather_info();
        let code = s.sm.encode_shader(&s);
        if code.is_empty() || code.len() % 8 != 0 {
            return Err(CompileError::InvalidIr(
                "invalid Maxwell SASS bundle length",
            ));
        }
        let sph = sph::encode_header(s.sm, &s.info, None);
        let mut header = [0; 20];
        header.copy_from_slice(&sph[..20]);
        let assembly = s.to_string();
        let metadata = ShaderMetadata {
            stage,
            sm: s.sm.sm(),
            num_gprs: (u32::from(s.info.num_gprs) + s.sm.hw_reserved_gprs())
                .max(4)
                .try_into()
                .unwrap(),
            num_control_barriers: s.info.num_control_barriers,
            scratch_bytes: s.info.slm_size.next_multiple_of(16),
            crs_bytes: s.sm.crs_size(s.info.max_crs_depth),
            info: s.info,
        };
        Ok(CompiledShader {
            code,
            header,
            metadata,
            assembly,
        })
    }))
    .unwrap_or_else(|payload| {
        let reason = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "internal compiler panic".to_owned());
        Err(CompileError::Backend(reason))
    })
}

// SPDX-License-Identifier: MIT
//! Pure-Rust, pinned Mesa NAK backend for Maxwell graphics shaders.
#![allow(dead_code, non_snake_case)]

pub use compiler;
pub mod api;
mod assign_regs;
mod bindings;
pub mod builder;
mod calc_instr_deps;
mod const_tracker;
pub mod ir;
mod legalize;
mod liveness;
mod lower_copy_swap;
mod lower_par_copies;
mod opt_bar_prop;
mod opt_copy_prop;
mod opt_crs;
mod opt_dce;
mod opt_instr_sched_common;
mod opt_instr_sched_postpass;
mod opt_instr_sched_prepass;
mod opt_jump_thread;
mod opt_lop;
mod opt_out;
mod opt_prmt;
mod opt_uniform_instrs;
mod reg_tracker;
mod repair_ssa;
mod sm50;
pub mod sph;
mod spill_values;
mod ssa_value;
mod to_cssa;

pub use api::{
    CompileError, CompiledShader, GraphicsStage, ShaderMetadata, compile_graphics_ir,
    graphics_shader_info,
};

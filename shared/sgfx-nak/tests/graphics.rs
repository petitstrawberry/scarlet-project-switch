// SPDX-License-Identifier: MIT
use sgfx_nak::{compiler::cfg::CFGBuilder, ir::*, sph::PixelImap, *};
use std::collections::hash_map::RandomState;

fn single_block(alloc: SSAValueAllocator, instrs: Vec<Instr>) -> Function {
    let mut cfg = CFGBuilder::<u32, BasicBlock, RandomState>::new();
    cfg.add_node(
        0,
        BasicBlock {
            label: LabelAllocator::new().alloc(),
            uniform: true,
            instrs,
        },
    );
    Function {
        ssa_alloc: alloc,
        phi_alloc: PhiAllocator::new(),
        blocks: cfg.as_cfg(false),
    }
}
fn vertex(sm: &ShaderModelInfo) -> Shader<'_> {
    let mut alloc = SSAValueAllocator::new();
    let pos = alloc.alloc_vec(RegFile::GPR, 4);
    let instrs = vec![
        OpALd {
            dst: pos.clone().into(),
            vtx: 0.into(),
            offset: 0.into(),
            addr: 0x80,
            comps: 4,
            patch: false,
            output: false,
            phys: false,
        }
        .into(),
        OpASt {
            vtx: 0.into(),
            offset: 0.into(),
            data: pos.into(),
            addr: 0x70,
            comps: 4,
            patch: false,
            phys: false,
        }
        .into(),
        OpExit {}.into(),
    ];
    let mut info = graphics_shader_info(GraphicsStage::Vertex);
    if let ShaderIoInfo::Vtg(io) = &mut info.io {
        io.mark_attrs_read(0x80..0x90);
        io.mark_attrs_written(0x70..0x80);
        io.mark_store_req(0x70..0x80);
    }
    Shader {
        sm,
        info,
        functions: vec![single_block(alloc, instrs)],
    }
}
fn fragment(sm: &ShaderModelInfo) -> Shader<'_> {
    let mut alloc = SSAValueAllocator::new();
    let mut instrs = Vec::new();
    let mut srcs = Vec::new();
    let mut info = graphics_shader_info(GraphicsStage::Fragment);
    for addr in (0x80..0x90).step_by(4) {
        let dst = alloc.alloc(RegFile::GPR);
        instrs.push(
            OpIpa {
                dst: dst.into(),
                addr,
                freq: InterpFreq::Pass,
                loc: InterpLoc::Default,
                inv_w: 0.into(),
                offset: 0.into(),
            }
            .into(),
        );
        srcs.push(dst.into());
        if let ShaderIoInfo::Fragment(io) = &mut info.io {
            io.mark_attr_read(addr, PixelImap::ScreenLinear);
        }
    }
    if let ShaderIoInfo::Fragment(io) = &mut info.io {
        io.writes_color = 0xf;
    }
    instrs.push(OpRegOut { srcs }.into());
    instrs.push(OpExit {}.into());
    Shader {
        sm,
        info,
        functions: vec![single_block(alloc, instrs)],
    }
}

#[test]
fn sm52_vertex_passthrough() {
    let sm = ShaderModelInfo::new(52, 64);
    let bin = compile_graphics_ir(vertex(&sm)).unwrap();
    // Pinned upstream SM50 encoder: ALD, AST, EXIT, schedule words and padding.
    assert_eq!(
        bin.code,
        [
            0x1c400706, 0x003fb401, 0x0807ff00, 0xefd9ff80, 0x0707ff00, 0xeff1ff81, 0x0007000f,
            0xe3000000, 0xfc0007e2, 0x001f8000, 0x00070f00, 0x50b00000, 0x00070f00, 0x50b00000,
            0x00070f00, 0x50b00000,
        ]
    );
    assert_eq!(
        bin.header,
        [
            0x02020461, 0, 0, 0, 0x1f01c000, 0, 0xf, 0, 0, 0, 0, 0, 0, 0xf000, 0, 0, 0, 0, 0, 0,
        ]
    );
    assert_eq!(bin.metadata.stage, GraphicsStage::Vertex);
    assert_eq!(bin.metadata.sm, 52);
    // Upstream register allocation reserves at least 24 GPRs for Maxwell.
    assert_eq!(bin.metadata.num_gprs, 24);
    assert_eq!(bin.metadata.scratch_bytes, 0);
    assert_eq!(bin.metadata.crs_bytes, 0);
    assert_eq!(bin.metadata.info.num_instrs, 4);
    assert!(bin.assembly.contains("ald"));
    assert!(bin.assembly.contains("ast"));
}
#[test]
fn sm52_fragment_passthrough() {
    let sm = ShaderModelInfo::new(52, 64);
    let bin = compile_graphics_ir(fragment(&sm)).unwrap();
    assert_eq!(
        bin.code,
        [
            0xe4200701, 0x001d0400, 0x0ff7ff00, 0xe003ff88, 0x4ff7ff01, 0xe003ff88, 0x8ff7ff02,
            0xe003ff88, 0xfda00762, 0x001f880f, 0xcff7ff03, 0xe003ff88, 0x0007000f, 0xe3000000,
            0x00070f00, 0x50b00000,
        ]
    );
    assert_eq!(
        bin.header,
        [
            0x25462, 0, 0, 0, 0, 0x80000000, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xf, 0,
        ]
    );
    assert_eq!(bin.metadata.stage, GraphicsStage::Fragment);
    assert_eq!(bin.metadata.sm, 52);
    assert_eq!(bin.metadata.num_gprs, 24);
    assert_eq!(bin.metadata.scratch_bytes, 0);
    assert_eq!(bin.metadata.crs_bytes, 0);
    assert_eq!(bin.header[5], 1 << 31);
    assert_eq!(bin.header[18], 0xf);
}
#[test]
fn reject_unsupported_shader_model() {
    let sm = ShaderModelInfo::new(70, 64);
    assert_eq!(
        compile_graphics_ir(vertex(&sm)).unwrap_err(),
        CompileError::UnsupportedShaderModel(70)
    );
}
#[test]
fn reject_compute_stage() {
    let sm = ShaderModelInfo::new(52, 64);
    let mut shader = vertex(&sm);
    shader.info.stage = ShaderStageInfo::Compute(ComputeShaderInfo {
        local_size: [1; 3],
        smem_size: 0,
    });
    shader.info.io = ShaderIoInfo::None;
    assert_eq!(
        compile_graphics_ir(shader).unwrap_err(),
        CompileError::UnsupportedStage
    );
}

#[test]
fn sm50_and_sm52_encode_basic_graphics_identically() {
    let sm50 = ShaderModelInfo::new(50, 64);
    let sm52 = ShaderModelInfo::new(52, 64);
    let a = compile_graphics_ir(vertex(&sm50)).unwrap();
    let b = compile_graphics_ir(vertex(&sm52)).unwrap();
    assert_eq!(a.code, b.code);
    assert_eq!(a.header, b.header);
    assert_eq!(a.metadata.sm, 50);
    let a = compile_graphics_ir(fragment(&sm50)).unwrap();
    let b = compile_graphics_ir(fragment(&sm52)).unwrap();
    assert_eq!(a.code, b.code);
    assert_eq!(a.header, b.header);
}

#[test]
fn fragment_sph_matches_pinned_legacy_mesa_fixed_pack() {
    let sm = ShaderModelInfo::new(52, 64);
    let mut info = graphics_shader_info(GraphicsStage::Fragment);
    if let ShaderIoInfo::Fragment(io) = &mut info.io {
        io.writes_color = 0xf;
    }
    let instrs = vec![
        OpRegOut {
            srcs: vec![0.into(), 0.into(), 0.into(), 1.0f32.into()],
        }
        .into(),
        OpExit {}.into(),
    ];
    let bin = compile_graphics_ir(Shader {
        sm: &sm,
        info,
        functions: vec![single_block(SSAValueAllocator::new(), instrs)],
    })
    .unwrap();
    let fixture = include_bytes!("fixtures/fs_solid.header.bin");
    let bytes: Vec<u8> = bin.header.iter().flat_map(|x| x.to_le_bytes()).collect();
    assert_eq!(bytes, fixture);
}

#[test]
fn front_face_constant_pseudo_attribute_does_not_need_sph_input_bit() {
    let sm = ShaderModelInfo::new(52, 64);
    let mut shader = fragment(&sm);
    let f = &mut shader.functions[0];
    let dst = f.ssa_alloc.alloc(RegFile::GPR);
    f.blocks[0].instrs[0] = OpIpa {
        dst: dst.into(),
        addr: 0x3fc,
        freq: InterpFreq::Constant,
        loc: InterpLoc::Default,
        inv_w: 0.into(),
        offset: 0.into(),
    }
    .into();
    // Use FACE in the output; otherwise DCE would remove the pseudo-attribute.
    if let Op::RegOut(out) = &mut f.blocks[0].instrs[4].op {
        out.srcs[0] = dst.into();
    }
    let bin = compile_graphics_ir(shader).unwrap();
    assert!(bin.assembly.contains("ipa.constant a[0x3fc]"));
}

#[test]
fn discard_only_fragment_is_valid() {
    let sm = ShaderModelInfo::new(52, 64);
    let mut info = graphics_shader_info(GraphicsStage::Fragment);
    if let ShaderStageInfo::Fragment(stage) = &mut info.stage {
        stage.uses_kill = true;
    }
    let instrs = vec![OpKill {}.into(), OpExit {}.into()];
    let bin = compile_graphics_ir(Shader {
        sm: &sm,
        info,
        functions: vec![single_block(SSAValueAllocator::new(), instrs)],
    })
    .unwrap();
    assert!(bin.assembly.contains("kill"));
    assert_eq!(bin.header[18], 0);
    assert_ne!(bin.header[0] & (1 << 15), 0);
}

#[test]
fn reject_reflection_mismatch() {
    let sm = ShaderModelInfo::new(52, 64);
    let mut shader = vertex(&sm);
    if let ShaderIoInfo::Vtg(io) = &mut shader.info.io {
        io.attr_in = [0; 4];
    }
    assert_eq!(
        compile_graphics_ir(shader).unwrap_err(),
        CompileError::InvalidIr("vertex input is absent from IO reflection")
    );
    let mut shader = fragment(&sm);
    if let ShaderIoInfo::Fragment(io) = &mut shader.info.io {
        io.writes_depth = true;
    }
    assert_eq!(
        compile_graphics_ir(shader).unwrap_err(),
        CompileError::InvalidIr("fragment register outputs disagree with IO reflection")
    );
    let mut shader = fragment(&sm);
    if let ShaderIoInfo::Fragment(io) = &mut shader.info.io {
        io.sysvals_in.ab = 0;
    }
    assert_eq!(
        compile_graphics_ir(shader).unwrap_err(),
        CompileError::InvalidIr("mandatory fragment input bit 31 is missing")
    );
}

#[test]
fn spills_and_control_stack_are_reflected_in_launch_metadata() {
    let sm = ShaderModelInfo::new(52, 64);
    let mut alloc = SSAValueAllocator::new();
    let mut values = Vec::new();
    let mut instrs = Vec::new();
    let mut info = graphics_shader_info(GraphicsStage::Vertex);
    info.max_crs_depth = 32;
    // Keep 280 scalar values live across the loads, exceeding Maxwell's
    // 255 GPRs. Every loaded value is then consumed by an attribute store.
    for i in 0..70 {
        let value = alloc.alloc_vec(RegFile::GPR, 4);
        let addr = 0x80 + (i % 32) * 16;
        instrs.push(
            OpALd {
                dst: value.clone().into(),
                vtx: 0.into(),
                offset: 0.into(),
                addr,
                comps: 4,
                patch: false,
                output: false,
                phys: false,
            }
            .into(),
        );
        values.push((value, addr));
        if let ShaderIoInfo::Vtg(io) = &mut info.io {
            io.mark_attrs_read(addr..addr + 16);
        }
    }
    for (value, addr) in values {
        instrs.push(
            OpASt {
                vtx: 0.into(),
                offset: 0.into(),
                data: value.into(),
                addr,
                comps: 4,
                patch: false,
                phys: false,
            }
            .into(),
        );
        if let ShaderIoInfo::Vtg(io) = &mut info.io {
            io.mark_attrs_written(addr..addr + 16);
            io.mark_store_req(addr..addr + 16);
        }
    }
    instrs.push(OpExit {}.into());
    let bin = compile_graphics_ir(Shader {
        sm: &sm,
        info,
        functions: vec![single_block(alloc, instrs)],
    })
    .unwrap();
    assert!(bin.metadata.scratch_bytes > 0);
    assert_eq!(bin.metadata.scratch_bytes % 16, 0);
    assert!(bin.metadata.info.num_spills_to_mem > 0);
    assert!(bin.metadata.info.num_fills_from_mem > 0);
    assert_eq!(bin.metadata.crs_bytes, 1024);
    assert!(bin.assembly.contains("st.local"));
    assert!(bin.assembly.contains("ld.local"));
    // SPHv3 word one holds the low 24 bits of shader local memory size.
    assert_eq!(bin.header[1] & 0x00ff_ffff, bin.metadata.scratch_bytes);
}

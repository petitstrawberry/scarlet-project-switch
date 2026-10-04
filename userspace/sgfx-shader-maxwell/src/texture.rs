//! TGSI texture operations lowered to Maxwell's vector texture instructions.

use sgfx_nak::ir::*;

struct Target {
    dim: TexDim,
    spatial_components: usize,
    layer: Option<usize>,
    comparator: Option<usize>,
    promote_1d: bool,
    buffer: bool,
    rectangle: bool,
}

impl Target {
    fn parse(name: &str) -> Result<Self, String> {
        let (dim, spatial_components, layer, comparator, promote_1d) = match name {
            // The backend stores 1D images as height-one 2D images.
            "1D" => (TexDim::_2D, 2, None, None, true),
            "1D_ARRAY" => (TexDim::Array2D, 2, Some(1), None, true),
            "SHADOW1D" => (TexDim::_2D, 2, None, Some(2), true),
            "SHADOW1D_ARRAY" => (TexDim::Array2D, 2, Some(1), Some(2), true),
            "2D" | "RECT" => (TexDim::_2D, 2, None, None, false),
            "2D_ARRAY" => (TexDim::Array2D, 2, Some(2), None, false),
            "SHADOW2D" | "SHADOWRECT" => (TexDim::_2D, 2, None, Some(2), false),
            "SHADOW2D_ARRAY" => (TexDim::Array2D, 2, Some(2), Some(3), false),
            "3D" => (TexDim::_3D, 3, None, None, false),
            "CUBE" => (TexDim::Cube, 3, None, None, false),
            "CUBE_ARRAY" => (TexDim::ArrayCube, 3, Some(3), None, false),
            "SHADOWCUBE" => (TexDim::Cube, 3, None, Some(3), false),
            "BUFFER" => (TexDim::_1D, 1, None, None, false),
            _ => return Err(format!("unsupported TGSI texture target {name}")),
        };
        Ok(Self {
            dim,
            spatial_components,
            layer,
            comparator,
            promote_1d,
            buffer: name == "BUFFER",
            rectangle: matches!(name, "RECT" | "SHADOWRECT"),
        })
    }

    fn cube(&self) -> bool {
        matches!(self.dim, TexDim::Cube | TexDim::ArrayCube)
    }
}

fn fmax(builder: &mut SSAInstrBuilder<'_>, x: Src, y: Src) -> SSAValue {
    let dst = builder.alloc_ssa(RegFile::GPR);
    builder.push_op(OpFMnMx {
        dst: dst.into(),
        srcs: [x, y],
        min: false.into(),
        ftz: false,
    });
    dst
}

fn fmul(builder: &mut SSAInstrBuilder<'_>, x: Src, y: Src) -> SSAValue {
    let dst = builder.alloc_ssa(RegFile::GPR);
    builder.push_op(OpFMul {
        dst: dst.into(),
        srcs: [x, y],
        saturate: false,
        rnd_mode: FRndMode::NearestEven,
        ftz: false,
        dnz: false,
    });
    dst
}

fn array_layer(builder: &mut SSAInstrBuilder<'_>, layer: Src, fetch: bool) -> Src {
    let layer = if fetch {
        // TXF takes an integer layer, unlike the sampled texture operations.
        layer
    } else {
        // Match NAK's NIR lowering: floor(max(layer + 0.5, 0)).
        let rounded = builder.fadd(layer, 0.5f32.into());
        let positive = fmax(builder, rounded.into(), 0.0f32.into());
        let integer = builder.alloc_ssa(RegFile::GPR);
        builder.push_op(OpF2I {
            dst: integer.into(),
            src: positive.into(),
            src_type: FloatType::F32,
            dst_type: IntType::U32,
            rnd_mode: FRndMode::Zero,
            ftz: false,
        });
        integer.into()
    };
    // The array index occupies the low 16 bits of its source register.
    builder
        .imnmx(
            IntCmpType::U32,
            layer,
            u32::from(u16::MAX).into(),
            true.into(),
        )
        .into()
}

fn source_vector(builder: &mut SSAInstrBuilder<'_>, sources: Vec<Src>) -> Src {
    if sources.is_empty() {
        return Src::ZERO;
    }
    let vector = builder.alloc_ssa_vec(RegFile::GPR, sources.len() as u8);
    for (dst, src) in vector.iter().zip(sources) {
        builder.copy_to((*dst).into(), src);
    }
    vector.into()
}

/// Emit a TGSI TEX, TXB, TXL, TXF or TXQ using a bound texture/sampler handle.
///
/// `slot` is the hardware bound-handle index: its descriptor must combine the
/// texture and sampler selected by the TGSI sampler register. Coordinate
/// components are already swizzled; TXB/TXL use W for bias/LOD and TXF uses W
/// for the integer mip level; TXQ uses X for the integer mip level. Unsupported
/// TGSI target/opcode combinations fail before adding instructions to the builder.
pub fn build_texture(
    builder: &mut SSAInstrBuilder<'_>,
    opcode: &str,
    target: &str,
    slot: u32,
    mut coords: [Src; 4],
) -> Result<SSARef, String> {
    if !matches!(builder.sm(), 50 | 52) {
        return Err("TGSI texture lowering requires Maxwell SM50 or SM52".into());
    }
    if slot >= 1 << 13 {
        return Err(format!(
            "texture handle slot {slot} exceeds Maxwell's 13-bit bound index"
        ));
    }
    if !matches!(opcode, "TEX" | "TXB" | "TXL" | "TXF" | "TXQ") {
        return Err(format!("unsupported TGSI texture opcode {opcode}"));
    }
    let target_info = Target::parse(target)?;
    if opcode == "TXQ" {
        let level = source_vector(builder, vec![coords[0].clone()]);
        let dst = builder.alloc_ssa_vec(RegFile::GPR, 4);
        builder.push_op(OpTxq {
            dsts: [dst.clone().into(), Dst::None],
            tex: TexRef::Bound(slot as u16),
            src: level,
            query: TexQuery::Dimension,
            nodep: false,
            channel_mask: ChannelMask::new(0xf),
        });
        // Dimension queries infer the image type from its descriptor, rather
        // than from an instruction dimension field. The backend's height-one
        // 2D representation gives promoted 1D images their correct X result,
        // while 2D arrays expose their layer count in Z. Cube descriptors store
        // depth in cubes, so their dimensions need no arithmetic adjustment.
        // Explicit TGSI 1D_ARRAY still requires its layer result in Y; WGSL's
        // promoted 2D_ARRAY target keeps it in Z for textureNumLayers.
        if target_info.promote_1d && target_info.layer.is_some() {
            return Ok(SSARef::new(&[dst[0], dst[2], dst[2], dst[3]]));
        }
        return Ok(dst);
    }
    let fetch = opcode == "TXF";
    if target_info.buffer && !fetch {
        return Err(format!("{opcode} cannot sample a BUFFER texture; use TXF"));
    }
    if fetch && (target_info.cube() || target_info.comparator.is_some()) {
        return Err(format!("TXF is not defined for {target}"));
    }
    if matches!(opcode, "TXB" | "TXL")
        && (target_info.layer == Some(3) || target_info.comparator == Some(3))
    {
        return Err(format!(
            "{opcode} {target} requires a separate bias/LOD source"
        ));
    }
    if matches!(opcode, "TXB" | "TXL") && target_info.rectangle {
        return Err(format!("{opcode} is not defined for rectangle textures"));
    }

    // Save non-spatial components before promoting 1D coordinates or
    // normalizing a cube direction.
    let layer = target_info.layer.map(|component| coords[component].clone());
    let comparator = target_info
        .comparator
        .map(|component| coords[component].clone());
    let lod = coords[3].clone();
    if target_info.promote_1d {
        coords[1] = Src::ZERO;
    }
    if target_info.cube() {
        // Maxwell consumes cube directions whose largest absolute component
        // is one. This is the same normalization performed before NAK NIR
        // texture lowering; the layer and shadow reference stay untouched.
        let norm_xy = fmax(builder, coords[0].clone().fabs(), coords[1].clone().fabs());
        let norm = fmax(builder, norm_xy.into(), coords[2].clone().fabs());
        let inverse = builder.mufu(MuFuOp::Rcp, norm.into(), FloatType::F32);
        for coord in &mut coords[..3] {
            *coord = fmul(builder, coord.clone(), inverse.into()).into();
        }
    }

    // SM50+ non-scalar operands: layer, spatial coordinates in source 0;
    // explicit LOD/bias followed by shadow comparison value in source 1.
    let mut src0 = Vec::with_capacity(4);
    if let Some(layer) = layer {
        src0.push(array_layer(builder, layer, fetch));
    }
    src0.extend(coords[..target_info.spatial_components].iter().cloned());
    let mut src1 = Vec::with_capacity(2);
    let lod_mode = match opcode {
        "TEX" => TexLodMode::Auto,
        "TXB" => {
            src1.push(lod);
            TexLodMode::Bias
        }
        "TXL" => {
            src1.push(lod);
            TexLodMode::Lod
        }
        "TXF" if target_info.buffer || target_info.rectangle => TexLodMode::Zero,
        "TXF" => {
            src1.push(lod);
            TexLodMode::Lod
        }
        _ => unreachable!(),
    };
    if let Some(comparator) = comparator {
        src1.push(comparator);
    }
    let srcs = [source_vector(builder, src0), source_vector(builder, src1)];
    let dst = builder.alloc_ssa_vec(RegFile::GPR, 4);
    let dsts = [dst.clone().into(), Dst::None];
    let tex = TexRef::Bound(slot as u16);
    let channel_mask = ChannelMask::new(0xf);
    if fetch {
        builder.push_op(OpTld {
            dsts,
            fault: Dst::None,
            tex,
            srcs,
            dim: target_info.dim,
            is_ms: false,
            lod_mode,
            offset_mode: TexOffsetMode::None,
            mem_eviction_priority: MemEvictionPriority::Normal,
            nodep: false,
            channel_mask,
            scalar: false,
        });
    } else {
        builder.push_op(OpTex {
            dsts,
            fault: Dst::None,
            tex,
            srcs,
            dim: target_info.dim,
            lod_mode,
            deriv_mode: TexDerivMode::Auto,
            z_cmpr: target_info.comparator.is_some(),
            offset_mode: TexOffsetMode::None,
            mem_eviction_priority: MemEvictionPriority::Normal,
            nodep: false,
            channel_mask,
            scalar: false,
        });
    }
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(opcode: &str, target: &str) -> (SSARef, Vec<Instr>) {
        let sm = ShaderModelInfo::new(52, 32);
        let mut allocator = SSAValueAllocator::new();
        let mut builder = SSAInstrBuilder::new(&sm, &mut allocator);
        let dst = build_texture(
            &mut builder,
            opcode,
            target,
            7,
            [1.0f32.into(), 2.0f32.into(), 3.0f32.into(), 4.0f32.into()],
        )
        .unwrap();
        (dst, builder.into_vec())
    }

    fn copies_to(instructions: &[Instr], source: &Src) -> Vec<Src> {
        source
            .as_ssa()
            .unwrap()
            .iter()
            .map(|component| {
                instructions
                    .iter()
                    .find_map(|instruction| match &instruction.op {
                        Op::Copy(op) if op.dst.as_ssa().is_some_and(|dst| dst[0] == *component) => {
                            Some(op.src.clone())
                        }
                        _ => None,
                    })
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn shadow_sampling_packs_lod_before_comparator() {
        let (dst, instructions) = lower("TXL", "SHADOW2D");
        assert_eq!(dst.comps(), 4);
        let Op::Tex(op) = &instructions.last().unwrap().op else {
            panic!("expected sampled texture operation");
        };
        assert!(op.tex == TexRef::Bound(7));
        assert!(op.dim == TexDim::_2D);
        assert!(op.lod_mode == TexLodMode::Lod);
        assert!(op.z_cmpr);
        assert_eq!(op.channel_mask.to_bits(), 0xf);
        let coordinates = copies_to(&instructions, &op.srcs[0]);
        let parameters = copies_to(&instructions, &op.srcs[1]);
        assert!(coordinates == vec![1.0f32.into(), 2.0f32.into()]);
        assert!(parameters == vec![4.0f32.into(), 3.0f32.into()]);
    }

    #[test]
    fn array_fetch_keeps_integer_layer_and_places_it_first() {
        let (_, instructions) = lower("TXF", "2D_ARRAY");
        let Op::Tld(op) = &instructions.last().unwrap().op else {
            panic!("expected texel fetch operation");
        };
        assert!(op.dim == TexDim::Array2D);
        assert!(op.lod_mode == TexLodMode::Lod);
        assert_eq!(op.srcs[0].as_ssa().unwrap().comps(), 3);
        assert_eq!(op.srcs[1].as_ssa().unwrap().comps(), 1);
        assert!(
            !instructions
                .iter()
                .any(|instruction| matches!(instruction.op, Op::F2I(_)))
        );
        let coordinates = copies_to(&instructions, &op.srcs[0]);
        assert!(coordinates[1] == 1.0f32.into());
        assert!(coordinates[2] == 2.0f32.into());
        assert!(copies_to(&instructions, &op.srcs[1]) == vec![4.0f32.into()]);
    }

    #[test]
    fn sampled_array_rounds_and_clamps_the_layer() {
        let (_, instructions) = lower("TEX", "2D_ARRAY");
        assert!(instructions.iter().any(|instruction| matches!(&instruction.op, Op::F2I(op) if op.dst_type == IntType::U32 && op.rnd_mode == FRndMode::Zero)));
        assert!(instructions.iter().any(|instruction| matches!(&instruction.op, Op::IMnMx(op) if op.cmp_type == IntCmpType::U32 && op.srcs[1] == u32::from(u16::MAX).into())));
    }

    #[test]
    fn buffer_fetch_has_no_mip_source() {
        let (_, instructions) = lower("TXF", "BUFFER");
        let Op::Tld(op) = &instructions.last().unwrap().op else {
            panic!("expected buffer fetch operation");
        };
        assert!(op.dim == TexDim::_1D);
        assert!(op.lod_mode == TexLodMode::Zero);
        assert!(op.srcs[1].is_zero());
    }

    #[test]
    fn dimension_query_uses_x_integer_level_and_hardware_dimensions() {
        let sm = ShaderModelInfo::new(52, 32);
        let mut allocator = SSAValueAllocator::new();
        let mut builder = SSAInstrBuilder::new(&sm, &mut allocator);
        let dst = build_texture(
            &mut builder,
            "TXQ",
            "2D",
            13,
            [2u32.into(), 0u32.into(), 0u32.into(), 9u32.into()],
        )
        .unwrap();
        let instructions = builder.into_vec();
        let Op::Txq(op) = &instructions.last().unwrap().op else {
            panic!("expected hardware dimension query");
        };
        assert!(op.query == TexQuery::Dimension);
        assert!(op.tex == TexRef::Bound(13));
        assert_eq!(op.channel_mask.to_bits(), 0xf);
        assert!(op.dsts[0].as_ssa().unwrap() == &dst);
        assert!(copies_to(&instructions, &op.src) == vec![2u32.into()]);
        assert!(
            !instructions
                .iter()
                .any(|instruction| matches!(instruction.op, Op::F2I(_) | Op::Tex(_) | Op::Tld(_)))
        );
    }

    #[test]
    fn array_layer_query_retains_hardware_z_and_cube_queries_do_not_normalize() {
        for target in ["2D_ARRAY", "SHADOW2D_ARRAY", "CUBE", "CUBE_ARRAY"] {
            let (dst, instructions) = lower("TXQ", target);
            let Op::Txq(op) = &instructions.last().unwrap().op else {
                panic!("expected hardware dimension query for {target}");
            };
            let query_dst = op.dsts[0].as_ssa().unwrap();
            assert!(dst == *query_dst);
            assert!(dst[2] == query_dst[2]);
            assert!(op.query == TexQuery::Dimension);
            assert_eq!(instructions.len(), 2);
        }
    }

    #[test]
    fn promoted_one_dimensional_queries_use_hardware_width_and_layers() {
        let (dst, instructions) = lower("TXQ", "1D");
        let Op::Txq(op) = &instructions.last().unwrap().op else {
            panic!("expected promoted 1D dimension query");
        };
        assert!(dst == *op.dsts[0].as_ssa().unwrap());

        let (dst, instructions) = lower("TXQ", "1D_ARRAY");
        let Op::Txq(op) = &instructions.last().unwrap().op else {
            panic!("expected promoted 1D array dimension query");
        };
        let hardware = op.dsts[0].as_ssa().unwrap();
        assert!(dst[0] == hardware[0]);
        assert!(dst[1] == hardware[2]);
        assert!(dst[3] == hardware[3]);
    }

    #[test]
    fn dimension_queries_survive_maxwell_allocation_scheduling_and_encoding() {
        use sgfx_nak::{
            GraphicsStage, compile_graphics_ir, compiler::cfg::CFGBuilder, graphics_shader_info,
        };
        use std::collections::hash_map::RandomState;

        for target in ["1D", "1D_ARRAY", "2D", "2D_ARRAY", "CUBE"] {
            let sm = ShaderModelInfo::new(52, 64);
            let mut allocator = SSAValueAllocator::new();
            let mut builder = SSAInstrBuilder::new(&sm, &mut allocator);
            let dimensions = build_texture(
                &mut builder,
                "TXQ",
                target,
                8,
                [2u32.into(), Src::ZERO, Src::ZERO, Src::ZERO],
            )
            .unwrap();
            builder.push_op(OpRegOut {
                srcs: dimensions.iter().copied().map(Src::from).collect(),
            });
            builder.push_op(OpExit {});
            let instructions = builder.into_vec();
            let mut cfg = CFGBuilder::<u32, BasicBlock, RandomState>::new();
            cfg.add_node(
                0,
                BasicBlock {
                    label: LabelAllocator::new().alloc(),
                    uniform: true,
                    instrs: instructions,
                },
            );
            let mut info = graphics_shader_info(GraphicsStage::Fragment);
            let ShaderIoInfo::Fragment(io) = &mut info.io else {
                unreachable!();
            };
            io.writes_color = 0xf;
            let compiled = compile_graphics_ir(Shader {
                sm: &sm,
                info,
                functions: vec![Function {
                    ssa_alloc: allocator,
                    phi_alloc: PhiAllocator::new(),
                    blocks: cfg.as_cfg(false),
                }],
            })
            .unwrap();
            assert!(compiled.assembly.contains("txq"));
            assert!(compiled.assembly.contains("tex[8]"));
            assert!(compiled.assembly.contains("dimension"));
            assert!(!compiled.code.is_empty());
            assert_eq!(compiled.code.len() % 8, 0);
        }
    }

    #[test]
    fn cube_direction_is_normalized_without_altering_shadow_reference() {
        let (_, instructions) = lower("TEX", "SHADOWCUBE");
        assert_eq!(
            instructions
                .iter()
                .filter(|instruction| matches!(instruction.op, Op::FMul(_)))
                .count(),
            3
        );
        assert!(
            instructions
                .iter()
                .any(|instruction| matches!(&instruction.op, Op::MuFu(op) if op.op == MuFuOp::Rcp))
        );
        let Op::Tex(op) = &instructions.last().unwrap().op else {
            panic!("expected cube sampling operation");
        };
        assert!(copies_to(&instructions, &op.srcs[1]) == vec![4.0f32.into()]);
    }

    #[test]
    fn unsupported_operands_do_not_emit_partial_instructions() {
        let sm = ShaderModelInfo::new(52, 32);
        let mut allocator = SSAValueAllocator::new();
        let mut builder = SSAInstrBuilder::new(&sm, &mut allocator);
        for (opcode, target, slot) in [
            ("TEX", "2D", 8192),
            ("TXL", "SHADOW2D_ARRAY", 0),
            ("TXB", "SHADOWCUBE", 0),
            ("TXF", "CUBE", 0),
            ("TEX", "BUFFER", 0),
        ] {
            assert!(
                build_texture(
                    &mut builder,
                    opcode,
                    target,
                    slot,
                    std::array::from_fn(|_| Src::ZERO)
                )
                .is_err()
            );
        }
        assert!(builder.into_vec().is_empty());
    }
}

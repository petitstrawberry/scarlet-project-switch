// SPDX-License-Identifier: MIT
//! TGSI's mutable scalar lanes become NAK SSA, with actual Maxwell SIMT control
//! flow. Re-convergence follows Mesa's SM50 SSY/SYNC and PBK/PCNT convention.

use crate::{alu, texture, tgsi::*};
use sgfx_nak::ir::Instr;
use sgfx_nak::{GraphicsStage, compiler::cfg::CFGBuilder, ir::*, sph::PixelImap};
use std::collections::{HashMap, hash_map::RandomState};

type Lanes = HashMap<(RegisterFile, u16, u8), SSAValue>;

#[derive(Clone, Copy)]
enum Scope {
    If {
        start: usize,
        otherwise: Option<usize>,
    },
    Loop {
        start: usize,
    },
}
struct Control {
    if_else: HashMap<usize, usize>,
    if_end: HashMap<usize, usize>,
    else_end: HashMap<usize, usize>,
    loop_end: HashMap<usize, usize>,
    loop_start: HashMap<usize, usize>,
    jump_loop: HashMap<usize, usize>,
    depth: u32,
}
fn control(program: &Program) -> Result<Control, String> {
    let mut c = Control {
        if_else: HashMap::new(),
        if_end: HashMap::new(),
        else_end: HashMap::new(),
        loop_end: HashMap::new(),
        loop_start: HashMap::new(),
        jump_loop: HashMap::new(),
        depth: 0,
    };
    let mut scopes = Vec::new();
    for (i, instr) in program.instructions.iter().enumerate() {
        match instr.opcode.as_str() {
            "UIF" => scopes.push(Scope::If {
                start: i,
                otherwise: None,
            }),
            "ELSE" => match scopes.last_mut() {
                Some(Scope::If { start, otherwise }) if otherwise.is_none() => {
                    *otherwise = Some(i);
                    c.if_else.insert(*start, i);
                }
                _ => return Err("malformed ELSE control flow".into()),
            },
            "ENDIF" => match scopes.pop() {
                Some(Scope::If { start, otherwise }) => {
                    c.if_end.insert(start, i);
                    if let Some(e) = otherwise {
                        c.else_end.insert(e, i);
                    }
                }
                _ => return Err("malformed ENDIF control flow".into()),
            },
            "BGNLOOP" => scopes.push(Scope::Loop { start: i }),
            "ENDLOOP" => match scopes.pop() {
                Some(Scope::Loop { start }) => {
                    c.loop_end.insert(start, i);
                    c.loop_start.insert(i, start);
                }
                _ => return Err("malformed ENDLOOP control flow".into()),
            },
            "BRK" | "CONT" => {
                let start = scopes
                    .iter()
                    .rev()
                    .find_map(|scope| {
                        if let Scope::Loop { start } = scope {
                            Some(*start)
                        } else {
                            None
                        }
                    })
                    .ok_or("loop jump outside loop")?;
                c.jump_loop.insert(i, start);
            }
            _ => {}
        }
        let depth = scopes
            .iter()
            .map(|s| match s {
                Scope::If { .. } => 1,
                Scope::Loop { .. } => 2,
            })
            .sum();
        c.depth = c.depth.max(depth);
        if c.depth > 16 {
            return Err("Maxwell re-convergence stack exceeds 16 entries".into());
        }
    }
    if !scopes.is_empty() {
        return Err("unterminated TGSI control flow".into());
    }
    Ok(c)
}

pub fn lower<'a>(sm: &'a ShaderModelInfo, program: &Program) -> Result<Shader<'a>, String> {
    let stage = match program.stage {
        Stage::Vertex => GraphicsStage::Vertex,
        Stage::Fragment => GraphicsStage::Fragment,
    };
    let mut info = sgfx_nak::graphics_shader_info(stage);
    let control = control(program)?;
    info.max_crs_depth = control.depth;
    let mut alloc = SSAValueAllocator::new();
    let mut labels = LabelAllocator::new();
    let entry_label = labels.alloc();
    let instr_labels: Vec<_> = (0..program.instructions.len())
        .map(|_| labels.alloc())
        .collect();
    let mut cfg = CFGBuilder::<usize, BasicBlock, RandomState>::new();
    let mut lanes = Lanes::new();
    let mut prolog = SSAInstrBuilder::new(sm, &mut alloc);
    let mut consumed = std::collections::HashSet::new();
    let mut written = HashMap::<u16, u8>::new();
    for instr in &program.instructions {
        let no_dst = matches!(
            instr.opcode.as_str(),
            "UIF" | "ELSE" | "ENDIF" | "BGNLOOP" | "ENDLOOP" | "BRK" | "CONT" | "KILL" | "END"
        );
        if !no_dst {
            if let Some(dst) = instr.args.first() {
                if dst.file == RegisterFile::Out {
                    *written.entry(dst.index).or_default() |= dst.mask;
                }
            }
        }
        for src in instr.args.iter().skip(if no_dst { 0 } else { 1 }) {
            if matches!(src.file, RegisterFile::In | RegisterFile::Sv) {
                for c in src.swizzle {
                    consumed.insert((src.file, src.index, c));
                }
            }
        }
    }
    let mut inv_w = None;
    if stage == GraphicsStage::Fragment {
        // SPH must always declare position.w; Mesa documents an otherwise
        // reproducible hardware trap, including shaders with only flat inputs.
        if let ShaderIoInfo::Fragment(io) = &mut info.io {
            io.sysvals_in.ab |= 1 << 31;
        }
    }
    for decl in &program.declarations {
        match decl.file {
            RegisterFile::Temp | RegisterFile::Out => {
                for index in decl.first..=decl.last {
                    for c in 0..4 {
                        lanes.insert((decl.file, index, c), prolog.copy(0.into()));
                    }
                }
            }
            RegisterFile::In | RegisterFile::Sv => {
                for index in decl.first..=decl.last {
                    for c in 0..4 {
                        if !consumed.contains(&(decl.file, index, c)) {
                            continue;
                        }
                        let addr = input_addr(stage, decl, index, c)?;
                        let value = if stage == GraphicsStage::Vertex {
                            let dst = prolog.alloc_ssa(RegFile::GPR);
                            prolog.push_op(OpALd {
                                dst: dst.into(),
                                vtx: 0.into(),
                                offset: 0.into(),
                                addr,
                                comps: 1,
                                patch: false,
                                output: false,
                                phys: false,
                            });
                            if let ShaderIoInfo::Vtg(io) = &mut info.io {
                                io.mark_attrs_read(addr..addr + 4);
                            }
                            dst
                        } else {
                            let interpolation =
                                decl.interpolation.unwrap_or(Interpolation::Perspective);
                            let (freq, imap) = match interpolation {
                                Interpolation::Constant => {
                                    (InterpFreq::Constant, PixelImap::Constant)
                                }
                                Interpolation::Linear => {
                                    (InterpFreq::Pass, PixelImap::ScreenLinear)
                                }
                                Interpolation::Perspective => {
                                    (InterpFreq::PassMulW, PixelImap::Perspective)
                                }
                            };
                            let inv = if interpolation == Interpolation::Perspective {
                                if inv_w.is_none() {
                                    let w = prolog.alloc_ssa(RegFile::GPR);
                                    prolog.push_op(OpIpa {
                                        dst: w.into(),
                                        addr: 0x7c,
                                        freq: InterpFreq::Pass,
                                        loc: InterpLoc::Default,
                                        inv_w: 0.into(),
                                        offset: 0.into(),
                                    });
                                    inv_w = Some(alu::lower(
                                        &mut prolog,
                                        "DIV",
                                        &[1.0f32.to_bits().into(), w.into()],
                                    )?);
                                }
                                inv_w.clone().unwrap()
                            } else {
                                0.into()
                            };
                            let dst = prolog.alloc_ssa(RegFile::GPR);
                            prolog.push_op(OpIpa {
                                dst: dst.into(),
                                addr,
                                freq,
                                loc: InterpLoc::Default,
                                inv_w: inv,
                                offset: 0.into(),
                            });
                            if let ShaderIoInfo::Fragment(io) = &mut info.io {
                                io.mark_attr_read(addr, imap);
                            }
                            if matches!(decl.semantic, Some(Semantic::Face)) {
                                // Maxwell's flat front-face value is a boolean bit
                                // pattern; TGSI FACE is a signed float +1/-1.
                                let pred = prolog.isetp(
                                    IntCmpType::U32,
                                    IntCmpOp::Ne,
                                    dst.into(),
                                    0.into(),
                                );
                                {
                                    let value = prolog.sel(
                                        pred.into(),
                                        1.0f32.to_bits().into(),
                                        (-1.0f32).to_bits().into(),
                                    );
                                    prolog.copy(value.into())
                                }
                            } else {
                                dst
                            }
                        };
                        lanes.insert((decl.file, index, c), value);
                    }
                }
            }
            _ => {}
        }
    }
    let mut prolog_instrs = prolog.into_vec();
    if prolog_instrs.is_empty() {
        prolog_instrs.push(Instr::new(OpNop { label: None }));
    }
    cfg.add_node(
        usize::MAX,
        BasicBlock {
            label: entry_label,
            uniform: true,
            instrs: prolog_instrs,
        },
    );
    cfg.add_edge(usize::MAX, 0);
    let immediates: HashMap<_, _> = program
        .immediates
        .iter()
        .map(|imm| (imm.index, imm.values))
        .collect();
    for (i, instr) in program.instructions.iter().enumerate() {
        let mut b = SSAInstrBuilder::new(sm, &mut alloc);
        // A loop header pushes the continue target on every iteration.
        if control.loop_end.keys().any(|start| *start + 1 == i) {
            b.push_op(OpPCnt {
                target: instr_labels[i],
            });
        }
        let next = i + 1;
        match instr.opcode.as_str() {
            "UIF" => {
                let cond = source(&instr.args[0], 0, &lanes, &immediates)?;
                let pred = b.isetp(IntCmpType::U32, IntCmpOp::Ne, cond, 0.into());
                let end = control.if_end[&i];
                let otherwise = control
                    .if_else
                    .get(&i)
                    .map(|index| index + 1)
                    .unwrap_or(end);
                // NAK's SSA repair requires every conditional edge to be
                // non-critical. Give both arms a real single-successor block,
                // including the empty false arm of a WGSL if without else.
                let then_key = program.instructions.len() + i * 2;
                let else_key = then_key + 1;
                let then_label = labels.alloc();
                let else_label = labels.alloc();
                cfg.add_node(
                    then_key,
                    BasicBlock {
                        label: then_label,
                        uniform: false,
                        instrs: vec![Instr::new(OpBra {
                            target: instr_labels[next],
                            cond: true.into(),
                        })],
                    },
                );
                cfg.add_node(
                    else_key,
                    BasicBlock {
                        label: else_label,
                        uniform: false,
                        instrs: vec![Instr::new(OpBra {
                            target: instr_labels[otherwise],
                            cond: true.into(),
                        })],
                    },
                );
                cfg.add_edge(then_key, next);
                cfg.add_edge(else_key, otherwise);
                b.push_op(OpSSy {
                    target: instr_labels[end + 1],
                });
                b.predicate(Pred::from(pred).bnot()).push_op(OpBra {
                    target: else_label,
                    cond: true.into(),
                });
                cfg.add_edge(i, then_key);
                cfg.add_edge(i, else_key);
            }
            "ELSE" => {
                let target = control.else_end[&i] + 1;
                b.push_op(OpSync {
                    target: instr_labels[target],
                });
                cfg.add_edge(i, target);
            }
            "ENDIF" => {
                b.push_op(OpSync {
                    target: instr_labels[next],
                });
                cfg.add_edge(i, next);
            }
            "BGNLOOP" => {
                b.push_op(OpPBk {
                    target: instr_labels[control.loop_end[&i] + 1],
                });
                cfg.add_edge(i, next);
            }
            "ENDLOOP" => {
                let target = control.loop_start[&i] + 1;
                b.push_op(OpCont {
                    target: instr_labels[target],
                });
                cfg.add_edge(i, target);
            }
            "BRK" => {
                let target = control.loop_end[&control.jump_loop[&i]] + 1;
                b.push_op(OpBrk {
                    target: instr_labels[target],
                });
                cfg.add_edge(i, target);
            }
            "CONT" => {
                let target = control.jump_loop[&i] + 1;
                b.push_op(OpCont {
                    target: instr_labels[target],
                });
                cfg.add_edge(i, target);
            }
            "KILL" => {
                if let ShaderStageInfo::Fragment(fs) = &mut info.stage {
                    fs.uses_kill = true;
                } else {
                    return Err("discard in vertex shader".into());
                }
                b.push_op(OpKill {});
                cfg.add_edge(i, next);
            }
            "END" => {
                emit_outputs(&mut b, stage, program, &lanes, &written, &mut info)?;
                b.push_op(OpExit {});
            }
            "TEX" | "TXB" | "TXL" | "TXF" | "TXQ" => {
                let target = instr
                    .texture_target
                    .as_ref()
                    .ok_or("texture instruction without target")?;
                let coords: [Result<Src, String>; 4] =
                    std::array::from_fn(|c| source(&instr.args[1], c, &lanes, &immediates));
                let coords: [Src; 4] = coords
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()?
                    .try_into()
                    .map_err(|_| "texture coordinate shape")?;
                let slot = instr.args[2].index;
                let value = texture::build_texture(
                    &mut b,
                    &instr.opcode,
                    &target.to_string(),
                    u32::from(slot) + 8,
                    coords,
                )?;
                let dst = &instr.args[0];
                for c in 0..4 {
                    if dst.mask & (1 << c) != 0 {
                        b.copy_to(
                            destination(dst, c, &lanes)?.into(),
                            value[c as usize].into(),
                        );
                    }
                }
                cfg.add_edge(i, next);
            }
            opcode => {
                let dst = &instr.args[0];
                // All source components are captured before writes so vector
                // destinations preserve TGSI's simultaneous assignment rule.
                let mut results = Vec::new();
                for c in 0..4 {
                    if dst.mask & (1 << c) == 0 {
                        continue;
                    }
                    let srcs = instr.args[1..]
                        .iter()
                        .map(|arg| source(arg, c as usize, &lanes, &immediates))
                        .collect::<Result<Vec<_>, _>>()?;
                    results.push((c, alu::lower(&mut b, opcode, &srcs)?));
                }
                for (c, value) in results {
                    b.copy_to(destination(dst, c, &lanes)?.into(), value);
                }
                cfg.add_edge(i, next);
            }
        }
        cfg.add_node(
            i,
            BasicBlock {
                label: instr_labels[i],
                uniform: false,
                instrs: b.into_vec(),
            },
        );
    }
    let mut function = Function {
        ssa_alloc: alloc,
        phi_alloc: PhiAllocator::new(),
        blocks: cfg.as_cfg(true),
    };
    function.repair_ssa();
    Ok(Shader {
        sm,
        info,
        functions: vec![function],
    })
}

fn source(
    arg: &Operand,
    component: usize,
    lanes: &Lanes,
    imm: &HashMap<u16, [u32; 4]>,
) -> Result<Src, String> {
    let c = arg.swizzle[component];
    let value: Src = match arg.file {
        RegisterFile::Imm => {
            imm.get(&arg.index).ok_or("undeclared TGSI immediate")?[c as usize].into()
        }
        RegisterFile::Const => SrcRef::CBuf(CBufRef {
            buf: CBuf::Binding(0),
            offset: arg
                .index
                .checked_mul(16)
                .and_then(|v| v.checked_add(u16::from(c) * 4))
                .ok_or("constant offset overflow")?,
        })
        .into(),
        _ => lanes
            .get(&(arg.file, arg.index, c))
            .copied()
            .ok_or_else(|| format!("undeclared TGSI source {:?}[{}].{}", arg.file, arg.index, c))?
            .into(),
    };
    Ok(if arg.negate {
        if arg.absolute {
            value.fabs().fneg()
        } else {
            value.fneg()
        }
    } else if arg.absolute {
        value.fabs()
    } else {
        value
    })
}
fn destination(arg: &Operand, c: u8, lanes: &Lanes) -> Result<SSAValue, String> {
    if !matches!(arg.file, RegisterFile::Temp | RegisterFile::Out) {
        return Err("TGSI write outside temporary/output lanes".into());
    }
    lanes
        .get(&(arg.file, arg.index, c))
        .copied()
        .ok_or_else(|| "undeclared TGSI destination".into())
}
fn input_addr(stage: GraphicsStage, decl: &Declaration, index: u16, c: u8) -> Result<u16, String> {
    let base = match decl.semantic {
        None if stage == GraphicsStage::Vertex => 0x80 + index * 16,
        Some(Semantic::Generic(location)) => 0x80 + location * 16,
        Some(Semantic::Position) => 0x70,
        Some(Semantic::VertexId) if stage == GraphicsStage::Vertex => 0x2fc,
        Some(Semantic::InstanceId) if stage == GraphicsStage::Vertex => 0x2f8,
        Some(Semantic::Face) if stage == GraphicsStage::Fragment => 0x3fc,
        _ => return Err("unsupported Maxwell input semantic".into()),
    };
    if matches!(
        decl.semantic,
        Some(Semantic::VertexId | Semantic::InstanceId | Semantic::Face)
    ) && c != 0
    {
        return Err("non-scalar system value".into());
    }
    Ok(base + u16::from(c) * 4)
}
fn emit_outputs(
    b: &mut SSAInstrBuilder<'_>,
    stage: GraphicsStage,
    program: &Program,
    lanes: &Lanes,
    written: &HashMap<u16, u8>,
    info: &mut ShaderInfo,
) -> Result<(), String> {
    if stage == GraphicsStage::Vertex {
        for decl in program
            .declarations
            .iter()
            .filter(|d| d.file == RegisterFile::Out)
        {
            for index in decl.first..=decl.last {
                let base = match decl.semantic {
                    Some(Semantic::Position) => 0x70,
                    Some(Semantic::PointSize) => 0x6c,
                    Some(Semantic::Generic(location)) => 0x80 + location * 16,
                    _ => return Err("unsupported Maxwell vertex output semantic".into()),
                };
                let mask = written.get(&index).copied().unwrap_or(0);
                for c in 0..4 {
                    if mask & (1 << c) != 0 {
                        let addr = base + u16::from(c) * 4;
                        let data = lanes[&(RegisterFile::Out, index, c)];
                        b.push_op(OpASt {
                            vtx: 0.into(),
                            offset: 0.into(),
                            data: data.into(),
                            addr,
                            comps: 1,
                            patch: false,
                            phys: false,
                        });
                        if let ShaderIoInfo::Vtg(io) = &mut info.io {
                            io.mark_attrs_written(addr..addr + 4);
                            io.mark_store_req(addr..addr + 4);
                        }
                    }
                }
            }
        }
    } else {
        let mut outputs = Vec::new();
        let mut colors: Vec<_> = program
            .declarations
            .iter()
            .filter_map(|d| {
                if d.file == RegisterFile::Out {
                    if let Some(Semantic::Color(location)) = d.semantic {
                        Some((location, d.first))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        colors.sort_unstable();
        for (location, index) in colors {
            if location >= 8 {
                return Err("Maxwell supports at most eight fragment color outputs".into());
            }
            let mask = written.get(&index).copied().unwrap_or(0);
            if mask == 0 {
                continue;
            }
            if let ShaderIoInfo::Fragment(io) = &mut info.io {
                io.writes_color |= u32::from(mask) << (location * 4);
            }
            outputs.extend((0..4).map(|c| Src::from(lanes[&(RegisterFile::Out, index, c)])));
        }
        if !outputs.is_empty() {
            b.push_op(OpRegOut { srcs: outputs });
        }
    }
    Ok(())
}

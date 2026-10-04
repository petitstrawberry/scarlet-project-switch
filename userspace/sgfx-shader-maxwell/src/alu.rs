//! Typed scalar TGSI operations for the Maxwell programmable shader frontend.
//!
//! Integer registers retain their 32-bit representation throughout lowering;
//! comparisons explicitly produce TGSI's all-bits boolean values.

use sgfx_nak::ir::*;

/// Lower one scalar instruction emitted by `sgfx-codegen-virgl`.
pub fn lower(b: &mut SSAInstrBuilder<'_>, opcode: &str, srcs: &[Src]) -> Result<Src, String> {
    let arity = match opcode {
        "MOV" | "ABS" | "EX2" | "LG2" | "SIN" | "COS" | "RSQ" | "SQRT" | "FRC" | "CEIL" | "FLR"
        | "ROUND" | "TRUNC" | "NOT" | "INEG" | "IABS" | "F2I" | "F2U" | "I2F" | "U2F" => 1,
        "ADD" | "SUB" | "MUL" | "DIV" | "POW" | "MIN" | "MAX" | "FSEQ" | "FSNE" | "FSGE"
        | "FSLT" | "SGE" | "USEQ" | "USNE" | "ISGE" | "ISLT" | "USGE" | "USLT" | "AND" | "OR"
        | "XOR" | "UADD" | "UMUL" | "IMIN" | "IMAX" | "UMIN" | "UMAX" | "SHL" | "ISHR" | "USHR"
        | "IDIV" | "IMOD" | "UDIV" | "UMOD" => 2,
        "UCMP" | "CMP" => 3,
        _ => return Err(format!("unsupported scalar TGSI opcode: {opcode}")),
    };
    if srcs.len() != arity {
        return Err(format!(
            "TGSI {opcode} requires {arity} scalar sources, got {}",
            srcs.len()
        ));
    }
    if !(52..70).contains(&b.sm()) {
        return Err(format!(
            "scalar TGSI lowering requires SM52–SM62, got SM{}",
            b.sm()
        ));
    }

    // SEL and bitwise instructions do not accept floating point modifiers.
    // Materialize them without floating point arithmetic so MOV also preserves
    // signed zero, NaN payloads, and integer bit patterns.
    let args: Vec<Src> = srcs.iter().map(|src| unmodify(b, src)).collect();
    let a = args[0].clone();
    let c = || args[1].clone();

    let result = match opcode {
        "MOV" => b.copy(a).into(),
        "ABS" => b.lop2(LogicOp2::And, a, 0x7fff_ffff_u32.into()).into(),
        "ADD" => b.fadd(a, c()).into(),
        "SUB" => b.fadd(a, c().fneg()).into(),
        "MUL" => fmul(b, a, c()),
        "DIV" => fdiv(b, a, c()),
        "POW" => {
            let log = b.mufu(MuFuOp::Log2, a, FloatType::F32);
            let exponent = fmul(b, log.into(), c());
            b.fexp2(exponent).into()
        }
        "EX2" => b.fexp2(a).into(),
        "LG2" => b.mufu(MuFuOp::Log2, a, FloatType::F32).into(),
        "SIN" => b.fsin(a).into(),
        "COS" => b.fcos(a).into(),
        "RSQ" => b.mufu(MuFuOp::Rsq, a, FloatType::F32).into(),
        "SQRT" => b.mufu(MuFuOp::Sqrt, a, FloatType::F32).into(),
        "CEIL" => round(b, a, FRndMode::PosInf),
        "FLR" => round(b, a, FRndMode::NegInf),
        "ROUND" => round(b, a, FRndMode::NearestEven),
        "TRUNC" => round(b, a, FRndMode::Zero),
        "FRC" => {
            let floor = round(b, a.clone(), FRndMode::NegInf);
            b.fadd(a, floor.fneg()).into()
        }
        "MIN" | "MAX" => {
            let dst = b.alloc_ssa(RegFile::GPR);
            b.push_op(OpFMnMx {
                dst: dst.into(),
                srcs: [a, c()],
                min: (opcode == "MIN").into(),
                ftz: false,
            });
            dst.into()
        }
        "FSEQ" | "FSNE" | "FSGE" | "FSLT" => {
            let cmp = match opcode {
                "FSEQ" => FloatCmpOp::OrdEq,
                "FSNE" => FloatCmpOp::UnordNe,
                "FSGE" => FloatCmpOp::OrdGe,
                "FSLT" => FloatCmpOp::OrdLt,
                _ => unreachable!(),
            };
            let pred = b.fsetp(cmp, a, c());
            bool_bits(b, pred.into())
        }
        "SGE" => b.fset(FloatCmpOp::OrdGe, a, c()).into(),
        "USEQ" | "USNE" | "ISGE" | "ISLT" | "USGE" | "USLT" => {
            let ty = if opcode.starts_with('I') {
                IntCmpType::I32
            } else {
                IntCmpType::U32
            };
            let cmp = match opcode {
                "USEQ" => IntCmpOp::Eq,
                "USNE" => IntCmpOp::Ne,
                "ISGE" | "USGE" => IntCmpOp::Ge,
                "ISLT" | "USLT" => IntCmpOp::Lt,
                _ => unreachable!(),
            };
            let pred = b.isetp(ty, cmp, a, c());
            bool_bits(b, pred.into())
        }
        "NOT" => b.lop2(LogicOp2::PassB, 0_u32.into(), a.bnot()).into(),
        "AND" => b.lop2(LogicOp2::And, a, c()).into(),
        "OR" => b.lop2(LogicOp2::Or, a, c()).into(),
        "XOR" => b.lop2(LogicOp2::Xor, a, c()).into(),
        "UADD" => b.iadd(a, c(), 0_u32.into()).into(),
        "UMUL" => b.imul(a, c()).into(),
        "INEG" => b.ineg(a).into(),
        "IABS" => b.iabs(a).into(),
        "IMIN" | "IMAX" | "UMIN" | "UMAX" => b
            .imnmx(
                if opcode.starts_with('I') {
                    IntCmpType::I32
                } else {
                    IntCmpType::U32
                },
                a,
                c(),
                opcode.ends_with("MIN").into(),
            )
            .into(),
        "SHL" => b.shl(a, c()).into(),
        "ISHR" => b.shr(a, c(), true).into(),
        "USHR" => b.shr(a, c(), false).into(),
        "F2I" | "F2U" => {
            let dst = b.alloc_ssa(RegFile::GPR);
            b.push_op(OpF2I {
                dst: dst.into(),
                src: a,
                src_type: FloatType::F32,
                dst_type: if opcode == "F2I" {
                    IntType::I32
                } else {
                    IntType::U32
                },
                rnd_mode: FRndMode::Zero,
                ftz: false,
            });
            dst.into()
        }
        "I2F" | "U2F" => {
            let dst = b.alloc_ssa(RegFile::GPR);
            b.push_op(OpI2F {
                dst: dst.into(),
                src: a,
                src_type: if opcode == "I2F" {
                    IntType::I32
                } else {
                    IntType::U32
                },
                dst_type: FloatType::F32,
                rnd_mode: FRndMode::NearestEven,
            });
            dst.into()
        }
        "UCMP" => {
            let pred = b.isetp(IntCmpType::U32, IntCmpOp::Ne, a, 0_u32.into());
            b.sel(pred.into(), c(), args[2].clone()).into()
        }
        "CMP" => {
            let pred = b.fsetp(FloatCmpOp::OrdLt, a, 0_u32.into());
            b.sel(pred.into(), c(), args[2].clone()).into()
        }
        "UDIV" | "UMOD" | "IDIV" | "IMOD" => {
            let signed = opcode.starts_with('I');
            let remainder = opcode.ends_with("MOD");
            divrem(b, a, c(), signed, remainder)
        }
        _ => unreachable!(),
    };
    Ok(result)
}

fn unmodify(b: &mut SSAInstrBuilder<'_>, src: &Src) -> Src {
    let mut raw = src.clone();
    raw.src_mod = SrcMod::None;
    match src.src_mod {
        SrcMod::None => raw,
        SrcMod::FAbs => b.lop2(LogicOp2::And, raw, 0x7fff_ffff_u32.into()).into(),
        SrcMod::FNeg => b.lop2(LogicOp2::Xor, raw, 0x8000_0000_u32.into()).into(),
        SrcMod::FNegAbs => {
            let abs = b.lop2(LogicOp2::And, raw, 0x7fff_ffff_u32.into());
            b.lop2(LogicOp2::Or, abs.into(), 0x8000_0000_u32.into())
                .into()
        }
        SrcMod::INeg => b.ineg(raw).into(),
        SrcMod::BNot => b.lop2(LogicOp2::PassB, 0_u32.into(), raw.bnot()).into(),
    }
}

fn bool_bits(b: &mut SSAInstrBuilder<'_>, pred: Src) -> Src {
    b.sel(pred, u32::MAX.into(), 0_u32.into()).into()
}

fn fmul(b: &mut SSAInstrBuilder<'_>, x: Src, y: Src) -> Src {
    let dst = b.alloc_ssa(RegFile::GPR);
    b.push_op(OpFMul {
        dst: dst.into(),
        srcs: [x, y],
        saturate: false,
        rnd_mode: FRndMode::NearestEven,
        ftz: false,
        dnz: false,
    });
    dst.into()
}

fn round(b: &mut SSAInstrBuilder<'_>, src: Src, rnd_mode: FRndMode) -> Src {
    let dst = b.alloc_ssa(RegFile::GPR);
    b.push_op(OpF2F {
        dst: dst.into(),
        src,
        src_type: FloatType::F32,
        dst_type: FloatType::F32,
        rnd_mode,
        ftz: false,
        integer_rnd: true,
    });
    dst.into()
}

fn fdiv(b: &mut SSAInstrBuilder<'_>, x: Src, y: Src) -> Src {
    // Match Mesa's scale_fdiv + lower_fdiv for an RCP that flushes denormals.
    // Scaling both operands keeps tiny or huge denominators in RCP's range.
    let big = b.fsetp(FloatCmpOp::OrdGt, y.clone().fabs(), 0x7e80_0000_u32.into());
    let small = b.fsetp(FloatCmpOp::OrdLt, y.clone().fabs(), 0x0080_0000_u32.into());
    let down_x = fmul(b, x.clone(), 0.25_f32.to_bits().into());
    let down_y = fmul(b, y.clone(), 0.25_f32.to_bits().into());
    let up_x = fmul(b, x.clone(), 16_777_216_f32.to_bits().into());
    let up_y = fmul(b, y.clone(), 16_777_216_f32.to_bits().into());
    let x = b.sel(small.into(), up_x, x);
    let y = b.sel(small.into(), up_y, y);
    let x = b.sel(big.into(), down_x, x.into());
    let y = b.sel(big.into(), down_y, y.into());
    let reciprocal = b.mufu(MuFuOp::Rcp, y.into(), FloatType::F32);
    fmul(b, x.into(), reciprocal.into())
}

/// Restoring division uses only integer arithmetic. The high carry preserves
/// the 33rd bit of the tentative remainder before its wrapped subtraction.
fn unsigned_divrem(b: &mut SSAInstrBuilder<'_>, n: Src, d: Src) -> (Src, Src) {
    let mut q: Src = 0_u32.into();
    let mut r: Src = 0_u32.into();
    for bit in (0..32_u32).rev() {
        let high = b.isetp(IntCmpType::I32, IntCmpOp::Lt, r.clone(), 0_u32.into());
        let shifted = b.shl(r, 1_u32.into());
        let next = b.shr(n.clone(), bit.into(), false);
        let next = b.lop2(LogicOp2::And, next.into(), 1_u32.into());
        let tentative = b.lop2(LogicOp2::Or, shifted.into(), next.into());
        let take = b.alloc_ssa(RegFile::Pred);
        b.push_op(OpISetP {
            dst: take.into(),
            set_op: PredSetOp::Or,
            cmp_op: IntCmpOp::Ge,
            cmp_type: IntCmpType::U32,
            ex: false,
            srcs: [tentative.into(), d.clone()],
            accum: high.into(),
            low_cmp: true.into(),
        });
        let difference = b.iadd(tentative.into(), d.clone().ineg(), 0_u32.into());
        r = b
            .sel(take.into(), difference.into(), tentative.into())
            .into();
        let q_bit = b.sel(take.into(), (1_u32 << bit).into(), 0_u32.into());
        q = b.lop2(LogicOp2::Or, q, q_bit.into()).into();
    }
    (q, r)
}

fn divrem(b: &mut SSAInstrBuilder<'_>, n: Src, d: Src, signed: bool, remainder: bool) -> Src {
    let zero = b.isetp(IntCmpType::U32, IntCmpOp::Eq, d.clone(), 0_u32.into());
    let result = if signed {
        let n_negative = b.isetp(IntCmpType::I32, IntCmpOp::Lt, n.clone(), 0_u32.into());
        let d_negative = b.isetp(IntCmpType::I32, IntCmpOp::Lt, d.clone(), 0_u32.into());
        let magnitude_n = b.iabs(n);
        let magnitude_d = b.iabs(d);
        let (q, r) = unsigned_divrem(b, magnitude_n.into(), magnitude_d.into());
        let negative = if remainder {
            n_negative
        } else {
            b.lop2(LogicOp2::Xor, n_negative.into(), d_negative.into())
        };
        let magnitude = if remainder { r } else { q };
        let negated = b.ineg(magnitude.clone());
        b.sel(negative.into(), negated.into(), magnitude).into()
    } else {
        let (q, r) = unsigned_divrem(b, n, d);
        if remainder { r } else { q }
    };
    // TGSI mandates UINT32_MAX for both unsigned operations on zero. Signed
    // division is unspecified; return zero, as the TGSI reference interpreter
    // does. IMOD is the virgl frontend's signed remainder extension.
    b.sel(
        zero.into(),
        if signed {
            0_u32.into()
        } else {
            u32::MAX.into()
        },
        result,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Program {
        instrs: Vec<Instr>,
        result: Src,
        inputs: [SSAValue; 3],
        ssa_count: usize,
    }

    impl Program {
        fn new(opcode: &str, arity: usize) -> Self {
            let sm = ShaderModelInfo::new(52, 32);
            let mut alloc = SSAValueAllocator::new();
            let mut b = SSAInstrBuilder::new(&sm, &mut alloc);
            let inputs = std::array::from_fn(|_| b.alloc_ssa(RegFile::GPR));
            let args: Vec<Src> = inputs[..arity].iter().copied().map(Into::into).collect();
            let result = lower(&mut b, opcode, &args).unwrap();
            let instrs = b.into_vec();
            Self {
                instrs,
                result,
                inputs,
                ssa_count: alloc.max_idx() as usize,
            }
        }

        /// Execute the emitted integer IR instead of testing a host copy of the
        /// division algorithm. Float operations here only test their IR wiring;
        /// transcendental accuracy remains a hardware property.
        fn evaluate(&self, inputs: [u32; 3]) -> u32 {
            let mut values = vec![0_u32; self.ssa_count];
            for (ssa, value) in self.inputs.iter().zip(inputs) {
                values[ssa.idx() as usize] = value;
            }
            for instr in &self.instrs {
                let src = |s: &Src| read(&values, s);
                let (dst, value) = match &instr.op {
                    Op::Copy(op) => (&op.dst, src(&op.src)),
                    Op::Lop2(op) => {
                        let x = src(&op.srcs[0]);
                        let y = src(&op.srcs[1]);
                        (
                            &op.dst,
                            match op.op {
                                LogicOp2::And => x & y,
                                LogicOp2::Or => x | y,
                                LogicOp2::Xor => x ^ y,
                                LogicOp2::PassB => y,
                            },
                        )
                    }
                    Op::IAdd2(op) => (&op.dst, src(&op.srcs[0]).wrapping_add(src(&op.srcs[1]))),
                    Op::IMul(op) => {
                        assert!(!op.high);
                        (&op.dst, src(&op.srcs[0]).wrapping_mul(src(&op.srcs[1])))
                    }
                    Op::I2I(op) => {
                        assert!(op.src_type == IntType::I32 && op.dst_type == IntType::I32);
                        let mut value = src(&op.src) as i32;
                        if op.abs {
                            value = value.wrapping_abs();
                        }
                        if op.neg {
                            value = value.wrapping_neg();
                        }
                        (&op.dst, value as u32)
                    }
                    Op::IMnMx(op) => {
                        let (x, y) = (src(&op.srcs[0]), src(&op.srcs[1]));
                        let min = src(&op.min) != 0;
                        let value = match op.cmp_type {
                            IntCmpType::I32 => {
                                (if min {
                                    (x as i32).min(y as i32)
                                } else {
                                    (x as i32).max(y as i32)
                                }) as u32
                            }
                            IntCmpType::U32 => {
                                if min {
                                    x.min(y)
                                } else {
                                    x.max(y)
                                }
                            }
                        };
                        (&op.dst, value)
                    }
                    Op::Shl(op) => {
                        let shift = src(&op.shift);
                        let value = if op.wrap {
                            src(&op.src) << (shift & 31)
                        } else {
                            src(&op.src).checked_shl(shift).unwrap_or(0)
                        };
                        (&op.dst, value)
                    }
                    Op::Shr(op) => {
                        let shift = src(&op.shift);
                        let shift = if op.wrap { shift & 31 } else { shift.min(32) };
                        let value = if op.signed {
                            ((src(&op.src) as i32) >> shift.min(31)) as u32
                        } else {
                            src(&op.src).checked_shr(shift).unwrap_or(0)
                        };
                        (&op.dst, value)
                    }
                    Op::ISetP(op) => {
                        assert!(!op.ex);
                        let x = src(&op.srcs[0]);
                        let y = src(&op.srcs[1]);
                        let order = match op.cmp_type {
                            IntCmpType::I32 => (x as i32).cmp(&(y as i32)),
                            IntCmpType::U32 => x.cmp(&y),
                        };
                        let cmp = match op.cmp_op {
                            IntCmpOp::False => false,
                            IntCmpOp::True => true,
                            IntCmpOp::Eq => order.is_eq(),
                            IntCmpOp::Ne => !order.is_eq(),
                            IntCmpOp::Lt => order.is_lt(),
                            IntCmpOp::Le => !order.is_gt(),
                            IntCmpOp::Gt => order.is_gt(),
                            IntCmpOp::Ge => !order.is_lt(),
                        };
                        (
                            &op.dst,
                            pred_combine(op.set_op, cmp, src(&op.accum) != 0) as u32,
                        )
                    }
                    Op::PSetP(op) => {
                        let cmp =
                            pred_combine(op.ops[0], src(&op.srcs[0]) != 0, src(&op.srcs[1]) != 0);
                        (
                            &op.dsts[0],
                            pred_combine(op.ops[1], cmp, src(&op.srcs[2]) != 0) as u32,
                        )
                    }
                    Op::Sel(op) => (
                        &op.dst,
                        src(&op.srcs[if src(&op.cond) != 0 { 0 } else { 1 }]),
                    ),
                    Op::FAdd(op) => (
                        &op.dst,
                        (f32::from_bits(src(&op.srcs[0])) + f32::from_bits(src(&op.srcs[1])))
                            .to_bits(),
                    ),
                    Op::FMul(op) => (
                        &op.dst,
                        (f32::from_bits(src(&op.srcs[0])) * f32::from_bits(src(&op.srcs[1])))
                            .to_bits(),
                    ),
                    Op::FMnMx(op) => {
                        let x = f32::from_bits(src(&op.srcs[0]));
                        let y = f32::from_bits(src(&op.srcs[1]));
                        (
                            &op.dst,
                            (if src(&op.min) != 0 {
                                x.min(y)
                            } else {
                                x.max(y)
                            })
                            .to_bits(),
                        )
                    }
                    Op::FSetP(op) => {
                        let cmp = float_cmp(
                            op.cmp_op,
                            f32::from_bits(src(&op.srcs[0])),
                            f32::from_bits(src(&op.srcs[1])),
                        );
                        (
                            &op.dst,
                            pred_combine(op.set_op, cmp, src(&op.accum) != 0) as u32,
                        )
                    }
                    Op::FSet(op) => {
                        let cmp = float_cmp(
                            op.cmp_op,
                            f32::from_bits(src(&op.srcs[0])),
                            f32::from_bits(src(&op.srcs[1])),
                        );
                        (&op.dst, (if cmp { 1_f32 } else { 0_f32 }).to_bits())
                    }
                    Op::F2F(op) => {
                        assert!(op.integer_rnd);
                        let x = f32::from_bits(src(&op.src));
                        let value = match op.rnd_mode {
                            FRndMode::NearestEven => x.round_ties_even(),
                            FRndMode::NegInf => x.floor(),
                            FRndMode::PosInf => x.ceil(),
                            FRndMode::Zero => x.trunc(),
                        };
                        (&op.dst, value.to_bits())
                    }
                    Op::F2I(op) => {
                        assert!(op.rnd_mode == FRndMode::Zero);
                        let x = f32::from_bits(src(&op.src));
                        (
                            &op.dst,
                            match op.dst_type {
                                IntType::I32 => (x as i32) as u32,
                                IntType::U32 => x as u32,
                                _ => panic!("unexpected integer conversion size"),
                            },
                        )
                    }
                    Op::I2F(op) => {
                        let x = src(&op.src);
                        (
                            &op.dst,
                            match op.src_type {
                                IntType::I32 => (x as i32 as f32).to_bits(),
                                IntType::U32 => (x as f32).to_bits(),
                                _ => panic!("unexpected integer conversion size"),
                            },
                        )
                    }
                    Op::Rro(op) => (&op.dst, src(&op.src)),
                    Op::MuFu(op) => {
                        let x = f32::from_bits(src(&op.src));
                        let value = match op.op {
                            MuFuOp::Rcp => x.recip(),
                            MuFuOp::Rsq => x.sqrt().recip(),
                            MuFuOp::Sqrt => x.sqrt(),
                            MuFuOp::Sin => x.sin(),
                            MuFuOp::Cos => x.cos(),
                            MuFuOp::Exp2 => x.exp2(),
                            MuFuOp::Log2 => x.log2(),
                            _ => panic!("unexpected transcendental operation"),
                        };
                        (&op.dst, value.to_bits())
                    }
                    op => panic!("unsupported test instruction: {op}"),
                };
                let Dst::SSA(ssa) = dst else {
                    panic!("expected scalar SSA destination")
                };
                assert_eq!(ssa.len(), 1);
                values[ssa[0].idx() as usize] = value;
            }
            read(&values, &self.result)
        }
    }

    fn read(values: &[u32], src: &Src) -> u32 {
        let value = match &src.src_ref {
            SrcRef::Zero | SrcRef::False => 0,
            SrcRef::True => 1,
            SrcRef::Imm32(value) => *value,
            SrcRef::SSA(ssa) => {
                assert_eq!(ssa.len(), 1);
                values[ssa[0].idx() as usize]
            }
            _ => panic!("unexpected test source"),
        };
        match src.src_mod {
            SrcMod::None => value,
            SrcMod::INeg => value.wrapping_neg(),
            SrcMod::BNot => {
                if src.is_predicate() {
                    (value == 0) as u32
                } else {
                    !value
                }
            }
            SrcMod::FNeg => value ^ 0x8000_0000,
            SrcMod::FAbs => value & 0x7fff_ffff,
            SrcMod::FNegAbs => value | 0x8000_0000,
        }
    }

    fn pred_combine(op: PredSetOp, x: bool, y: bool) -> bool {
        match op {
            PredSetOp::And => x && y,
            PredSetOp::Or => x || y,
            PredSetOp::Xor => x ^ y,
        }
    }

    fn float_cmp(op: FloatCmpOp, x: f32, y: f32) -> bool {
        match op {
            FloatCmpOp::OrdEq => x == y,
            FloatCmpOp::OrdNe => !x.is_nan() && !y.is_nan() && x != y,
            FloatCmpOp::OrdLt => x < y,
            FloatCmpOp::OrdLe => x <= y,
            FloatCmpOp::OrdGt => x > y,
            FloatCmpOp::OrdGe => x >= y,
            FloatCmpOp::UnordEq => x.is_nan() || y.is_nan() || x == y,
            FloatCmpOp::UnordNe => x != y,
            FloatCmpOp::UnordLt => !(x >= y),
            FloatCmpOp::UnordLe => !(x > y),
            FloatCmpOp::UnordGt => !(x <= y),
            FloatCmpOp::UnordGe => !(x < y),
            FloatCmpOp::IsNum => !x.is_nan() && !y.is_nan(),
            FloatCmpOp::IsNan => x.is_nan() || y.is_nan(),
        }
    }

    #[test]
    fn every_virgl_scalar_opcode_lowers_to_typed_ir() {
        for opcode in [
            "MOV", "ABS", "EX2", "LG2", "SIN", "COS", "RSQ", "SQRT", "FRC", "CEIL", "FLR", "ROUND",
            "TRUNC", "NOT", "INEG", "IABS", "F2I", "F2U", "I2F", "U2F",
        ] {
            assert!(!Program::new(opcode, 1).instrs.is_empty(), "{opcode}");
        }
        for opcode in [
            "ADD", "SUB", "MUL", "DIV", "POW", "MIN", "MAX", "FSEQ", "FSNE", "FSGE", "FSLT", "SGE",
            "USEQ", "USNE", "ISGE", "ISLT", "USGE", "USLT", "AND", "OR", "XOR", "UADD", "UMUL",
            "IMIN", "IMAX", "UMIN", "UMAX", "SHL", "ISHR", "USHR", "IDIV", "IMOD", "UDIV", "UMOD",
        ] {
            assert!(!Program::new(opcode, 2).instrs.is_empty(), "{opcode}");
        }
        for opcode in ["UCMP", "CMP"] {
            assert!(!Program::new(opcode, 3).instrs.is_empty());
        }
    }

    #[test]
    fn exact_division_evaluates_the_emitted_integer_instructions() {
        let unsigned_q = Program::new("UDIV", 2);
        let unsigned_r = Program::new("UMOD", 2);
        let signed_q = Program::new("IDIV", 2);
        let signed_r = Program::new("IMOD", 2);
        let check = |n: u32, d: u32| {
            let expected_q = if d == 0 { u32::MAX } else { n / d };
            let expected_r = if d == 0 { u32::MAX } else { n % d };
            assert_eq!(
                unsigned_q.evaluate([n, d, 0]),
                expected_q,
                "UDIV({n:#x}, {d:#x})"
            );
            assert_eq!(
                unsigned_r.evaluate([n, d, 0]),
                expected_r,
                "UMOD({n:#x}, {d:#x})"
            );
            let expected_q = if d == 0 {
                0
            } else {
                (n as i32).wrapping_div(d as i32) as u32
            };
            let expected_r = if d == 0 {
                0
            } else {
                (n as i32).wrapping_rem(d as i32) as u32
            };
            assert_eq!(
                signed_q.evaluate([n, d, 0]),
                expected_q,
                "IDIV({}, {})",
                n as i32,
                d as i32
            );
            assert_eq!(
                signed_r.evaluate([n, d, 0]),
                expected_r,
                "IMOD({}, {})",
                n as i32,
                d as i32
            );
        };
        let boundary = [
            0,
            1,
            2,
            3,
            7,
            31,
            32,
            0x7fff_ffff,
            0x8000_0000,
            0xffff_fffd,
            0xffff_fffe,
            u32::MAX,
        ];
        for n in boundary {
            for d in boundary {
                check(n, d);
            }
        }
        let mut random = 0x9e37_79b9_u32;
        for _ in 0..4096 {
            random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = random;
            random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            check(n, random);
        }
        for program in [unsigned_q, unsigned_r, signed_q, signed_r] {
            assert!(
                program
                    .instrs
                    .iter()
                    .all(|i| !matches!(&i.op, Op::F2I(_) | Op::I2F(_) | Op::FMul(_) | Op::MuFu(_)))
            );
        }
    }

    #[test]
    fn comparisons_and_selects_preserve_tgsi_boolean_representations() {
        assert_eq!(Program::new("USEQ", 2).evaluate([7, 7, 0]), u32::MAX);
        assert_eq!(Program::new("USNE", 2).evaluate([7, 7, 0]), 0);
        assert_eq!(Program::new("ISLT", 2).evaluate([u32::MAX, 1, 0]), u32::MAX);
        assert_eq!(Program::new("USLT", 2).evaluate([u32::MAX, 1, 0]), 0);
        assert_eq!(
            Program::new("FSEQ", 2).evaluate([1_f32.to_bits(), 1_f32.to_bits(), 0]),
            u32::MAX
        );
        assert_eq!(
            Program::new("FSNE", 2).evaluate([f32::NAN.to_bits(), 1_f32.to_bits(), 0]),
            u32::MAX
        );
        assert_eq!(
            Program::new("FSGE", 2).evaluate([f32::NAN.to_bits(), 0, 0]),
            0
        );
        assert_eq!(
            Program::new("SGE", 2).evaluate([1_f32.to_bits(), 0, 0]),
            1_f32.to_bits()
        );
        assert_eq!(
            Program::new("UCMP", 3).evaluate([u32::MAX, 0x1234, 0xabcd]),
            0x1234
        );
        assert_eq!(
            Program::new("UCMP", 3).evaluate([0, 0x1234, 0xabcd]),
            0xabcd
        );
        assert_eq!(
            Program::new("CMP", 3).evaluate([(-1_f32).to_bits(), 0x1234, 0xabcd]),
            0x1234
        );
        assert_eq!(
            Program::new("CMP", 3).evaluate([(-0_f32).to_bits(), 0x1234, 0xabcd]),
            0xabcd
        );
    }

    #[test]
    fn integer_math_and_conversions_remain_typed() {
        assert_eq!(Program::new("UADD", 2).evaluate([u32::MAX, 2, 0]), 1);
        assert_eq!(Program::new("UMUL", 2).evaluate([0x8000_0001, 2, 0]), 2);
        assert_eq!(
            Program::new("IABS", 1).evaluate([0x8000_0000, 0, 0]),
            0x8000_0000
        );
        assert_eq!(Program::new("IMIN", 2).evaluate([u32::MAX, 1, 0]), u32::MAX);
        assert_eq!(Program::new("UMIN", 2).evaluate([u32::MAX, 1, 0]), 1);
        assert_eq!(
            Program::new("NOT", 1).evaluate([0x1234_5678, 0, 0]),
            0xedcb_a987
        );
        assert_eq!(Program::new("SHL", 2).evaluate([1, 33, 0]), 2);
        assert_eq!(
            Program::new("ISHR", 2).evaluate([0x8000_0000, 31, 0]),
            u32::MAX
        );
        assert_eq!(Program::new("USHR", 2).evaluate([0x8000_0000, 31, 0]), 1);
        assert_eq!(
            Program::new("F2I", 1).evaluate([(-7.8_f32).to_bits(), 0, 0]),
            (-7_i32) as u32
        );
        assert_eq!(
            Program::new("F2U", 1).evaluate([7.8_f32.to_bits(), 0, 0]),
            7
        );
        assert_eq!(
            Program::new("I2F", 1).evaluate([(-7_i32) as u32, 0, 0]),
            (-7_f32).to_bits()
        );
        assert_eq!(
            Program::new("U2F", 1).evaluate([0x8000_0000, 0, 0]),
            2_147_483_648_f32.to_bits()
        );
    }

    #[test]
    fn float_rounding_and_fraction_wire_the_correct_rounding_mode() {
        assert_eq!(
            Program::new("ROUND", 1).evaluate([2.5_f32.to_bits(), 0, 0]),
            2_f32.to_bits()
        );
        assert_eq!(
            Program::new("ROUND", 1).evaluate([3.5_f32.to_bits(), 0, 0]),
            4_f32.to_bits()
        );
        assert_eq!(
            Program::new("FLR", 1).evaluate([(-1.25_f32).to_bits(), 0, 0]),
            (-2_f32).to_bits()
        );
        assert_eq!(
            Program::new("CEIL", 1).evaluate([(-1.25_f32).to_bits(), 0, 0]),
            (-1_f32).to_bits()
        );
        assert_eq!(
            Program::new("TRUNC", 1).evaluate([(-1.25_f32).to_bits(), 0, 0]),
            (-1_f32).to_bits()
        );
        assert_eq!(
            Program::new("FRC", 1).evaluate([(-1.25_f32).to_bits(), 0, 0]),
            0.75_f32.to_bits()
        );
        assert_eq!(
            Program::new("DIV", 2).evaluate([2_f32.to_bits(), 1_f32.to_bits(), 0]),
            2_f32.to_bits()
        );
        assert_eq!(
            Program::new("DIV", 2).evaluate([f32::MAX.to_bits(), f32::MAX.to_bits(), 0]),
            1_f32.to_bits()
        );
        assert_eq!(
            Program::new("DIV", 2).evaluate([
                f32::from_bits(1).to_bits(),
                f32::from_bits(1).to_bits(),
                0
            ]),
            1_f32.to_bits()
        );
    }

    #[test]
    fn source_modifiers_materialize_bit_exactly_for_mov() {
        let sm = ShaderModelInfo::new(52, 32);
        let mut alloc = SSAValueAllocator::new();
        let mut b = SSAInstrBuilder::new(&sm, &mut alloc);
        let result = lower(&mut b, "MOV", &[Src::from(0x7fc1_2345_u32).fneg()]).unwrap();
        let instrs = b.into_vec();
        let inputs = std::array::from_fn(|_| alloc.alloc(RegFile::GPR));
        let p = Program {
            instrs,
            result,
            inputs,
            ssa_count: alloc.max_idx() as usize,
        };
        assert_eq!(p.evaluate([0; 3]), 0xffc1_2345);
    }

    #[test]
    fn unsupported_opcode_and_bad_arity_fail_before_emitting_instructions() {
        let sm = ShaderModelInfo::new(52, 32);
        let mut alloc = SSAValueAllocator::new();
        let mut b = SSAInstrBuilder::new(&sm, &mut alloc);
        assert!(lower(&mut b, "UNKNOWN", &[]).is_err());
        assert!(lower(&mut b, "UCMP", &[0_u32.into()]).is_err());
        assert!(b.into_vec().is_empty());
    }
}

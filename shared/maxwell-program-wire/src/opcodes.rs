// SPDX-License-Identifier: MIT
// Encoding audit: Mesa NAK sm50.rs at
// e881540692daac6532cefec76699f7a025563767.
// The source encoder is Copyright (c) 2023 Collabora, Ltd.
//
// This is an exact computational-instruction allowlist. `mask` contains only
// fields actually written by the pinned encoder, never an opcode-prefix mask.
// Sparse enums and dependent operand forms become separate entries. Scheduling
// is carried by a separate instruction word and is deliberately absent here.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CbSource {
    None,
    /// Word-aligned constant-buffer reference, at the given bit position.
    Word(u8),
    /// Two consecutive constant-buffer words, at the given bit position.
    Double(u8),
}

#[derive(Clone, Copy, Debug)]
pub struct OpEncoding {
    pub fixed: u64,
    pub mask: u64,
    pub cb: CbSource,
    /// GPR fields as (bit position, number of consecutive 32-bit registers).
    /// Predicate and carry registers are not included.
    pub regs: &'static [(u8, u8)],
    pub fp64: bool,
    #[allow(dead_code)] // Human-readable encoding audit and test diagnostics.
    pub name: &'static str,
}

impl OpEncoding {
    pub fn matches(&self, instruction: u64) -> bool {
        instruction & !self.mask == self.fixed
    }
}

const fn b(bit: u32) -> u64 {
    1u64 << bit
}
const fn r(start: u32, end: u32) -> u64 {
    ((1u64 << (end - start)) - 1) << start
}
const PRED: u64 = r(16, 20);
const DST: u64 = r(0, 8);
const SRC0: u64 = r(8, 16);
const REG1: u64 = r(20, 28);
const REG2: u64 = r(39, 47);
const IMM20: u64 = r(20, 39) | b(56);
const CB20: u64 = r(20, 39);
const IMM32: u64 = r(20, 52);

const W_DST: &[(u8, u8)] = &[(0, 1)];
const W_UNARY: &[(u8, u8)] = &[(0, 1), (20, 1)];
const W_S0: &[(u8, u8)] = &[(0, 1), (8, 1)];
const W_BINARY: &[(u8, u8)] = &[(0, 1), (8, 1), (20, 1)];
const W_S0_S2: &[(u8, u8)] = &[(0, 1), (8, 1), (39, 1)];
const W_TERNARY: &[(u8, u8)] = &[(0, 1), (8, 1), (20, 1), (39, 1)];
const W_PRED_S0: &[(u8, u8)] = &[(8, 1)];
const W_PRED_BINARY: &[(u8, u8)] = &[(8, 1), (20, 1)];
const D_S0: &[(u8, u8)] = &[(0, 2), (8, 2)];
const D_BINARY: &[(u8, u8)] = &[(0, 2), (8, 2), (20, 2)];
const D_S0_S2: &[(u8, u8)] = &[(0, 2), (8, 2), (39, 2)];
const D_TERNARY: &[(u8, u8)] = &[(0, 2), (8, 2), (20, 2), (39, 2)];
const D_PRED_S0: &[(u8, u8)] = &[(8, 2)];
const D_PRED_BINARY: &[(u8, u8)] = &[(8, 2), (20, 2)];

// All SM50 ALU CBuf forms below, including the third-source FMA/MAD
// forms, place the CBuf descriptor at 20..39. The other source moves to 39..47.
#[derive(Clone, Copy)]
struct Family {
    mask: u64,
    src_mod: u64,
    imm_mask: u64,
    clear: u64,
    set: u64,
    cb: CbSource,
    regs_r: &'static [(u8, u8)],
    regs_ic: &'static [(u8, u8)],
    fp64: bool,
    name: &'static str,
}

const fn word_binary(name: &'static str, mask: u64) -> Family {
    Family {
        mask: mask | DST | SRC0,
        src_mod: 0,
        imm_mask: IMM20,
        clear: 0,
        set: 0,
        cb: CbSource::Word(20),
        regs_r: W_BINARY,
        regs_ic: W_S0,
        fp64: false,
        name,
    }
}

const fn double_binary(name: &'static str, mask: u64) -> Family {
    let mut f = word_binary(name, mask);
    f.cb = CbSource::Double(20);
    f.regs_r = D_BINARY;
    f.regs_ic = D_S0;
    f.fp64 = true;
    f
}

const EMPTY: OpEncoding = OpEncoding {
    fixed: 0,
    mask: 0,
    cb: CbSource::None,
    regs: &[],
    fp64: false,
    name: "",
};
struct Table<const N: usize> {
    entries: [OpEncoding; N],
    len: usize,
}

impl<const N: usize> Table<N> {
    const fn push(
        &mut self,
        opcode: u16,
        mask: u64,
        clear: u64,
        set: u64,
        cb: CbSource,
        regs: &'static [(u8, u8)],
        fp64: bool,
        name: &'static str,
    ) {
        let mask = mask | PRED;
        // set_opcode initially writes 48..64. Every subsequent fixed field
        // write clears its old opcode bits before supplying its new value.
        let fixed = (((opcode as u64) << 48) & !(mask | clear)) | set;
        assert!(fixed & mask == 0);
        if N != 0 {
            assert!(self.len < N);
            self.entries[self.len] = OpEncoding {
                fixed,
                mask,
                cb,
                regs,
                fp64,
                name,
            };
        }
        self.len += 1;
    }

    const fn triple(&mut self, opcodes: [u16; 3], f: Family) {
        self.push(
            opcodes[0],
            f.mask | REG1 | f.src_mod,
            f.clear,
            f.set,
            CbSource::None,
            f.regs_r,
            f.fp64,
            f.name,
        );
        self.push(
            opcodes[1],
            f.mask | f.imm_mask,
            f.clear,
            f.set,
            CbSource::None,
            f.regs_ic,
            f.fp64,
            f.name,
        );
        self.push(
            opcodes[2],
            f.mask | CB20 | f.src_mod,
            f.clear,
            f.set,
            f.cb,
            f.regs_ic,
            f.fp64,
            f.name,
        );
    }
}

const fn build<const N: usize>() -> Table<N> {
    let mut t = Table {
        entries: [EMPTY; N],
        len: 0,
    };

    // FADD: short source fabs/fneg are absent on immediate form. FADD32I
    // overwrites opcode bits 48..51 with immediate bits and has no rounding
    // or saturation field in the pinned encoder.
    let mut f = word_binary("FADD", r(39, 41) | b(44) | b(46) | b(48) | b(50));
    f.src_mod = b(49) | b(45);
    t.triple([0x5c58, 0x3858, 0x4c58], f);
    t.push(
        0x0800,
        DST | SRC0 | IMM32 | b(54) | b(56) | b(55),
        0,
        0,
        CbSource::None,
        W_S0,
        false,
        "FADD32I",
    );

    // FFMA: bits 48..54 are arithmetic modifiers, not opcode discriminator
    // bits. The CB-in-src2 form is a distinct opcode with src1 at 39..47.
    let mut f = word_binary("FFMA", REG2 | r(48, 55));
    f.regs_r = W_TERNARY;
    f.regs_ic = W_S0_S2;
    t.triple([0x5980, 0x3280, 0x4980], f);
    t.push(
        0x5180,
        f.mask | CB20,
        0,
        0,
        CbSource::Word(20),
        W_S0_S2,
        false,
        "FFMA.CB2",
    );

    let mut f = word_binary("FMNMX", r(39, 43) | b(44) | b(46) | b(48));
    f.src_mod = b(49) | b(45);
    t.triple([0x5c60, 0x3860, 0x4c60], f);

    let f = word_binary("FMUL", r(39, 41) | b(44) | b(45) | b(48) | b(50));
    t.triple([0x5c68, 0x3868, 0x4c68], f);
    t.push(
        0x1e00,
        DST | SRC0 | IMM32 | r(53, 56),
        0,
        0,
        CbSource::None,
        W_S0,
        false,
        "FMUL32I",
    );

    let mut f = word_binary("RRO", DST | b(39));
    f.mask &= !SRC0;
    f.src_mod = b(49) | b(45);
    f.regs_r = W_UNARY;
    f.regs_ic = W_DST;
    t.triple([0x5c90, 0x3890, 0x4c90], f);

    // GM20B is SM53: MUFU.SQRT (SM52+) is available. TANH and selector
    // values 9..15 are absent. RCP64H/RSQ64H each consume one high word.
    let mut mufu = 0u64;
    while mufu <= 8 {
        t.push(
            0x5080,
            DST | SRC0 | b(46) | b(48),
            r(20, 24),
            mufu << 20,
            CbSource::None,
            W_S0,
            mufu == 6 || mufu == 7,
            "MUFU",
        );
        mufu += 1;
    }

    // Float comparisons allow 1..14 only. FSET always emits a true
    // accumulator and bool-float result. Note the pinned FSET.CB source
    // fneg write at bit 6 is overwritten by the GPR destination.
    let mut cmp = 1u64;
    while cmp <= 14 {
        let mut f = word_binary("FSET", b(54) | b(43) | b(55));
        f.clear = r(48, 53);
        f.set = (cmp << 48) | b(52) | (7 << 39);
        f.src_mod = b(44) | b(53);
        t.push(
            0x5800,
            f.mask | REG1 | f.src_mod,
            f.clear,
            f.set,
            CbSource::None,
            W_BINARY,
            false,
            f.name,
        );
        t.push(
            0x3000,
            f.mask | IMM20,
            f.clear,
            f.set,
            CbSource::None,
            W_S0,
            false,
            f.name,
        );
        t.push(
            0x4800,
            f.mask | CB20 | b(44),
            f.clear,
            f.set,
            CbSource::Word(20),
            W_S0,
            false,
            f.name,
        );

        let mut set_op = 0u64;
        while set_op <= 2 {
            let mut f = word_binary("FSETP", r(3, 6) | b(7) | SRC0 | r(39, 43) | b(43) | b(47));
            f.mask = (f.mask & !DST) | r(3, 6) | b(7);
            f.src_mod = b(44) | b(6);
            f.clear = r(45, 47) | r(48, 52);
            f.set = 7 | (set_op << 45) | (cmp << 48);
            f.regs_r = W_PRED_BINARY;
            f.regs_ic = W_PRED_S0;
            t.triple([0x5bb0, 0x36b0, 0x4bb0], f);

            // The DSETP.CB branch calls the register-source helper with a
            // 19-bit field and a CBuf source; it panics and is not emitted.
            f.name = "DSETP";
            f.mask &= !b(47);
            f.fp64 = true;
            t.push(
                0x5b80,
                f.mask | REG1 | f.src_mod,
                f.clear,
                f.set,
                CbSource::None,
                D_PRED_BINARY,
                true,
                f.name,
            );
            t.push(
                0x3680,
                f.mask | IMM20,
                f.clear,
                f.set,
                CbSource::None,
                D_PRED_S0,
                true,
                f.name,
            );
            set_op += 1;
        }
        cmp += 1;
    }

    // FSWZADD: four two-bit operations, rounding, derivative mode, FTZ.
    t.push(
        0x50f8,
        DST | SRC0 | REG1 | r(28, 36) | b(38) | r(39, 41) | b(44),
        b(47),
        0,
        CbSource::None,
        W_BINARY,
        false,
        "FSWZADD",
    );

    let mut f = double_binary("DADD", r(39, 41) | b(46) | b(48));
    f.src_mod = b(49) | b(45);
    t.triple([0x5c70, 0x3870, 0x4c70], f);
    let mut f = double_binary("DFMA", REG2 | r(48, 52));
    f.regs_r = D_TERNARY;
    f.regs_ic = D_S0_S2;
    t.triple([0x5b70, 0x3670, 0x4b70], f);
    t.push(
        0x5370,
        f.mask | CB20,
        0,
        0,
        CbSource::Double(20),
        D_S0_S2,
        true,
        "DFMA.CB2",
    );
    let mut f = double_binary("DMNMX", r(39, 43) | b(46) | b(48));
    f.src_mod = b(49) | b(45);
    t.triple([0x5c50, 0x3850, 0x4c50], f);
    t.triple(
        [0x5c80, 0x3880, 0x4c80],
        double_binary("DMUL", r(39, 41) | b(48)),
    );

    let mut f = word_binary("BFE", b(48) | b(40));
    f.imm_mask = r(20, 36); // Encoder explicitly discards all higher bits.
    t.triple([0x5c00, 0x3800, 0x4c00], f);
    let mut f = word_binary("FLO", DST | b(40) | b(48) | b(41));
    f.mask &= !SRC0;
    f.clear = b(47);
    f.regs_r = W_UNARY;
    f.regs_ic = W_DST;
    t.push(
        0x5c30,
        f.mask | REG1,
        f.clear,
        0,
        CbSource::None,
        W_UNARY,
        false,
        "FLO",
    );
    // The immediate branch asserts an unmodified source, hence no BNOT.
    t.push(
        0x3830,
        (f.mask & !b(40)) | IMM20,
        f.clear | b(40),
        0,
        CbSource::None,
        W_DST,
        false,
        "FLO.I",
    );
    t.push(
        0x4c30,
        f.mask | CB20,
        f.clear,
        0,
        CbSource::Word(20),
        W_DST,
        false,
        "FLO.CB",
    );

    // IADD and IADD.X share opcodes; bit 43 (short) / 53 (long)
    // selects carry-in. Plain IADD forbids simultaneous source negation.
    let mut ex = 0u64;
    while ex <= 1 {
        let mut negs = 0u64;
        while negs < 4 {
            if ex != 0 || negs != 3 {
                let f = word_binary("IADD", b(47));
                let set = (ex << 43) | ((negs & 1) << 49) | ((negs >> 1) << 48);
                t.push(
                    0x5c10,
                    f.mask | REG1,
                    b(43) | b(48) | b(49),
                    set,
                    CbSource::None,
                    W_BINARY,
                    false,
                    "IADD",
                );
                t.push(
                    0x4c10,
                    f.mask | CB20,
                    b(43) | b(48) | b(49),
                    set,
                    CbSource::Word(20),
                    W_S0,
                    false,
                    "IADD.CB",
                );
            }
            negs += 1;
        }
        t.push(
            0x3810,
            DST | SRC0 | IMM20 | b(49) | b(47),
            b(43),
            ex << 43,
            CbSource::None,
            W_S0,
            false,
            "IADD.I20",
        );
        t.push(
            0x1c00,
            DST | SRC0 | IMM32 | b(56) | b(52),
            b(53),
            ex << 53,
            CbSource::None,
            W_S0,
            false,
            "IADD32I",
        );
        ex += 1;
    }

    // IMAD's two signedness bits are tied by one bool.
    let mut signed = 0u64;
    while signed <= 1 {
        let mut f = word_binary("IMAD", REG2 | b(51) | b(52));
        f.clear = b(48) | b(53);
        f.set = (signed << 48) | (signed << 53);
        f.regs_r = W_TERNARY;
        f.regs_ic = W_S0_S2;
        t.triple([0x5a00, 0x3400, 0x4a00], f);
        t.push(
            0x5200,
            f.mask | CB20,
            f.clear,
            f.set,
            CbSource::Word(20),
            W_S0_S2,
            false,
            "IMAD.CB2",
        );
        signed += 1;
    }
    t.triple([0x5c38, 0x3838, 0x4c38], word_binary("IMUL", r(39, 42)));
    t.push(
        0x1fc0,
        DST | SRC0 | IMM32 | r(53, 56),
        0,
        0,
        CbSource::None,
        W_S0,
        false,
        "IMUL32I",
    );
    let mut f = word_binary("IMNMX", r(39, 43) | b(48));
    f.clear = b(47);
    t.triple([0x5c20, 0x3820, 0x4c20], f);

    // Integer comparisons have all eight values. Predicate accumulation
    // has only AND/OR/XOR; ISETP.X is explicitly forbidden.
    let mut set_op = 0u64;
    while set_op <= 2 {
        let mut f = word_binary("ISETP", r(3, 6) | SRC0 | r(39, 43) | r(48, 52));
        f.mask = (f.mask & !DST) | r(3, 6);
        f.clear = b(43) | r(45, 47);
        f.set = 7 | (set_op << 45);
        f.regs_r = W_PRED_BINARY;
        f.regs_ic = W_PRED_S0;
        t.triple([0x5b60, 0x3660, 0x4b60], f);
        set_op += 1;
    }

    let mut f = word_binary("LOP", b(39) | r(41, 43));
    f.src_mod = b(40);
    f.clear = r(48, 51);
    f.set = 7 << 48; // No predicate destination.
    t.triple([0x5c40, 0x3840, 0x4c40], f);
    let mut logic = 0u64;
    while logic <= 2 {
        t.push(
            0x0400,
            DST | SRC0 | IMM32 | b(55) | b(56),
            r(53, 55),
            logic << 53,
            CbSource::None,
            W_S0,
            false,
            "LOP32I",
        );
        logic += 1;
    }
    let mut f = word_binary("POPC", DST | b(40));
    f.mask &= !SRC0;
    f.regs_r = W_UNARY;
    f.regs_ic = W_DST;
    t.triple([0x5c08, 0x3808, 0x4c08], f);

    // SHF type is only 0 (32), 2 (u64), or 3 (i64). Left shifts
    // always clear .HIGH; both directions clear .X and .CC.
    let mut right = 0u64;
    while right <= 1 {
        let mut ty = 0u64;
        while ty <= 3 {
            if ty != 1 {
                let mask = DST | SRC0 | REG2 | b(50) | if right != 0 { b(48) } else { 0 };
                let clear = r(37, 39) | b(47) | b(49) | if right == 0 { b(48) } else { 0 };
                let set = ty << 37;
                let opcode = if right != 0 { 0x5cf8 } else { 0x5bf8 };
                t.push(
                    opcode,
                    mask | REG1,
                    clear,
                    set,
                    CbSource::None,
                    W_TERNARY,
                    false,
                    "SHF",
                );
                let opcode = if right != 0 { 0x38f8 } else { 0x36f8 };
                let shift_bits = if ty == 0 { 5 } else { 6 };
                t.push(
                    opcode,
                    mask | r(20, 20 + shift_bits),
                    clear,
                    set,
                    CbSource::None,
                    W_S0_S2,
                    false,
                    "SHF.I",
                );
                t.push(
                    opcode,
                    mask & !b(50),
                    clear | b(50),
                    set | b(20 + shift_bits),
                    CbSource::None,
                    W_S0_S2,
                    false,
                    "SHF.I.MAX",
                );
            }
            ty += 1;
        }
        right += 1;
    }
    let mut shr = 0u64;
    while shr <= 1 {
        let mut f = word_binary(
            if shr != 0 { "SHR" } else { "SHL" },
            b(39) | if shr != 0 { b(48) } else { 0 },
        );
        let opcodes = if shr != 0 {
            [0x5c28, 0x3828, 0x4c28]
        } else {
            [0x5c48, 0x3848, 0x4c48]
        };
        f.imm_mask = r(20, 25);
        t.triple(opcodes, f);
        t.push(
            opcodes[1],
            f.mask & !b(39),
            b(39),
            b(25),
            CbSource::None,
            W_S0,
            false,
            "SHIFT.I.MAX",
        );
        shr += 1;
    }

    // Conversions: type code is log2(bytes). Float type codes are 1..3,
    // integer codes 0..3. The cross-span assertion excludes a 64-bit
    // type paired with an 8/16-bit type. GPR/CB width follows source type.
    let mut kind = 0u64;
    while kind < 4 {
        let mut dst_ty = if kind == 0 || kind == 2 { 1u64 } else { 0u64 };
        while dst_ty <= 3 {
            let mut src_ty = if kind == 0 || kind == 1 { 1u64 } else { 0u64 };
            while src_ty <= 3 {
                if !((dst_ty == 3 && src_ty < 2) || (src_ty == 3 && dst_ty < 2)) {
                    let mut f = word_binary("", DST);
                    f.mask &= !SRC0;
                    f.clear = r(8, 12);
                    f.set = (dst_ty << 8) | (src_ty << 10);
                    f.cb = if src_ty == 3 {
                        CbSource::Double(20)
                    } else {
                        CbSource::Word(20)
                    };
                    f.regs_r = match (dst_ty == 3, src_ty == 3) {
                        (false, false) => &[(0, 1), (20, 1)],
                        (false, true) => &[(0, 1), (20, 2)],
                        (true, false) => &[(0, 2), (20, 1)],
                        (true, true) => &[(0, 2), (20, 2)],
                    };
                    f.regs_ic = if dst_ty == 3 { &[(0, 2)] } else { W_DST };
                    f.fp64 = match kind {
                        0 => dst_ty == 3 || src_ty == 3,
                        1 => src_ty == 3,
                        2 => dst_ty == 3,
                        _ => false,
                    };
                    let opcodes = match kind {
                        0 => {
                            f.name = "F2F";
                            f.mask |= r(39, 43) | b(44);
                            f.src_mod = b(49) | b(45);
                            f.imm_mask = r(32, 39) | b(56); // f20/i20 intersection.
                            f.clear |= b(50);
                            [0x5ca8, 0x38a8, 0x4ca8]
                        }
                        1 => {
                            f.name = "F2I";
                            f.mask |= b(12) | r(39, 42) | b(44);
                            f.src_mod = b(49) | b(45);
                            f.clear |= b(47);
                            [0x5cb0, 0x38b0, 0x4cb0]
                        }
                        2 => {
                            f.name = "I2F";
                            f.mask |= b(13) | r(39, 41);
                            f.src_mod = b(45);
                            f.clear |= r(41, 43) | b(49);
                            [0x5cb8, 0x38b8, 0x4cb8]
                        }
                        _ => {
                            f.name = "I2I";
                            f.mask |= b(12) | b(13) | b(45) | b(49) | b(50);
                            f.clear |= r(41, 43) | b(47);
                            [0x5ce0, 0x38e0, 0x4ce0]
                        }
                    };
                    t.triple(opcodes, f);
                }
                src_ty += 1;
            }
            dst_ty += 1;
        }
        kind += 1;
    }

    t.push(
        0x5c98,
        DST | REG1 | r(39, 43),
        0,
        0,
        CbSource::None,
        W_UNARY,
        false,
        "MOV",
    );
    t.push(
        0x4c98,
        DST | CB20 | r(39, 43),
        0,
        0,
        CbSource::Word(20),
        W_DST,
        false,
        "MOV.CB",
    );
    t.push(
        0x0100,
        DST | IMM32 | r(12, 16),
        0,
        0,
        CbSource::None,
        W_DST,
        false,
        "MOV32I",
    );

    // PRMT mode 7 is reserved. The selector immediate was reduced to u16.
    let mut mode = 0u64;
    while mode <= 6 {
        let mut f = word_binary("PRMT", REG2);
        f.clear = r(48, 51);
        f.set = mode << 48;
        f.imm_mask = r(20, 36);
        f.regs_r = W_TERNARY;
        f.regs_ic = W_S0_S2;
        t.triple([0x5bc0, 0x36c0, 0x4bc0], f);
        mode += 1;
    }
    t.triple([0x5ca0, 0x38a0, 0x4ca0], word_binary("SEL", r(39, 43)));

    // SHFL forms have different data masks, and C-immediate has holes
    // corresponding to reduce_lane_c_imm's 0x1f1f mask.
    let mut lane_imm = 0u64;
    while lane_imm <= 1 {
        let mut c_imm = 0u64;
        while c_imm <= 1 {
            let mask = DST
                | SRC0
                | r(30, 32)
                | r(48, 51)
                | if lane_imm != 0 { r(20, 25) } else { REG1 }
                | if c_imm != 0 {
                    r(34, 39) | r(42, 47)
                } else {
                    REG2
                };
            let regs = match (lane_imm != 0, c_imm != 0) {
                (false, false) => W_TERNARY,
                (false, true) => W_BINARY,
                (true, false) => W_S0_S2,
                (true, true) => W_S0,
            };
            t.push(
                0xef10,
                mask,
                b(28) | b(29),
                (lane_imm << 28) | (c_imm << 29),
                CbSource::None,
                regs,
                false,
                "SHFL",
            );
            c_imm += 1;
        }
        lane_imm += 1;
    }

    let mut op0 = 0u64;
    while op0 <= 2 {
        let mut op1 = 0u64;
        while op1 <= 2 {
            t.push(
                0x5090,
                r(0, 6) | r(12, 16) | r(29, 33) | r(39, 43),
                r(24, 26) | r(45, 47),
                (op0 << 24) | (op1 << 45),
                CbSource::None,
                &[],
                false,
                "PSETP",
            );
            op1 += 1;
        }
        op0 += 1;
    }
    t
}

pub const ALU_ENCODING_COUNT: usize = build::<0>().len;
pub static ALU_ENCODINGS: [OpEncoding; ALU_ENCODING_COUNT] = build::<ALU_ENCODING_COUNT>().entries;

pub fn find_alu_encoding(instruction: u64) -> Option<&'static OpEncoding> {
    ALU_ENCODINGS
        .iter()
        .find(|encoding| encoding.matches(instruction))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instruction(opcode: u16, fields: u64) -> u64 {
        ((opcode as u64) << 48) | (7 << 16) | fields
    }

    #[test]
    fn every_entry_keeps_fixed_and_operand_bits_disjoint() {
        assert!(ALU_ENCODING_COUNT > 300);
        for encoding in &ALU_ENCODINGS {
            assert_eq!(encoding.fixed & encoding.mask, 0, "{}", encoding.name);
            assert!(encoding.matches(encoding.fixed));
            assert!(encoding.matches(encoding.fixed | encoding.mask));
            for bit in 0..64 {
                if encoding.mask & b(bit) == 0 {
                    assert!(
                        !encoding.matches(encoding.fixed ^ b(bit)),
                        "{} bit {bit}",
                        encoding.name
                    );
                }
            }
        }
    }

    #[test]
    fn long_immediate_and_opcode_overwrite_forms_are_recognized() {
        assert_eq!(
            find_alu_encoding(instruction(0x0100, 0xdead_beefu64 << 20))
                .unwrap()
                .name,
            "MOV32I"
        );
        let ffma = instruction(0x5980, r(48, 55) | (2 << 39) | (3 << 20) | (4 << 8) | 5);
        assert_eq!(find_alu_encoding(ffma).unwrap().name, "FFMA");
        let cb2 = instruction(0x5180, (4 << 34) | (16 << 20));
        assert_eq!(find_alu_encoding(cb2).unwrap().cb, CbSource::Word(20));
    }

    #[test]
    fn sparse_and_reserved_fields_are_rejected() {
        assert!(find_alu_encoding(instruction(0x5080, 9 << 20)).is_none());
        assert!(find_alu_encoding(instruction(0x5090, 3 << 24)).is_none());
        assert!(find_alu_encoding(instruction(0x5bc0, 7 << 48)).is_none());
        assert!(find_alu_encoding(instruction(0x5c58, b(28))).is_none());
        assert!(find_alu_encoding(instruction(0x5cf8, 1 << 37)).is_none());
        assert!(find_alu_encoding(instruction(0x5ca8, (1 << 8) | (3 << 10))).is_none());
        assert!(find_alu_encoding(instruction(0xef10, b(29) | b(39))).is_none());
    }

    #[test]
    fn memory_control_texture_and_external_opcodes_are_absent() {
        for opcode in [
            0xeed0, 0xeed8, 0xef90, 0xef98, 0xe240, 0xe300, 0xe330, 0xdeb8, 0xc038,
        ] {
            assert!(
                find_alu_encoding(instruction(opcode, 0)).is_none(),
                "{opcode:04x}"
            );
        }
    }
}

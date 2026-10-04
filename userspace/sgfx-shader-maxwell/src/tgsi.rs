//! Bounded parser for the TGSI text emitted by SGFX's VirGL compiler.
//!
//! Unsupported syntax and opcodes are errors. Values retain their scalar bit
//! patterns; in particular an integer immediate is never converted to a float.

use std::string::String;
use std::vec::Vec;

pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_INSTRUCTIONS: usize = 16_385;
pub const MAX_IMMEDIATES: usize = 16_384;
const MAX_DECLARATIONS: usize = 256;
const MAX_CONTROL_DEPTH: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Fragment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RegisterFile {
    In,
    Out,
    Const,
    Temp,
    Samp,
    SView,
    Sv,
    Imm,
}

impl RegisterFile {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "IN" => Ok(Self::In),
            "OUT" => Ok(Self::Out),
            "CONST" => Ok(Self::Const),
            "TEMP" => Ok(Self::Temp),
            "SAMP" => Ok(Self::Samp),
            "SVIEW" => Ok(Self::SView),
            "SV" => Ok(Self::Sv),
            "IMM" => Ok(Self::Imm),
            _ => Err(format!("unsupported register file {name:?}")),
        }
    }

    fn limit(self) -> usize {
        match self {
            Self::In | Self::Out => 32,
            Self::Const | Self::Temp => 1024,
            // VirGL bounds the combined storage-buffer and sampled-image bank.
            Self::Samp | Self::SView => 16,
            Self::Sv => 2,
            Self::Imm => MAX_IMMEDIATES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarType {
    Float,
    Sint,
    Uint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Semantic {
    Position,
    Generic(u16),
    Color(u16),
    Face,
    VertexId,
    InstanceId,
    PointSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interpolation {
    Linear,
    Perspective,
    Constant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextureTarget {
    D2,
    D2Array,
    Cube,
    Shadow2D,
    Shadow2DArray,
    ShadowCube,
    Buffer,
}

impl TextureTarget {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "2D" => Ok(Self::D2),
            "2D_ARRAY" => Ok(Self::D2Array),
            "CUBE" => Ok(Self::Cube),
            "SHADOW2D" => Ok(Self::Shadow2D),
            "SHADOW2D_ARRAY" => Ok(Self::Shadow2DArray),
            "SHADOWCUBE" => Ok(Self::ShadowCube),
            "BUFFER" => Ok(Self::Buffer),
            _ => Err(format!("unsupported texture target {name:?}")),
        }
    }
}

impl std::fmt::Display for TextureTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::D2 => "2D",
            Self::D2Array => "2D_ARRAY",
            Self::Cube => "CUBE",
            Self::Shadow2D => "SHADOW2D",
            Self::Shadow2DArray => "SHADOW2D_ARRAY",
            Self::ShadowCube => "SHADOWCUBE",
            Self::Buffer => "BUFFER",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    pub file: RegisterFile,
    pub first: u16,
    pub last: u16,
    pub semantic: Option<Semantic>,
    pub interpolation: Option<Interpolation>,
    pub texture_target: Option<TextureTarget>,
    pub scalar_type: Option<ScalarType>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Immediate {
    pub index: u16,
    pub kind: ScalarType,
    /// Raw IEEE-754, two's-complement, or unsigned scalar words.
    pub values: [u32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    FsCoordOriginUpperLeft,
    FsCoordPixelCenterHalfInteger,
    FsColor0WritesAllCbufs(bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operand {
    pub file: RegisterFile,
    pub index: u16,
    /// Source lane selection; defaults to xyzw, with scalar swizzles replicated.
    pub swizzle: [u8; 4],
    /// Destination lane mask, with x in bit 0; defaults to all four lanes.
    pub mask: u8,
    pub negate: bool,
    pub absolute: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instr {
    pub number: u32,
    pub opcode: String,
    /// The first argument is a destination except for UIF and zero-argument ops.
    pub args: Vec<Operand>,
    pub texture_target: Option<TextureTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub stage: Stage,
    pub declarations: Vec<Declaration>,
    pub immediates: Vec<Immediate>,
    pub properties: Vec<Property>,
    pub instructions: Vec<Instr>,
}

impl Program {
    pub fn declaration(&self, file: RegisterFile, index: u16) -> Option<&Declaration> {
        self.declarations
            .iter()
            .find(|d| d.file == file && d.first <= index && index <= d.last)
    }
}

/// Parse only the bounded SGFX-emitted graphics subset, including control flow.
pub fn parse(text: &str) -> Result<Program, String> {
    if text.len() > MAX_TEXT_BYTES {
        return Err("TGSI text exceeds 4 MiB".into());
    }
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, s)| !s.trim().is_empty());
    let (header_line, header) = lines
        .next()
        .ok_or_else(|| "empty TGSI program".to_string())?;
    let stage = match header.trim() {
        "VERT" => Stage::Vertex,
        "FRAG" => Stage::Fragment,
        _ => {
            return Err(format!(
                "line {}: expected VERT or FRAG header",
                header_line + 1
            ));
        }
    };
    let mut program = Program {
        stage,
        declarations: Vec::new(),
        immediates: Vec::new(),
        properties: Vec::new(),
        instructions: Vec::new(),
    };
    let mut saw_immediate = false;
    let mut saw_instruction = false;
    let mut ended = false;
    let mut control = Vec::new();
    for (line_number, line) in lines {
        let line = line.trim();
        let result = (|| {
            if ended {
                return Err("text follows END".into());
            }
            if let Some(rest) = line.strip_prefix("PROPERTY ") {
                if saw_immediate || saw_instruction || !program.declarations.is_empty() {
                    return Err("property follows declarations or instructions".into());
                }
                let property = parse_property(rest, stage)?;
                if program
                    .properties
                    .iter()
                    .any(|p| same_property(*p, property))
                {
                    return Err("duplicate property".into());
                }
                program.properties.push(property);
            } else if let Some(rest) = line.strip_prefix("DCL ") {
                if saw_immediate || saw_instruction {
                    return Err("declaration follows immediates or instructions".into());
                }
                if program.declarations.len() >= MAX_DECLARATIONS {
                    return Err("more than 256 declarations".into());
                }
                let declaration = parse_declaration(rest, stage)?;
                if program.declarations.iter().any(|d| {
                    d.file == declaration.file
                        && d.first <= declaration.last
                        && declaration.first <= d.last
                }) {
                    return Err("overlapping register declarations".into());
                }
                program.declarations.push(declaration);
            } else if line.starts_with("IMM[") {
                if saw_instruction {
                    return Err("immediate follows instructions".into());
                }
                saw_immediate = true;
                let immediate = parse_immediate(line)?;
                if program.immediates.len() >= MAX_IMMEDIATES {
                    return Err("more than 16384 immediate registers".into());
                }
                if usize::from(immediate.index) != program.immediates.len() {
                    return Err("immediate indices are not consecutive from zero".into());
                }
                program.immediates.push(immediate);
            } else {
                saw_instruction = true;
                if program.instructions.len() >= MAX_INSTRUCTIONS {
                    return Err("more than 16385 instructions including END".into());
                }
                let instruction = parse_instruction(line)?;
                if instruction.number as usize != program.instructions.len() {
                    return Err("instruction numbers are not consecutive from zero".into());
                }
                validate_operands(&program, &instruction)?;
                validate_control(&instruction, stage, &mut control)?;
                ended = instruction.opcode == "END";
                program.instructions.push(instruction);
            }
            Ok(())
        })();
        result.map_err(|error: String| format!("line {}: {error}", line_number + 1))?;
    }
    if !ended {
        return Err("TGSI program is missing END".into());
    }
    Ok(program)
}

fn same_property(a: Property, b: Property) -> bool {
    std::mem::discriminant(&a) == std::mem::discriminant(&b)
}

fn parse_property(text: &str, stage: Stage) -> Result<Property, String> {
    if stage != Stage::Fragment {
        return Err("fragment property in vertex program".into());
    }
    match text {
        "FS_COORD_ORIGIN UPPER_LEFT" => Ok(Property::FsCoordOriginUpperLeft),
        "FS_COORD_PIXEL_CENTER HALF_INTEGER" => Ok(Property::FsCoordPixelCenterHalfInteger),
        "FS_COLOR0_WRITES_ALL_CBUFS 0" => Ok(Property::FsColor0WritesAllCbufs(false)),
        "FS_COLOR0_WRITES_ALL_CBUFS 1" => Ok(Property::FsColor0WritesAllCbufs(true)),
        _ => Err(format!("unsupported property {text:?}")),
    }
}

fn parse_declaration(text: &str, stage: Stage) -> Result<Declaration, String> {
    let parts: Vec<_> = text.split(',').map(str::trim).take(4).collect();
    if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return Err("invalid declaration qualifiers".into());
    }
    let (file, first, last, suffix) = parse_register(parts[0], true)?;
    if !suffix.is_empty() || file == RegisterFile::Imm {
        return Err("invalid declaration register".into());
    }
    let mut declaration = Declaration {
        file,
        first,
        last,
        semantic: None,
        interpolation: None,
        texture_target: None,
        scalar_type: None,
    };
    match file {
        RegisterFile::In | RegisterFile::Out | RegisterFile::Sv => {
            if first != last {
                return Err("stage-interface declaration ranges are unsupported".into());
            }
            if let Some(semantic) = parts.get(1) {
                declaration.semantic = Some(parse_semantic(semantic)?);
            }
            if let Some(interpolation) = parts.get(2) {
                if file != RegisterFile::In {
                    return Err("interpolation on non-input declaration".into());
                }
                declaration.interpolation = Some(match *interpolation {
                    "LINEAR" => Interpolation::Linear,
                    "PERSPECTIVE" => Interpolation::Perspective,
                    "CONSTANT" => Interpolation::Constant,
                    _ => return Err(format!("unsupported interpolation {interpolation:?}")),
                });
            }
            if file == RegisterFile::Sv
                && (stage != Stage::Vertex
                    || !matches!(
                        declaration.semantic,
                        Some(Semantic::VertexId | Semantic::InstanceId)
                    ))
            {
                return Err("unsupported system-value declaration".into());
            }
            if file != RegisterFile::Sv
                && matches!(
                    declaration.semantic,
                    Some(Semantic::VertexId | Semantic::InstanceId)
                )
            {
                return Err("system-value semantic outside SV register file".into());
            }
        }
        RegisterFile::SView => {
            if first != last || parts.len() != 3 {
                return Err("sampler view requires one slot, target, and scalar type".into());
            }
            declaration.texture_target = Some(TextureTarget::parse(parts[1])?);
            declaration.scalar_type = Some(match parts[2] {
                "FLOAT" => ScalarType::Float,
                "UINT" => ScalarType::Uint,
                "SINT" => ScalarType::Sint,
                _ => return Err("unsupported sampler-view scalar type".into()),
            });
        }
        RegisterFile::Temp | RegisterFile::Const | RegisterFile::Samp => {
            if parts.len() != 1 || (file == RegisterFile::Samp && first != last) {
                return Err("unexpected register declaration qualifier or range".into());
            }
        }
        RegisterFile::Imm => unreachable!(),
    }
    Ok(declaration)
}

fn parse_semantic(text: &str) -> Result<Semantic, String> {
    match text {
        "POSITION" => Ok(Semantic::Position),
        "FACE" => Ok(Semantic::Face),
        "VERTEXID" => Ok(Semantic::VertexId),
        "INSTANCEID" => Ok(Semantic::InstanceId),
        "PSIZE" => Ok(Semantic::PointSize),
        "COLOR" => Ok(Semantic::Color(0)),
        _ => {
            for (prefix, color) in [("COLOR[", true), ("GENERIC[", false)] {
                if let Some(index) = text.strip_prefix(prefix).and_then(|s| s.strip_suffix(']')) {
                    let index = number(index)?;
                    if index >= 16 {
                        return Err("stage-interface location exceeds 15".into());
                    }
                    return Ok(if color {
                        Semantic::Color(index)
                    } else {
                        Semantic::Generic(index)
                    });
                }
            }
            Err(format!("unsupported semantic {text:?}"))
        }
    }
}

fn parse_immediate(text: &str) -> Result<Immediate, String> {
    let (file, index, last, rest) = parse_register(text, false)?;
    if file != RegisterFile::Imm || index != last {
        return Err("invalid immediate register".into());
    }
    let rest = rest.trim_start();
    let (kind, values) = rest
        .split_once(char::is_whitespace)
        .ok_or_else(|| "missing immediate type or values".to_string())?;
    let values = values
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| "immediate values require braces".to_string())?;
    let components: Vec<_> = values.split(',').map(str::trim).take(5).collect();
    if components.len() != 4 {
        return Err("immediate requires exactly four components".into());
    }
    let mut words = [0; 4];
    let scalar_type = match kind {
        "FLT32" => ScalarType::Float,
        "UINT32" => ScalarType::Uint,
        "INT32" => ScalarType::Sint,
        _ => return Err(format!("unsupported immediate type {kind:?}")),
    };
    for (word, component) in words.iter_mut().zip(components) {
        *word = match scalar_type {
            ScalarType::Float => {
                let value = component
                    .parse::<f32>()
                    .map_err(|_| "invalid float immediate")?;
                if !value.is_finite() {
                    return Err("non-finite float immediate".into());
                }
                value.to_bits()
            }
            ScalarType::Uint => component
                .parse::<u32>()
                .map_err(|_| "invalid uint immediate")?,
            ScalarType::Sint => component
                .parse::<i32>()
                .map_err(|_| "invalid sint immediate")? as u32,
        };
    }
    Ok(Immediate {
        index,
        kind: scalar_type,
        values: words,
    })
}

fn number(text: &str) -> Result<u16, String> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid register index {text:?}"));
    }
    text.parse()
        .map_err(|_| "register index exceeds u16".into())
}

fn parse_register(text: &str, range: bool) -> Result<(RegisterFile, u16, u16, &str), String> {
    let (name, rest) = text
        .split_once('[')
        .ok_or_else(|| "missing register index".to_string())?;
    let file = RegisterFile::parse(name)?;
    let (indices, suffix) = rest
        .split_once(']')
        .ok_or_else(|| "unclosed register index".to_string())?;
    let (first, last) = if let Some((first, last)) = indices.split_once("..") {
        if !range {
            return Err("register range in operand".into());
        }
        (number(first)?, number(last)?)
    } else {
        let index = number(indices)?;
        (index, index)
    };
    if first > last || usize::from(last) >= file.limit() {
        return Err(format!("register index exceeds bound for {file:?}"));
    }
    Ok((file, first, last, suffix))
}

fn parse_operand(text: &str, destination: bool) -> Result<Operand, String> {
    let mut text = text.trim();
    let negate = text.starts_with('-');
    if negate {
        text = &text[1..];
    }
    let absolute = text.starts_with('|');
    if absolute {
        text = text
            .strip_prefix('|')
            .and_then(|s| s.strip_suffix('|'))
            .ok_or_else(|| "unclosed absolute modifier".to_string())?;
    }
    if destination && (negate || absolute) {
        return Err("modifier on destination operand".into());
    }
    let (file, index, _, suffix) = parse_register(text, false)?;
    let mut operand = Operand {
        file,
        index,
        swizzle: [0, 1, 2, 3],
        mask: 15,
        negate,
        absolute,
    };
    if suffix.is_empty() {
        return Ok(operand);
    }
    let lanes = suffix
        .strip_prefix('.')
        .ok_or_else(|| "invalid operand suffix".to_string())?;
    let mut components = [0; 4];
    if lanes.is_empty() || lanes.len() > 4 {
        return Err("invalid operand lane count".into());
    }
    for (slot, lane) in components.iter_mut().zip(lanes.bytes()) {
        *slot = match lane {
            b'x' => 0,
            b'y' => 1,
            b'z' => 2,
            b'w' => 3,
            _ => return Err("invalid operand component".into()),
        };
    }
    if destination {
        let mut mask = 0;
        let mut previous = None;
        for &component in &components[..lanes.len()] {
            if previous.is_some_and(|p| p >= component) {
                return Err("destination mask must have unique, ordered lanes".into());
            }
            mask |= 1 << component;
            previous = Some(component);
        }
        operand.mask = mask;
    } else {
        operand.swizzle = match lanes.len() {
            1 => [components[0]; 4],
            4 => components,
            _ => return Err("source swizzle requires one or four lanes".into()),
        };
    }
    Ok(operand)
}

/// Returns (number of register arguments, has destination, has texture target).
fn opcode_signature(opcode: &str) -> Result<(usize, bool, bool), String> {
    match opcode {
        "END" | "ELSE" | "ENDIF" | "BGNLOOP" | "ENDLOOP" | "BRK" | "CONT" | "KILL" => {
            Ok((0, false, false))
        }
        "UIF" => Ok((1, false, false)),
        "MOV" | "INEG" | "NOT" | "F2I" | "F2U" | "I2F" | "U2F" | "IABS" | "ABS" | "FLR"
        | "CEIL" | "ROUND" | "TRUNC" | "FRC" | "SQRT" | "RSQ" | "SIN" | "COS" | "EX2" | "LG2" => {
            Ok((2, true, false))
        }
        "ADD" | "SUB" | "MUL" | "DIV" | "UADD" | "UMUL" | "IDIV" | "UDIV" | "IMOD" | "UMOD"
        | "FSEQ" | "FSNE" | "FSLT" | "FSGE" | "USEQ" | "USNE" | "ISLT" | "ISGE" | "USLT"
        | "USGE" | "AND" | "OR" | "XOR" | "SHL" | "USHR" | "ISHR" | "SGE" | "IMIN" | "UMIN"
        | "IMAX" | "UMAX" | "MIN" | "MAX" | "POW" => Ok((3, true, false)),
        "UCMP" | "CMP" => Ok((4, true, false)),
        "TEX" | "TXL" | "TXB" | "TXF" | "TXQ" => Ok((3, true, true)),
        _ => Err(format!("unsupported opcode {opcode:?}")),
    }
}

fn parse_instruction(text: &str) -> Result<Instr, String> {
    let (index, rest) = text
        .split_once(':')
        .ok_or_else(|| "expected numbered instruction".to_string())?;
    let number = u32::from(number(index.trim())?);
    let rest = rest.trim();
    let (opcode, args_text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (arity, destination, texture) = opcode_signature(opcode)?;
    let parts: Vec<_> = if args_text.trim().is_empty() {
        Vec::new()
    } else {
        args_text.split(',').map(str::trim).take(5).collect()
    };
    if parts.len() != arity + usize::from(texture) {
        return Err(format!("incorrect argument count for {opcode}"));
    }
    let args = parts[..arity]
        .iter()
        .enumerate()
        .map(|(index, text)| parse_operand(text, destination && index == 0))
        .collect::<Result<Vec<_>, _>>()?;
    let texture_target = if texture {
        Some(TextureTarget::parse(parts[arity])?)
    } else {
        None
    };
    Ok(Instr {
        number,
        opcode: opcode.into(),
        args,
        texture_target,
    })
}

fn validate_operands(program: &Program, instruction: &Instr) -> Result<(), String> {
    let (_, destination, texture) = opcode_signature(&instruction.opcode)?;
    for (index, operand) in instruction.args.iter().enumerate() {
        if destination && index == 0 {
            if !matches!(operand.file, RegisterFile::Temp | RegisterFile::Out) {
                return Err("destination register is not writable".into());
            }
        } else if texture && index == 2 {
            if operand.file != RegisterFile::Samp
                || operand.swizzle != [0, 1, 2, 3]
                || operand.absolute
                || operand.negate
            {
                return Err("texture instruction requires an unmodified SAMP operand".into());
            }
        } else if matches!(operand.file, RegisterFile::Samp | RegisterFile::SView) {
            return Err("sampler register in arithmetic operand".into());
        }
        if operand.file == RegisterFile::Imm {
            if usize::from(operand.index) >= program.immediates.len() {
                return Err("reference to undeclared immediate register".into());
            }
        } else if program.declaration(operand.file, operand.index).is_none() {
            return Err(format!(
                "reference to undeclared {:?}[{}]",
                operand.file, operand.index
            ));
        }
    }
    if texture {
        let sampler = &instruction.args[2];
        let view = program
            .declaration(RegisterFile::SView, sampler.index)
            .ok_or_else(|| "texture instruction has no sampler-view declaration".to_string())?;
        if view.texture_target != instruction.texture_target {
            return Err("texture target differs from sampler-view declaration".into());
        }
        if matches!(instruction.texture_target, Some(TextureTarget::Buffer))
            && !matches!(instruction.opcode.as_str(), "TXF" | "TXQ")
        {
            return Err("filtered sampling of buffer texture".into());
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Control {
    If { has_else: bool },
    Loop,
}

fn validate_control(
    instruction: &Instr,
    stage: Stage,
    stack: &mut Vec<Control>,
) -> Result<(), String> {
    match instruction.opcode.as_str() {
        "UIF" => stack.push(Control::If { has_else: false }),
        "BGNLOOP" => stack.push(Control::Loop),
        "ELSE" => match stack.last_mut() {
            Some(Control::If { has_else }) if !*has_else => *has_else = true,
            _ => return Err("ELSE without an unmatched UIF".into()),
        },
        "ENDIF" => {
            if !matches!(stack.pop(), Some(Control::If { .. })) {
                return Err("ENDIF without an unmatched UIF".into());
            }
        }
        "ENDLOOP" => {
            if !matches!(stack.pop(), Some(Control::Loop)) {
                return Err("ENDLOOP without an unmatched BGNLOOP".into());
            }
        }
        "BRK" | "CONT" if !stack.iter().any(|c| matches!(c, Control::Loop)) => {
            return Err("loop control outside BGNLOOP".into());
        }
        "KILL" if stage != Stage::Fragment => return Err("KILL in vertex program".into()),
        "END" if !stack.is_empty() => return Err("END inside unfinished control flow".into()),
        _ => {}
    }
    if stack.len() > MAX_CONTROL_DEPTH {
        return Err("control-flow nesting exceeds 256".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actual_virgl_arithmetic_control_and_texture_output() {
        use sgfx_codegen_virgl::programmable::compile_shader;
        use sgfx_core::ir::{ShaderModuleDesc, ShaderStage};

        let source = r#"
            struct Uniforms { scale: vec4<f32>, };
            @group(0) @binding(0) var<uniform> uniforms: Uniforms;
            @group(1) @binding(0) var image: texture_2d<f32>;
            @group(1) @binding(1) var filtering: sampler;
            @fragment fn main(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
                var result = textureSample(image, filtering, uv)
                    + textureSampleLevel(image, filtering, uv, 1.0);
                for (var i = 0u; i < 3u; i += 1u) {
                    if (i == 1u) { continue; }
                    result += uniforms.scale * vec4<f32>(uv, 0.5, 1.0);
                    if (result.x > 8.0) { break; }
                }
                if (uv.x < 0.25) {
                    result = min(result, vec4<f32>(1.0));
                } else {
                    result = max(result, vec4<f32>(0.0));
                }
                if (result.w < 0.0) { discard; }
                return select(result, result * 0.5, uv.y > 0.5);
            }
        "#;
        let module = ShaderModuleDesc::wgsl(source.into()).unwrap();
        let compiled = compile_shader(&module, ShaderStage::Fragment, "main").unwrap();
        let program = parse(&compiled.tgsi).unwrap();
        for opcode in [
            "TEX", "TXL", "ADD", "MUL", "MIN", "MAX", "UCMP", "UIF", "ELSE", "BGNLOOP", "BRK",
            "KILL",
        ] {
            assert!(
                program.instructions.iter().any(|i| i.opcode == opcode),
                "missing {opcode} from {}",
                compiled.tgsi
            );
        }
        assert!(
            program
                .declarations
                .iter()
                .any(|d| d.file == RegisterFile::Const)
        );
        assert!(
            program
                .declarations
                .iter()
                .any(|d| d.file == RegisterFile::SView)
        );
    }

    #[test]
    fn parses_actual_virgl_vertex_matrix_and_system_value_output() {
        use sgfx_codegen_virgl::programmable::compile_shader;
        use sgfx_core::ir::{ShaderModuleDesc, ShaderStage};

        let source = r#"
            struct Uniforms { matrix: mat4x4<f32>, };
            @group(0) @binding(0) var<uniform> uniforms: Uniforms;
            struct Output {
                @builtin(position) position: vec4<f32>,
                @location(0) color: vec4<f32>,
            };
            @vertex fn main(@location(0) position: vec3<f32>,
                @builtin(vertex_index) vertex: u32,
                @builtin(instance_index) instance: u32) -> Output {
                return Output(uniforms.matrix * vec4<f32>(position, 1.0),
                    vec4<f32>(f32(vertex), f32(instance), 0.0, 1.0));
            }
        "#;
        let module = ShaderModuleDesc::wgsl(source.into()).unwrap();
        let compiled = compile_shader(&module, ShaderStage::Vertex, "main").unwrap();
        let program = parse(&compiled.tgsi).unwrap();
        assert_eq!(program.stage, Stage::Vertex);
        assert!(
            program
                .declarations
                .iter()
                .any(|d| d.semantic == Some(Semantic::VertexId))
        );
        assert!(
            program
                .declarations
                .iter()
                .any(|d| d.semantic == Some(Semantic::InstanceId))
        );
        for opcode in ["MUL", "ADD", "SUB", "UADD", "U2F"] {
            assert!(program.instructions.iter().any(|i| i.opcode == opcode));
        }
    }

    #[test]
    fn parses_complete_arithmetic_control_and_texture_program() {
        // Uses the exact spelling, numbered instructions, scalar swizzles and
        // four-word immediate layout emitted by programmable.rs.
        let source = "FRAG\n\
PROPERTY FS_COORD_ORIGIN UPPER_LEFT\n\
PROPERTY FS_COORD_PIXEL_CENTER HALF_INTEGER\n\
DCL IN[0], GENERIC[0], PERSPECTIVE\n\
DCL IN[17], FACE, CONSTANT\n\
DCL OUT[0], COLOR[0]\n\
DCL CONST[0..1]\n\
DCL SAMP[0]\n\
DCL SVIEW[0], 2D, FLOAT\n\
DCL SAMP[1]\n\
DCL SVIEW[1], BUFFER, UINT\n\
DCL TEMP[0..4]\n\
IMM[0] FLT32 { 5.000000000e-1, 5.000000000e-1, 5.000000000e-1, 5.000000000e-1 }\n\
IMM[1] UINT32 { 4294967295, 0, 2, 3 }\n\
IMM[2] INT32 { -2147483648, -1, 0, 2147483647 }\n\
0: MOV TEMP[0].x, IN[0].xxxx\n\
1: MUL TEMP[0].y, IN[0].yyyy, IMM[0].xxxx\n\
2: UADD TEMP[1].x, CONST[0].xxxx, IMM[1].zzzz\n\
3: FSLT TEMP[1].y, IMM[0].xxxx, IN[17].xxxx\n\
4: UIF TEMP[1].yyyy\n\
5: TEX TEMP[2], TEMP[0], SAMP[0], 2D\n\
6: ELSE\n\
7: TXL TEMP[2], TEMP[0], SAMP[0], 2D\n\
8: ENDIF\n\
9: BGNLOOP\n\
10: UIF TEMP[1].xxxx\n\
11: BRK\n\
12: ENDIF\n\
13: TXF TEMP[3], TEMP[1], SAMP[1], BUFFER\n\
14: CONT\n\
15: ENDLOOP\n\
16: UCMP TEMP[4].x, TEMP[1].yyyy, TEMP[2].xxxx, TEMP[3].xxxx\n\
17: MOV OUT[0].xyzw, TEMP[4]\n\
18: END\n";
        let program = parse(source).unwrap();
        assert_eq!(program.stage, Stage::Fragment);
        assert_eq!(program.instructions.len(), 19);
        assert_eq!(program.immediates[0].values, [0.5f32.to_bits(); 4]);
        assert_eq!(program.immediates[1].values, [u32::MAX, 0, 2, 3]);
        assert_eq!(
            program.immediates[2].values,
            [0x8000_0000, u32::MAX, 0, 0x7fff_ffff]
        );
        assert_eq!(program.instructions[1].args[0].mask, 2);
        assert_eq!(program.instructions[1].args[1].swizzle, [1; 4]);
        assert_eq!(
            program.instructions[5].texture_target,
            Some(TextureTarget::D2)
        );
        assert_eq!(
            program.instructions[13].texture_target,
            Some(TextureTarget::Buffer)
        );
    }

    #[test]
    fn parses_vertex_system_values_and_legacy_color_property() {
        let vertex = parse("VERT\nDCL SV[0], VERTEXID\nDCL SV[1], INSTANCEID\nDCL OUT[0], POSITION\nDCL OUT[17], PSIZE\n0: MOV OUT[0], SV[0]\n1: MOV OUT[17].x, SV[1].xxxx\n2: END\n").unwrap();
        assert_eq!(vertex.declarations[1].semantic, Some(Semantic::InstanceId));
        assert_eq!(vertex.declarations[3].semantic, Some(Semantic::PointSize));
        let fragment = parse("FRAG\nPROPERTY FS_COLOR0_WRITES_ALL_CBUFS 1\nDCL IN[0], COLOR, PERSPECTIVE\nDCL OUT[0], COLOR\n0: MOV OUT[0], IN[0]\n1: END\n").unwrap();
        assert_eq!(
            fragment.properties,
            [Property::FsColor0WritesAllCbufs(true)]
        );
    }

    #[test]
    fn source_modifiers_and_swizzles_are_preserved() {
        let program = parse("VERT\nDCL IN[0]\nDCL OUT[0], POSITION\n0: ADD OUT[0].xz, -|IN[0].wzyx|, IN[0].z\n1: END\n").unwrap();
        let args = &program.instructions[0].args;
        assert_eq!(args[0].mask, 5);
        assert!(args[1].negate && args[1].absolute);
        assert_eq!(args[1].swizzle, [3, 2, 1, 0]);
        assert_eq!(args[2].swizzle, [2; 4]);
    }

    #[test]
    fn rejects_unsupported_malformed_and_unbounded_inputs() {
        for source in [
            "COMP\n0: END\n",
            "VERT\n0: UNKNOWN\n1: END\n",
            "VERT\n0: END\n1: END\n",
            "VERT\n1: END\n",
            "VERT\nDCL TEMP[0..1024]\n0: END\n",
            "VERT\nDCL TEMP[0]\nDCL TEMP[0]\n0: END\n",
            "VERT\n0: BRK\n1: END\n",
            "VERT\n0: ENDIF\n1: END\n",
            "VERT\n0: KILL\n1: END\n",
            "VERT\nDCL OUT[0], POSITION\n0: MOV OUT[0], IMM[0]\n1: END\n",
            "VERT\nDCL IN[0]\nDCL OUT[0], POSITION\n0: MOV OUT[0].xx, IN[0]\n1: END\n",
            "VERT\nDCL IN[0]\nDCL OUT[0], POSITION\n0: MOV OUT[0], IN[0].xy\n1: END\n",
            "VERT\nIMM[0] FLT32 { NaN, 0, 0, 0 }\n0: END\n",
            "VERT\nIMM[0] UINT32 { 4294967296, 0, 0, 0 }\n0: END\n",
            "VERT\nIMM[0] INT32 { -2147483649, 0, 0, 0 }\n0: END\n",
            "FRAG\nDCL SAMP[0]\nDCL SVIEW[0], CUBE, FLOAT\nDCL TEMP[0]\n0: TEX TEMP[0], TEMP[0], SAMP[0], 2D\n1: END\n",
        ] {
            assert!(parse(source).is_err(), "unexpectedly accepted {source:?}");
        }
        assert!(parse(&" ".repeat(MAX_TEXT_BYTES + 1)).is_err());
    }
}

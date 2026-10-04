use super::*;
use sgfx_nak::ir::ShaderIoInfo;

fn compile(source: &str, stage: ShaderStage) -> CompiledShader {
    let desc = ShaderModuleDesc::wgsl(source.into()).unwrap();
    let result = compile_shader(&desc, stage, "main").unwrap();
    assert!(!result.code.is_empty());
    assert_eq!(result.code.len() % 8, 0);
    assert!(result.metadata.num_gprs > 0);
    assert_eq!(result.metadata.scratch_bytes, 0);
    let virgl = sgfx_codegen_virgl::programmable::compile_shader(&desc, stage, "main").unwrap();
    assert_eq!(result.uniform_buffers, virgl.uniform_buffers);
    assert_eq!(result.storage_buffers, virgl.storage_buffers);
    assert_eq!(result.push_constants, virgl.push_constants);
    assert_eq!(result.textures, virgl.textures);
    assert_eq!(result.inputs, virgl.inputs);
    assert_eq!(result.outputs, virgl.outputs);
    validate_native_package(source, &result);
    result
}

fn validate_native_package(source: &str, shader: &CompiledShader) -> Vec<u8> {
    use maxwell_program_wire as wire;
    use sgfx_nak::{GraphicsStage, ir::ShaderStageInfo};
    let mut m = wire::Metadata::new(match shader.metadata.stage {
        GraphicsStage::Vertex => wire::Stage::Vertex,
        GraphicsStage::Fragment => wire::Stage::Fragment,
    });
    m.gprs = shader.metadata.num_gprs;
    if shader.metadata.info.uses_fp64 {
        m.flags |= wire::FLAG_FP64;
    }
    if let ShaderStageInfo::Fragment(fs) = &shader.metadata.info.stage {
        if fs.uses_kill {
            m.flags |= wire::FLAG_KILL;
        }
    }
    match &shader.metadata.info.io {
        ShaderIoInfo::Vtg(io) => {
            m.attr_in = io.attr_in;
            m.attr_out = io.attr_out;
            m.sysvals_in_ab = io.sysvals_in.ab;
            m.sysvals_in_c = io.sysvals_in.c;
            m.sysvals_in_d = io.sysvals_in_d;
            m.sysvals_out_ab = io.sysvals_out.ab;
            m.sysvals_out_c = io.sysvals_out.c;
            m.sysvals_out_d = io.sysvals_out_d;
            m.store_req_start = io.store_req_start;
            m.store_req_end = io.store_req_end;
        }
        ShaderIoInfo::Fragment(io) => {
            m.sysvals_in_ab = io.sysvals_in.ab;
            m.sysvals_in_c = io.sysvals_in.c;
            m.fs_inputs = io.attr_in.map(u8::from);
            m.fs_sysvals_d = io.sysvals_in_d.map(u8::from);
            m.fs_color_mask = io.writes_color;
            if io.writes_depth {
                m.flags |= wire::FLAG_DEPTH;
            }
            if io.writes_sample_mask {
                m.flags |= wire::FLAG_SAMPLE_MASK;
            }
        }
        _ => panic!("graphics stage IO"),
    }
    for u in &shader.uniform_buffers {
        m.cb_sizes[0] = m.cb_sizes[0].max((u.first_register + u.size.div_ceil(16)) * 16);
    }
    if let Some(p) = &shader.push_constants {
        m.cb_sizes[0] = m.cb_sizes[0].max((p.first_register + p.size.div_ceil(16)) * 16);
    }
    for s in &shader.storage_buffers {
        m.cb_sizes[0] = m.cb_sizes[0].max((s.first_register + 1) * 16);
        m.resource_mask |= 1 << s.slot;
    }
    for q in &shader.image_query_levels {
        m.cb_sizes[0] = m.cb_sizes[0].max((q.first_register + 1) * 16);
    }
    if let Some(first) = shader.first_instance_register {
        m.cb_sizes[0] = m.cb_sizes[0].max((first + 1) * 16);
    }
    if let Some(first) = shader.srgb_view_flags_register {
        m.cb_sizes[0] = m.cb_sizes[0].max(
            (first + (shader.storage_buffers.len() + shader.textures.len()).div_ceil(4) as u32)
                * 16,
        );
    }
    for t in &shader.textures {
        m.resource_mask |= 1 << t.slot;
    }
    let code: Vec<u8> = shader
        .code
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect();
    let program = wire::Program {
        metadata: m,
        code: &code,
    };
    let mut package = vec![0; wire::HEADER_SIZE + code.len()];
    program.encode_into(&mut package).unwrap();
    // Real compiler artifacts are useful for independently testing the kernel
    // verifier, including its strict opcode forms and structured stack rules.
    let hash = source.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.cache/sgfx-maxwell-runtime/compiler-fixtures");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{hash:016x}.mxp")), &package).unwrap();
    let parsed = wire::Program::parse(&package).unwrap();
    let verified = wire::validate(
        &parsed,
        &wire::Limits {
            cb_sizes: m.cb_sizes,
            resource_mask: m.resource_mask,
        },
    );
    assert!(
        verified.is_ok(),
        "native verifier rejected {hash:016x}: {:?}\n{}",
        verified.err(),
        shader.assembly
    );
    let verified = wire::validate(
        &parsed,
        &wire::Limits {
            cb_sizes: m.cb_sizes,
            resource_mask: m.resource_mask,
        },
    )
    .unwrap();
    let mut expected_header = shader.header;
    // NAK enables ISBE input/output space sharing after its instruction scan.
    // The kernel deliberately keeps this optional optimization disabled.
    if shader.metadata.stage == GraphicsStage::Vertex {
        expected_header[0] &= !(1 << 25);
    }
    assert_eq!(
        verified.header, expected_header,
        "kernel and NAK SPH encoding differ for {hash:016x}"
    );
    package
}

#[test]
fn boot_green_proof_fixtures() {
    let vs_source = "@vertex fn main(@location(0) p:vec2<f32>)->@builtin(position) vec4<f32>{return vec4<f32>(p,0.5,1.0);}";
    let fs_source =
        "@fragment fn main()->@location(0) vec4<f32>{return vec4<f32>(0.0,1.0,0.0,1.0);}";
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../shared/maxwell-program-wire/tests/fixtures");
    std::fs::create_dir_all(&directory).unwrap();
    for (name, source, stage) in [
        ("boot-vs", vs_source, ShaderStage::Vertex),
        ("boot-fs", fs_source, ShaderStage::Fragment),
    ] {
        let shader = compile(source, stage);
        let package = validate_native_package(source, &shader);
        std::fs::write(directory.join(format!("{name}.mxp")), package).unwrap();
        std::fs::write(
            directory.join(format!("{name}.wgsl")),
            format!("{source}\n"),
        )
        .unwrap();
    }
}

/// Execute the actual generated TGSI conversion entry point against a supplied
/// raw BGRA sample. This independent interpreter checks channel placement and
/// the mapping coordinates before NAK's separately-tested instruction lowering.
fn conversion_result(
    shader: &CompiledShader,
    sample: [f32; 4],
    position: [f32; 2],
    mapping: Option<[f32; 8]>,
) -> ([f32; 4], [f32; 4]) {
    use crate::tgsi::{RegisterFile as F, parse};
    use std::collections::HashMap;
    let program = parse(&shader.tgsi).unwrap();
    let mut regs = HashMap::<(F, u16), [u32; 4]>::new();
    for d in &program.declarations {
        for i in d.first..=d.last {
            regs.insert((d.file, i), [0; 4]);
        }
    }
    for i in &program.immediates {
        regs.insert((F::Imm, i.index), i.values);
    }
    regs.insert(
        (F::In, 16),
        [
            position[0].to_bits(),
            position[1].to_bits(),
            0,
            1f32.to_bits(),
        ],
    );
    if let Some(mapping) = mapping {
        let first = shader.uniform_buffers[0].first_register as u16;
        let destination: [f32; 4] = mapping[..4].try_into().unwrap();
        let source_rect: [f32; 4] = mapping[4..].try_into().unwrap();
        regs.insert((F::Const, first), destination.map(f32::to_bits));
        regs.insert((F::Const, first + 1), source_rect.map(f32::to_bits));
    }
    let source = |operand: &crate::tgsi::Operand, c: usize, regs: &HashMap<(F, u16), [u32; 4]>| {
        let mut value = regs[&(operand.file, operand.index)][operand.swizzle[c] as usize];
        if operand.absolute {
            value &= 0x7fffffff;
        }
        if operand.negate {
            value ^= 0x80000000;
        }
        value
    };
    let mut coords = [0f32; 4];
    let mut ip = 0;
    while ip < program.instructions.len() {
        let i = &program.instructions[ip];
        if i.opcode == "END" {
            break;
        }
        if i.opcode == "UIF" {
            if source(&i.args[0], 0, &regs) == 0 {
                let mut depth = 1;
                while depth != 0 {
                    ip += 1;
                    match program.instructions[ip].opcode.as_str() {
                        "UIF" => depth += 1,
                        "ENDIF" => depth -= 1,
                        _ => {}
                    }
                }
            }
            ip += 1;
            continue;
        }
        if i.opcode == "ENDIF" {
            ip += 1;
            continue;
        }
        let dst = &i.args[0];
        let mut result = regs[&(dst.file, dst.index)];
        for c in 0..4 {
            if dst.mask & (1 << c) == 0 {
                continue;
            }
            let a = source(&i.args[1], c, &regs);
            let f = f32::from_bits(a);
            let other = || f32::from_bits(source(&i.args[2], c, &regs));
            result[c] = match i.opcode.as_str() {
                "MOV" => a,
                "ADD" => (f + other()).to_bits(),
                "SUB" => (f - other()).to_bits(),
                "MUL" => (f * other()).to_bits(),
                "DIV" => (f / other()).to_bits(),
                "F2I" => (f as i32) as u32,
                "TXF" | "TXL" | "TEX" => {
                    for n in 0..4 {
                        let raw = source(&i.args[1], n, &regs);
                        coords[n] = if i.opcode == "TXF" {
                            (raw as i32) as f32
                        } else {
                            f32::from_bits(raw)
                        };
                    }
                    sample[c].to_bits()
                }
                op => panic!("unexpected conversion instruction {op}"),
            };
        }
        regs.insert((dst.file, dst.index), result);
        ip += 1;
    }
    (regs[&(F::Out, 0)].map(f32::from_bits), coords)
}

#[test]
fn r8_conversion_keeps_raw_channels_and_true_scratch_alpha() {
    let raw = [0.125, 0.25, 0.75, 0.625];
    for to_canonical in [false, true] {
        let (vs, fs) = compile_r8_conversion(to_canonical).unwrap();
        validate_native_package(
            if to_canonical {
                "r8-store-vs"
            } else {
                "r8-load-vs"
            },
            &vs,
        );
        validate_native_package(
            if to_canonical {
                "r8-store-fs"
            } else {
                "r8-load-fs"
            },
            &fs,
        );
        assert!(vs.vertex_inputs.is_empty());
        assert!(!fs.textures[0].uses_sampler);
        let (actual, coords) = conversion_result(&fs, raw, [3.5, 2.5], None);
        assert_eq!(
            actual,
            if to_canonical {
                [0.0, 0.0, 0.0, raw[0]]
            } else {
                [raw[3], 0.0, 0.0, 1.0]
            }
        );
        assert_eq!(&coords[..2], &[3.0, 2.0]);
    }
}

#[test]
fn r8_scaled_flip_conversion_executes_pixel_center_affine_mapping() {
    let (vs, fs) = compile_r8_blit_conversion(false).unwrap();
    validate_native_package("r8-scaled-vs", &vs);
    validate_native_package("r8-scaled-fs", &fs);
    assert!(fs.textures[0].uses_sampler);
    assert_eq!(fs.uniform_buffers[0].size, 32);
    // Destination pixel center halfway across, one quarter down; X is flipped.
    let (actual, coords) = conversion_result(
        &fs,
        [0.1, 0.2, 0.3, 0.6],
        [20.0, 25.0],
        Some([10.0, 20.0, 20.0, 20.0, 0.9, 0.2, -0.8, 0.4]),
    );
    assert_eq!(actual, [0.6, 0.0, 0.0, 1.0]);
    assert!((coords[0] - 0.5).abs() < 1e-6);
    assert!((coords[1] - 0.3).abs() < 1e-6);
}

const TRANSFORM: &str = r#"
struct Uniforms { transform:mat4x4<f32>, bias:vec4<f32> }
struct Push { transform:mat4x4<f32>, tint:array<vec4<f32>,4> }
@group(0) @binding(3) var<uniform> u:Uniforms;
var<push_constant> p:Push;
struct VertexOut { @builtin(position) position:vec4<f32>, @location(5) uv:vec2<f32> }
@vertex fn main(@location(2) position:vec3<f32>, @location(7) uv:vec2<f32>,
               @builtin(vertex_index) vertex:u32, @builtin(instance_index) instance:u32)->VertexOut {
  var out:VertexOut;
  out.position=u.transform*p.transform*vec4<f32>(position,1.0)+u.bias;
  out.position.x+=f32(vertex+instance)*0.001;
  out.uv=uv*p.tint[2].xy;
  return out;
}
"#;

#[test]
fn arbitrary_transform_preserves_layout_push_constants_and_system_values() {
    let shader = compile(TRANSFORM, ShaderStage::Vertex);
    assert_eq!(shader.uniform_buffers[0].size, 80);
    assert_eq!(shader.push_constants.as_ref().unwrap().size, 128);
    assert_eq!(shader.vertex_inputs.len(), 2);
    assert!(shader.first_instance_register.is_some());
    assert!(shader.assembly.contains("ald"));
    assert!(shader.assembly.contains("ast"));
    let ShaderIoInfo::Vtg(io) = &shader.metadata.info.io else {
        panic!("vertex IO")
    };
    assert_eq!(io.sysvals_in.c & (3 << 14), 3 << 14);
    assert_eq!(io.sysvals_out.ab & (15 << 28), 15 << 28);
    let different = compile(&TRANSFORM.replace("0.001", "0.037"), ShaderStage::Vertex);
    assert_ne!(shader.code, different.code);
}

#[test]
fn function_calls_loops_branching_discard_and_multiple_targets_compile() {
    let shader = compile(
        r#"
fn evolve(x:f32,n:u32)->f32 {
    var a=x;
    for(var i=0u;i<n;i+=1u){
        if(i==2u){continue;}
        if(a>7.0){break;}
        a=a*1.03125+0.0625;
    }
    return a;
}
struct Out { @location(0) color:vec4<f32>, @location(3) aux:vec4<f32> }
@fragment fn main(@location(0) x:vec2<f32>,@builtin(front_facing) front:bool,
                 @builtin(position) position:vec4<f32>)->Out {
    if(x.x<0.0){discard;}
    let y=evolve(x.x,5u);
    var out:Out;
    out.color=vec4<f32>(y,x.y,select(0.25,0.75,front),1.0);
    out.aux=vec4<f32>(position.xy,abs(y),1.0);
    return out;
}
"#,
        ShaderStage::Fragment,
    );
    assert!(shader.assembly.contains("ipa"));
    assert!(shader.assembly.contains("kill"));
    assert!(shader.assembly.contains("brk") || shader.assembly.contains("bra"));
    let ShaderIoInfo::Fragment(io) = &shader.metadata.info.io else {
        panic!("fragment IO")
    };
    assert_eq!(io.writes_color, 0xf00f);
}

#[test]
fn sampled_dimensions_depth_comparison_and_fetch_compile() {
    for (image, expression) in [
        ("texture_1d<f32>", "textureSampleLevel(t,s,uv.x,0.0)"),
        ("texture_2d<f32>", "textureSample(t,s,uv)"),
        ("texture_2d_array<f32>", "textureSampleLevel(t,s,uv,1,0.0)"),
        (
            "texture_cube<f32>",
            "textureSampleLevel(t,s,vec3<f32>(uv,1.0),0.0)",
        ),
        ("texture_2d<f32>", "textureLoad(t,vec2<i32>(uv*64.0),0)"),
    ] {
        let source = format!(
            "@group(1) @binding(4) var t:{image}; @group(2) @binding(6) var s:sampler; @fragment fn main(@location(0) uv:vec2<f32>)->@location(0) vec4<f32>{{return {expression};}}"
        );
        let shader = compile(&source, ShaderStage::Fragment);
        assert_eq!(shader.textures.len(), 1);
        assert!(shader.assembly.contains("tex") || shader.assembly.contains("tld"));
    }
    for (image, expression) in [
        ("texture_depth_2d", "textureSampleCompare(t,s,uv,0.3)"),
        (
            "texture_depth_2d_array",
            "textureSampleCompare(t,s,uv,1,0.3)",
        ),
        (
            "texture_depth_cube",
            "textureSampleCompare(t,s,vec3<f32>(uv,1.0),0.3)",
        ),
    ] {
        let source = format!(
            "@group(0) @binding(0) var t:{image}; @group(0) @binding(1) var s:sampler_comparison; @fragment fn main(@location(0) uv:vec2<f32>)->@location(0) vec4<f32>{{return vec4<f32>({expression});}}"
        );
        let shader = compile(&source, ShaderStage::Fragment);
        assert!(shader.textures[0].comparison);
        assert!(shader.assembly.contains(".dc"));
    }
}

#[test]
fn texture_dimensions_layers_and_mip_metadata_compile() {
    for (image, expression) in [
        (
            "texture_1d<f32>",
            "vec4<f32>(f32(textureDimensions(t,1)),f32(textureNumLevels(t)),0.0,1.0)",
        ),
        (
            "texture_2d<f32>",
            "vec4<f32>(vec2<f32>(textureDimensions(t,1)),f32(textureNumLevels(t)),1.0)",
        ),
        (
            "texture_2d_array<f32>",
            "vec4<f32>(vec2<f32>(textureDimensions(t,1)),f32(textureNumLayers(t)),1.0)",
        ),
        (
            "texture_cube<f32>",
            "vec4<f32>(vec2<f32>(textureDimensions(t,1)),0.0,1.0)",
        ),
    ] {
        let source = format!(
            "@group(0) @binding(2) var t:{image}; @fragment fn main()->@location(0) vec4<f32>{{return {expression};}}"
        );
        let shader = compile(&source, ShaderStage::Fragment);
        assert!(shader.assembly.contains("txq"));
        assert!(!shader.textures[0].uses_sampler);
    }
}

/// WGSL has no texture_1d_array spelling. Start from a validated 2D-array
/// program, reduce its explicit (x,0) coordinates to scalar x, and change only
/// the image dimension. This produces validated Naga IR and actual Dim=1D,
/// arrayed SPIR-V, rather than a handcrafted TGSI declaration.
fn logical_1d_array_spirv(query_layers: bool) -> ShaderModuleDesc {
    let query = if query_layers {
        "let layers=textureNumLayers(image); return sampled+loaded+vec4<f32>(f32(layers));"
    } else {
        "return sampled+loaded;"
    };
    let source = format!(
        r#"
@group(0) @binding(0) var image:texture_2d_array<f32>;
@group(0) @binding(1) var image_sampler:sampler;
@fragment fn main(@location(0) x:f32)->@location(0) vec4<f32>{{
    let sampled=textureSampleLevel(image,image_sampler,vec2<f32>(x,0.0),1,0.0);
    let loaded=textureLoad(image,vec2<i32>(i32(x),0),1,0);
    {query}
}}
"#
    );
    let mut module = naga::front::wgsl::parse_str(&source).unwrap();
    let validator = || {
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::PUSH_CONSTANT,
        )
    };
    validator().validate(&module).unwrap();
    let replacements: Vec<_> = module
        .types
        .iter()
        .filter_map(|(handle, ty)| {
            let mut ty = ty.clone();
            if let naga::TypeInner::Image {
                dim, arrayed: true, ..
            } = &mut ty.inner
            {
                assert_eq!(*dim, naga::ImageDimension::D2);
                *dim = naga::ImageDimension::D1;
                Some((handle, ty))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(replacements.len(), 1);
    for (handle, ty) in replacements {
        module.types.replace(handle, ty);
    }
    let function = &mut module.entry_points[0].function;
    let coordinates: Vec<_> = function
        .expressions
        .iter()
        .filter_map(|(_, expr)| match expr {
            naga::Expression::ImageSample { coordinate, .. }
            | naga::Expression::ImageLoad { coordinate, .. } => Some(*coordinate),
            _ => None,
        })
        .collect();
    assert_eq!(coordinates.len(), 2);
    for coordinate in coordinates {
        let naga::Expression::Compose { components, .. } = &function.expressions[coordinate] else {
            panic!("explicit (x,0) coordinate")
        };
        assert_eq!(components.len(), 2);
        // Scalar x + scalar zero retains arena ordering and expression scope.
        function.expressions[coordinate] = naga::Expression::Binary {
            op: naga::BinaryOperator::Add,
            left: components[0],
            right: components[1],
        };
    }
    let info = validator().validate(&module).unwrap();
    let mut options = naga::back::spv::Options::default();
    options
        .flags
        .remove(naga::back::spv::WriterFlags::ADJUST_COORDINATE_SPACE);
    let words = naga::back::spv::write_vec(
        &module,
        &info,
        &options,
        Some(&naga::back::spv::PipelineOptions {
            shader_stage: naga::ShaderStage::Fragment,
            entry_point: "main".into(),
        }),
    )
    .unwrap();
    // OpTypeImage's Dim=0 and Arrayed=1 mean an actual logical 1D array.
    let mut offset = 5;
    let mut found_1d_array = false;
    let mut found_size_query = false;
    while offset < words.len() {
        let len = (words[offset] >> 16) as usize;
        match words[offset] & 0xffff {
            25 => {
                assert_eq!(words[offset + 3], 0);
                assert_eq!(words[offset + 5], 1);
                found_1d_array = true;
            }
            103 => found_size_query = true, // OpImageQuerySizeLod
            _ => {}
        }
        offset += len;
    }
    assert!(found_1d_array);
    assert_eq!(found_size_query, query_layers);
    ShaderModuleDesc::spirv(words).unwrap()
}

#[test]
fn spirv_logical_1d_array_promotes_real_sample_and_load_to_2d_array() {
    use crate::tgsi::{TextureTarget, parse};
    use sgfx_core::ir::TextureViewDimension;
    let desc = logical_1d_array_spirv(false);
    let shader = compile_shader(&desc, ShaderStage::Fragment, "main").unwrap();
    let virgl =
        sgfx_codegen_virgl::programmable::compile_shader(&desc, ShaderStage::Fragment, "main")
            .unwrap();
    assert_eq!(shader.textures, virgl.textures);
    assert!(
        shader
            .textures
            .iter()
            .all(|t| t.dimension == TextureViewDimension::D1Array)
    );
    let tgsi = parse(&shader.tgsi).unwrap();
    for opcode in ["TXL", "TXF"] {
        let instruction = tgsi
            .instructions
            .iter()
            .find(|i| i.opcode == opcode)
            .unwrap();
        assert_eq!(instruction.texture_target, Some(TextureTarget::D2Array));
    }
    assert!(shader.assembly.contains("tex.a2d"), "{}", shader.assembly);
    assert!(shader.assembly.contains("tld.a2d"), "{}", shader.assembly);
    validate_native_package("actual-spirv-logical-1d-array-sample-load", &shader);
}

#[test]
fn spirv_logical_1d_array_query_has_the_same_naga_24_frontend_rejection_as_virgl() {
    let desc = logical_1d_array_spirv(true);
    // Naga 24's SPIR-V reader has a TODO for array OpImageQuerySize: it creates
    // a scalar D1 Size and then tries to extract the layers component from it.
    // The genuine input module validates before writing. Reject the reader's
    // invalid IR before native compilation, consistently with shared VirGL.
    let virgl =
        sgfx_codegen_virgl::programmable::compile_shader(&desc, ShaderStage::Fragment, "main")
            .unwrap_err();
    let native = compile_shader(&desc, ShaderStage::Fragment, "main").unwrap_err();
    assert_eq!(native.0, virgl.0);
    assert!(native.0.contains("shader validation:"), "{native}");
}

#[test]
fn readonly_storage_push_constants_and_exact_integer_math_compile() {
    let shader = compile(
        r#"
struct Storage { values:array<u32> }
struct Push { index:u32, divisor:u32 }
@group(3) @binding(7) var<storage,read> data:Storage;
var<push_constant> p:Push;
@fragment fn main()->@location(0) vec4<f32>{
    let x=data.values[p.index];
    let d=max(p.divisor,1u);
    return vec4<f32>(f32(x/d),f32(x%d),f32(x),1.0);
}
"#,
        ShaderStage::Fragment,
    );
    assert_eq!(shader.storage_buffers.len(), 1);
    assert_eq!(shader.push_constants.as_ref().unwrap().size, 8);
    assert!(shader.assembly.contains("tld"));
}

#[test]
fn spirv_and_wgsl_use_the_same_validated_semantics() {
    let source = r#"@vertex fn main(@location(0) p:vec2<f32>)->@builtin(position) vec4<f32>{return vec4<f32>(p*1.25+vec2<f32>(0.2,-0.3),0.5,1.0);}"#;
    let wgsl = compile(source, ShaderStage::Vertex);
    let module = naga::front::wgsl::parse_str(source).unwrap();
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::PUSH_CONSTANT,
    )
    .validate(&module)
    .unwrap();
    let mut options = naga::back::spv::Options::default();
    options
        .flags
        .remove(naga::back::spv::WriterFlags::ADJUST_COORDINATE_SPACE);
    let words = naga::back::spv::write_vec(
        &module,
        &info,
        &options,
        Some(&naga::back::spv::PipelineOptions {
            shader_stage: naga::ShaderStage::Vertex,
            entry_point: "main".into(),
        }),
    )
    .unwrap();
    let spv = compile_shader(
        &ShaderModuleDesc::spirv(words).unwrap(),
        ShaderStage::Vertex,
        "main",
    )
    .unwrap();
    // SPIR-V omits the unused interpolation decoration of vertex attributes.
    assert_eq!(wgsl.vertex_inputs, spv.vertex_inputs);
    assert_eq!(wgsl.input_locations, spv.input_locations);
    assert_eq!(wgsl.outputs, spv.outputs);
    assert_eq!(wgsl.code, spv.code);
}

#[test]
fn malformed_and_unsupported_semantics_fail_before_code_generation() {
    let malformed = ShaderModuleDesc::wgsl("this is not WGSL".into()).unwrap();
    assert!(compile_shader(&malformed, ShaderStage::Fragment, "main").is_err());
    let good = ShaderModuleDesc::wgsl(
        "@fragment fn main()->@location(0) vec4<f32>{return vec4<f32>(1.0);}".into(),
    )
    .unwrap();
    assert!(compile_shader(&good, ShaderStage::Fragment, "absent").is_err());
    assert!(compile_shader(&good, ShaderStage::Vertex, "main").is_err());
    assert!(compile_shader(&good, ShaderStage::Compute, "main").is_err());
    let writable=ShaderModuleDesc::wgsl("@group(0) @binding(0) var<storage,read_write> x:array<u32>; @fragment fn main()->@location(0) vec4<f32>{x[0]=1u;return vec4<f32>(1.0);}".into()).unwrap();
    assert!(compile_shader(&writable, ShaderStage::Fragment, "main").is_err());
}

#[test]
fn outputless_discard_shader_is_a_valid_fragment_program() {
    let shader = compile("@fragment fn main(){discard;}", ShaderStage::Fragment);
    let ShaderIoInfo::Fragment(io) = &shader.metadata.info.io else {
        panic!("fragment IO")
    };
    assert_eq!(io.writes_color, 0);
    assert!(shader.assembly.contains("kill"));
}

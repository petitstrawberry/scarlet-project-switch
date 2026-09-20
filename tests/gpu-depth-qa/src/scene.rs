use sgfx_codegen_maxwell::*;
use sgfx_core::ir::*;

pub const WIDTH: u32 = 67;
pub const HEIGHT: u32 = 131;
pub const STRIDE: u32 = 40;
pub const VERTEX_BYTES: u64 = 12 * STRIDE as u64;
pub const CASES: &[(CompareFunction, bool, bool, [u8; 4])] = &[
    (CompareFunction::Never, true, false, [0, 0, 0, 255]),
    (CompareFunction::Less, true, false, [0, 0, 255, 255]),
    (CompareFunction::Equal, true, false, [0, 0, 255, 255]),
    (CompareFunction::LessEqual, true, false, [0, 0, 255, 255]),
    (CompareFunction::Greater, true, false, [255, 0, 0, 255]),
    (CompareFunction::NotEqual, true, false, [255, 0, 0, 255]),
    (CompareFunction::GreaterEqual, true, false, [255, 0, 0, 255]),
    (CompareFunction::Always, true, false, [255, 0, 0, 255]),
    (CompareFunction::Less, false, false, [255, 0, 0, 255]),
    (CompareFunction::Less, true, true, [255, 0, 0, 255]),
];

pub fn image(id: u32, depth: bool, modifier: ImageModifier, pitch: u32, size: u64) -> ResourceMeta {
    let format = if depth {
        TextureFormat::Depth32Float
    } else {
        TextureFormat::Bgra8Unorm
    };
    ResourceMeta {
        id: ObjectId::new(id),
        size,
        kind: ResourceKind::Image(ImageMeta {
            format,
            storage_format: format,
            extent: Extent2D::new(WIDTH, HEIGHT).unwrap(),
            usage: TextureUsage::RENDER_ATTACHMENT,
            modifier,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: pitch,
                size,
            }],
        }),
    }
}

pub fn resources() -> Vec<ResourceMeta> {
    let pitch = (WIDTH * 4 + 63) & !63;
    let size = u64::from(pitch * ((HEIGHT + 127) & !127));
    vec![
        image(
            0,
            false,
            ImageModifier::NvidiaBlockLinear16Bx2H4,
            pitch,
            size,
        ),
        image(
            1,
            true,
            ImageModifier::NvidiaZf32BlockLinear16Bx2H4,
            pitch,
            size,
        ),
        ResourceMeta {
            id: ObjectId::new(2),
            size: VERTEX_BYTES,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX,
            },
        },
    ]
}

pub fn compile_scene(
    resources: &[ResourceMeta],
    compare: CompareFunction,
    write: bool,
    disable_last: bool,
) -> core::result::Result<RelocatableCommands, CompileError> {
    let base = RenderPipelineDesc::new(
        TextureFormat::Bgra8Unorm,
        PrimitiveTopology::TriangleList,
        VertexBufferLayout::new(
            STRIDE,
            vec![VertexAttribute::new(0, VertexFormat::Float32x4, 0)],
        )
        .unwrap(),
        FragmentProgram::Solid,
        BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
        RasterState::new(CullMode::None, FrontFace::CounterClockwise),
    )
    .unwrap();
    let pipelines = [
        PipelineMeta {
            id: PipelineId::new(0),
            descriptor: base
                .clone()
                .with_depth_state(DepthState::new(TextureFormat::Depth32Float, compare, write))
                .unwrap(),
        },
        PipelineMeta {
            id: PipelineId::new(1),
            descriptor: base,
        },
    ];
    let clear = match compare {
        CompareFunction::Greater => 0.0,
        CompareFunction::Equal | CompareFunction::LessEqual => 0.25,
        CompareFunction::GreaterEqual => 0.75,
        _ => 1.0,
    };
    let operations = [
        Operation::BeginRenderPass(RenderPass {
            target: ObjectId::new(0),
            area: PixelRect::new(0, 0, WIDTH, HEIGHT).unwrap(),
            load: LoadOp::Clear(Color::rgba(0.0, 0.0, 0.0, 1.0).unwrap()),
            store: StoreOp::Store,
            depth: Some(sgfx_codegen_maxwell::DepthAttachment {
                target: ObjectId::new(1),
                load: DepthLoadOp::Clear(clear),
                store: StoreOp::Store,
            }),
        }),
        Operation::SetPipeline(PipelineId::new(0)),
        Operation::SetVertexBuffer {
            buffer: ObjectId::new(2),
            offset: 0,
        },
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 0.0, 0.0, 1.0).unwrap(),
        )),
        Operation::Draw {
            vertex_count: 6,
            first_vertex: 0,
        },
        Operation::SetPipeline(PipelineId::new(if disable_last { 1 } else { 0 })),
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(0.0, 0.0, 1.0, 1.0).unwrap(),
        )),
        Operation::Draw {
            vertex_count: 6,
            first_vertex: 6,
        },
        Operation::EndRenderPass,
    ];
    compile(CompileInput {
        capabilities: Capabilities::gm20b(65536),
        resources,
        pipelines: &pipelines,
        operations: &operations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_scenes_preserve_access_and_disable_state() {
        for &(compare, write, disable, _) in CASES {
            let commands = compile_scene(&resources(), compare, write, disable).unwrap();
            assert_eq!(commands.words.len(), 4 * 64);
            assert_eq!(commands.words[0], 4);
            assert_eq!(commands.words[64], 1);
            assert_eq!(commands.words[2 * 64 + 60], compare as u32 + 1);
            assert_eq!(commands.words[2 * 64 + 61], u32::from(write));
            assert_eq!(
                commands
                    .fixups
                    .iter()
                    .filter(|f| f.word_offset % 64 == 54)
                    .count(),
                if disable { 1 } else { 2 }
            );
            for fixup in commands.fixups.iter().filter(|f| f.word_offset % 64 == 54) {
                assert_eq!(fixup.object, ObjectRef::External(ObjectId::new(1)));
                assert_eq!(fixup.access.contains(Access::WRITE), write);
            }
            if disable {
                assert!(commands.words[3 * 64 + 54..].iter().all(|&v| v == 0));
            }
        }
    }

    #[test]
    fn rejects_linear_color_with_depth() {
        let mut inputs = resources();
        if let ResourceKind::Image(image) = &mut inputs[0].kind {
            image.modifier = ImageModifier::Linear;
        }
        assert_eq!(
            compile_scene(&inputs, CompareFunction::Less, true, false),
            Err(CompileError::InvalidResource)
        );
    }

    #[test]
    fn rejects_color_kind_for_zf32() {
        let mut inputs = resources();
        if let ResourceKind::Image(image) = &mut inputs[1].kind {
            image.modifier = ImageModifier::NvidiaBlockLinear16Bx2H4;
        }
        assert_eq!(
            compile_scene(&inputs, CompareFunction::Less, true, false),
            Err(CompileError::InvalidResource)
        );
    }

    #[test]
    fn rejects_depth_extent_and_padded_allocation_mismatch() {
        let mut inputs = resources();
        if let ResourceKind::Image(image) = &mut inputs[1].kind {
            image.extent = Extent2D::new(WIDTH - 1, HEIGHT).unwrap();
        }
        assert!(compile_scene(&inputs, CompareFunction::Less, true, false).is_err());
        let mut inputs = resources();
        if let ResourceKind::Image(image) = &mut inputs[1].kind {
            image.planes[0].size -= 1;
        }
        assert_eq!(
            compile_scene(&inputs, CompareFunction::Less, true, false),
            Err(CompileError::OutOfBounds)
        );
    }
}

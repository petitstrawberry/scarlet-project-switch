//! Native image sampling must use the normal draw path and preserve access to
//! both planes; subsequent RGB overlays must remain ordinary RGB draws.
use sgfx_codegen_maxwell::*;
use sgfx_core::ir::*;

fn draw_native_then_overlay(
    modifier: ImageModifier,
    uv_size: u64,
) -> core::result::Result<RelocatableCommands, CompileError> {
    let target = ObjectId::new(0);
    let video = ObjectId::new(1);
    let vertices = ObjectId::new(2);
    let overlay = ObjectId::new(3);
    let extent = Extent2D::new(32, 32).unwrap();
    let rgb = |usage| {
        ResourceKind::Image(ImageMeta {
            format: TextureFormat::Bgra8Unorm,
            storage_format: TextureFormat::Bgra8Unorm,
            extent,
            usage,
            modifier: ImageModifier::Linear,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: 128,
                size: 4096,
            }],
        })
    };
    let resources = [
        ResourceMeta {
            id: target,
            size: 4096,
            kind: rgb(TextureUsage::RENDER_ATTACHMENT),
        },
        ResourceMeta {
            id: video,
            size: 8192,
            kind: ResourceKind::Image(ImageMeta {
                format: TextureFormat::Nv12,
                storage_format: TextureFormat::Nv12,
                extent,
                usage: TextureUsage::SAMPLED,
                modifier,
                planes: vec![
                    PlaneLayout {
                        offset: 0,
                        stride: 64,
                        size: 2048,
                    },
                    PlaneLayout {
                        offset: 4096,
                        stride: 64,
                        size: uv_size,
                    },
                ],
            }),
        },
        ResourceMeta {
            id: vertices,
            size: 48,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX,
            },
        },
        ResourceMeta {
            id: overlay,
            size: 4096,
            kind: rgb(TextureUsage::SAMPLED),
        },
    ];
    let pipeline = PipelineMeta {
        id: PipelineId::new(0),
        descriptor: RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            PrimitiveTopology::TriangleList,
            VertexBufferLayout::new(
                16,
                vec![
                    VertexAttribute::new(0, VertexFormat::Float32x2, 0),
                    VertexAttribute::new(1, VertexFormat::Float32x2, 8),
                ],
            )
            .unwrap(),
            FragmentProgram::Texture(TextureSampleMode::Rgba),
            BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap(),
    };
    let operations = [
        Operation::BeginRenderPass(RenderPass {
            target,
            area: PixelRect::new(0, 0, 32, 32).unwrap(),
            load: LoadOp::Load,
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(pipeline.id),
        Operation::SetVertexBuffer {
            buffer: vertices,
            offset: 0,
        },
        Operation::SetSampler(SamplerDesc::new(
            FilterMode::Linear,
            FilterMode::Linear,
            AddressMode::ClampToEdge,
            AddressMode::ClampToEdge,
        )),
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1., 1., 1., 1.).unwrap(),
        )),
        Operation::SetTexture(video),
        Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        },
        Operation::SetTexture(overlay),
        Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        },
        Operation::EndRenderPass,
    ];
    compile(CompileInput {
        capabilities: Capabilities::gm20b(1024),
        resources: &resources,
        pipelines: &[pipeline],
        operations: &operations,
    })
}

#[test]
fn native_nv12_and_rgb_overlay_compile_with_both_plane_access() {
    for modifier in [
        ImageModifier::Linear,
        ImageModifier::NvidiaBlockLinear16Bx2H1,
    ] {
        let commands = draw_native_then_overlay(modifier, 1024).unwrap();
        let draws: Vec<_> = commands
            .words
            .chunks_exact(64)
            .filter(|w| w[0] == 2)
            .collect();
        assert_eq!(draws.len(), 2);
        assert_eq!(
            draws[0][53],
            if modifier == ImageModifier::Linear {
                0
            } else {
                0x10
            }
        );
        assert_eq!(draws[1][53], 0);
        let access = commands
            .accesses
            .iter()
            .find(|a| a.object == ObjectRef::External(ObjectId::new(1)))
            .unwrap();
        assert_eq!(
            (access.offset, access.size, access.access),
            (0, 8192, Access::READ)
        );
        assert!(
            commands
                .fixups
                .iter()
                .any(|f| f.object == access.object && f.required_size == 8192)
        );
    }
}

#[test]
fn rejects_truncated_uv_plane_before_emitting_a_draw() {
    for modifier in [
        ImageModifier::Linear,
        ImageModifier::NvidiaBlockLinear16Bx2H1,
    ] {
        assert_eq!(
            draw_native_then_overlay(modifier, 8),
            Err(CompileError::InvalidResource)
        );
    }
}

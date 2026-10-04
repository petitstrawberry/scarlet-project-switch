//! Fixed viewports are explicit native state, independent of transform/scissor.
use sgfx_codegen_maxwell::*;
use sgfx_core::ir::*;

fn fixture() -> ([ResourceMeta; 2], PipelineMeta, Vec<Operation<'static>>) {
    let resources = [
        ResourceMeta {
            id: ObjectId::new(0),
            size: 4096,
            kind: ResourceKind::Image(ImageMeta {
                subresources: None,
                format: TextureFormat::Bgra8Unorm,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(32, 32).unwrap(),
                usage: TextureUsage::RENDER_ATTACHMENT,
                modifier: ImageModifier::Linear,
                planes: vec![PlaneLayout {
                    offset: 0,
                    stride: 128,
                    size: 4096,
                }],
            }),
        },
        ResourceMeta {
            id: ObjectId::new(1),
            size: 48,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX,
            },
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
            FragmentProgram::Solid,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap(),
    };
    let operations = vec![
        Operation::BeginRenderPass(RenderPass {
            target: resources[0].id,
            area: PixelRect::new(0, 0, 32, 32).unwrap(),
            load: LoadOp::Load,
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(pipeline.id),
        Operation::SetVertexBuffer {
            buffer: resources[1].id,
            offset: 0,
        },
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1., 1., 1., 1.).unwrap(),
        )),
    ];
    (resources, pipeline, operations)
}

fn compile_ops(
    resources: &[ResourceMeta],
    pipeline: &PipelineMeta,
    ops: &[Operation<'_>],
    budget: u32,
) -> core::result::Result<RelocatableCommands, CompileError> {
    compile(CompileInput {
        capabilities: Capabilities::gm20b(budget),
        resources,
        pipelines: core::slice::from_ref(pipeline),
        operations: ops,
    })
}

#[test]
fn native_viewport_is_emitted_for_each_draw_without_composing_the_transform() {
    let (resources, pipeline, mut ops) = fixture();
    let viewport = Viewport::new(2.25, 27.5, 17.5, -20.25, 0.8, 0.2).unwrap();
    ops.push(Operation::SetViewport(viewport));
    ops.extend([
        Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        },
        Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        },
    ]);
    ops.push(Operation::EndRenderPass);
    let compiled = compile_ops(&resources, &pipeline, &ops, 256).unwrap();
    let records: Vec<_> = compiled.words.chunks_exact(64).collect();
    assert_eq!(
        records.iter().map(|w| w[0]).collect::<Vec<_>>(),
        [7, 2, 7, 2]
    );
    for words in [records[0], records[2]] {
        assert_eq!(&words[32..38], &viewport.components().map(f32::to_bits));
        assert!(
            words[1..32]
                .iter()
                .chain(words[38..].iter())
                .all(|&v| v == 0)
        );
    }
    for words in [records[1], records[3]] {
        assert_eq!(
            &words[32..48],
            &Transform::identity().columns().map(f32::to_bits)
        );
        assert_eq!(&words[17..21], &[0, 0, 32, 32]);
    }
    assert!(
        compiled
            .fixups
            .iter()
            .all(|f| matches!(f.word_offset, 66 | 68 | 194 | 196))
    );
    // Serialize actual records and authority; viewport records have no relocations.
    use maxwell_submit_wire as wire;
    let wire_resources: Vec<_> = compiled
        .accesses
        .iter()
        .map(|a| wire::Resource {
            attachment_token: 1,
            range_offset: a.offset,
            range_size: a.size,
            access: u32::from(a.access.bits()),
        })
        .collect();
    let relocations: Vec<_> = compiled
        .fixups
        .iter()
        .map(|f| {
            let (id, a) = compiled
                .accesses
                .iter()
                .enumerate()
                .find(|(_, a)| a.object == f.object)
                .unwrap();
            wire::Relocation {
                commands_word_offset: f.word_offset,
                source: wire::RelocationSource::Attachment(id as u32),
                resource_offset: f.object_offset - a.offset,
                required_size: f.required_size,
                access: u32::from(f.access.bits()),
                encoding: wire::AddressEncoding::GpuVa64,
            }
        })
        .collect();
    let submit = wire::Submit {
        commands: &compiled.words,
        resources: &wire_resources,
        relocations: &relocations,
    };
    let mut bytes = vec![0; wire::encoded_len(submit).unwrap()];
    wire::encode(submit, &mut bytes).unwrap();
    let decoded = wire::decode(&bytes).unwrap();
    for (i, &word) in compiled.words.iter().enumerate() {
        assert_eq!(decoded.commands_word(i), Some(word));
    }
    assert_eq!(
        compile_ops(&resources, &pipeline, &ops, 192),
        Err(CompileError::CommandBudgetExceeded)
    );
}

#[test]
fn viewport_is_reset_at_pass_boundaries_and_zero_height_is_preserved() {
    let (resources, pipeline, mut ops) = fixture();
    let begin = ops.clone();
    ops.push(Operation::SetViewport(
        Viewport::new(0., 16., 16., 0., 0., 1.).unwrap(),
    ));
    ops.push(Operation::Draw {
        vertex_count: 3,
        first_vertex: 0,
    });
    ops.push(Operation::EndRenderPass);
    ops.extend(begin);
    ops.push(Operation::Draw {
        vertex_count: 3,
        first_vertex: 0,
    });
    ops.push(Operation::EndRenderPass);
    let compiled = compile_ops(&resources, &pipeline, &ops, 256).unwrap();
    assert_eq!(
        compiled
            .words
            .chunks_exact(64)
            .map(|w| w[0])
            .collect::<Vec<_>>(),
        [7, 2, 2]
    );
    assert_eq!(compiled.words[35], 0);
}

#[test]
fn viewport_bounds_and_render_pass_state_are_checked_before_emission() {
    let (resources, pipeline, begin) = fixture();
    for viewport in [
        Viewport::new(24., 0., 9., 16., 0., 1.).unwrap(),
        Viewport::new(0., 4., 16., -5., 0., 1.).unwrap(),
        Viewport::new(0., 30., 16., 3., 0., 1.).unwrap(),
    ] {
        let mut ops = begin.clone();
        ops.push(Operation::SetViewport(viewport));
        assert_eq!(
            compile_ops(&resources, &pipeline, &ops, 256),
            Err(CompileError::OutOfBounds)
        );
    }
    assert_eq!(
        compile_ops(
            &resources,
            &pipeline,
            &[Operation::SetViewport(
                Viewport::new(0., 0., 16., 16., 0., 1.).unwrap()
            )],
            256
        ),
        Err(CompileError::InvalidState)
    );
}

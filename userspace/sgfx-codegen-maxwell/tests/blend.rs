//! Fixed blend vocabulary, component masks and narrow clears use typed state.
use sgfx_codegen_maxwell::*;
use sgfx_core::ir::*;

fn fixture(
    blend: BlendState,
    raster: RasterState,
) -> ([ResourceMeta; 2], PipelineMeta, Vec<Operation<'static>>) {
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
            blend,
            raster,
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

fn finish_draw(ops: &mut Vec<Operation<'static>>) {
    ops.push(Operation::Draw {
        vertex_count: 3,
        first_vertex: 0,
    });
    ops.push(Operation::EndRenderPass);
}

#[test]
fn every_fixed_factor_and_equation_survives_actual_records_and_wire() {
    let factors = [
        BlendFactor::Zero,
        BlendFactor::One,
        BlendFactor::SourceAlpha,
        BlendFactor::OneMinusSourceAlpha,
        BlendFactor::DestinationAlpha,
        BlendFactor::OneMinusDestinationAlpha,
    ];
    let equations = [BlendOp::Add, BlendOp::Subtract, BlendOp::ReverseSubtract];
    for (src, &source) in factors.iter().enumerate() {
        for (dst, &destination) in factors.iter().enumerate() {
            for (op, &equation) in equations.iter().enumerate() {
                // Independent alpha parameters exercise all six normalized fields.
                let alpha_src = (src + 1) % 6;
                let alpha_dst = (dst + 2) % 6;
                let alpha_op = (op + 1) % 3;
                let blend = BlendState::new(
                    BlendComponent::new(source, destination, equation),
                    BlendComponent::new(
                        factors[alpha_src],
                        factors[alpha_dst],
                        equations[alpha_op],
                    ),
                );
                let (resources, pipeline, mut ops) = fixture(
                    blend,
                    RasterState::new(CullMode::None, FrontFace::CounterClockwise),
                );
                finish_draw(&mut ops);
                let compiled = compile_ops(&resources, &pipeline, &ops, 128).unwrap();
                assert_eq!(compiled.words.len(), 128);
                assert_eq!(compiled.words[0], 8);
                assert_eq!(compiled.words[64], 2);
                assert_eq!(
                    &compiled.words[21..29],
                    &[
                        15,
                        1,
                        src as u32,
                        dst as u32,
                        op as u32,
                        alpha_src as u32,
                        alpha_dst as u32,
                        alpha_op as u32
                    ]
                );
                assert!(
                    compiled.words[1..21]
                        .iter()
                        .chain(compiled.words[29..64].iter())
                        .all(|&w| w == 0)
                );
                assert!(
                    compiled
                        .fixups
                        .iter()
                        .all(|f| matches!(f.word_offset, 66 | 68))
                );
                assert!(compiled.generated_objects.is_empty());
                use maxwell_submit_wire as wire;
                let resources: Vec<_> = compiled
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
                    resources: &resources,
                    relocations: &relocations,
                };
                let mut bytes = vec![0; wire::encoded_len(submit).unwrap()];
                wire::encode(submit, &mut bytes).unwrap();
                let decoded = wire::decode(&bytes).unwrap();
                for (index, &word) in compiled.words.iter().enumerate() {
                    assert_eq!(decoded.commands_word(index), Some(word));
                }
            }
        }
    }
}

#[test]
fn masks_are_per_draw_and_reset_at_new_passes_while_all_cull_modes_are_retained() {
    for blend in [BlendState::REPLACE, BlendState::SOURCE_OVER_STRAIGHT_ALPHA] {
        for cull in [CullMode::None, CullMode::Front, CullMode::Back] {
            for face in [FrontFace::CounterClockwise, FrontFace::Clockwise] {
                for mask in 0..16 {
                    let (resources, pipeline, mut ops) =
                        fixture(blend, RasterState::new(cull, face));
                    let begin = ops.clone();
                    ops.push(Operation::SetColorWriteMask(mask));
                    ops.push(Operation::SetViewport(
                        Viewport::new(0., 0., 16., 16., 0., 1.).unwrap(),
                    ));
                    finish_draw(&mut ops);
                    ops.extend(begin);
                    finish_draw(&mut ops);
                    let compiled = compile_ops(&resources, &pipeline, &ops, 256).unwrap();
                    let records: Vec<_> = compiled.words.chunks_exact(64).collect();
                    let expected: &[u32] = if mask == 15 {
                        &[7, 2, 2]
                    } else {
                        &[8, 7, 2, 2]
                    };
                    assert_eq!(records.iter().map(|w| w[0]).collect::<Vec<_>>(), expected);
                    if mask != 15 {
                        assert_eq!(records[0][21], mask);
                        assert_eq!(records[0][22], u32::from(blend != BlendState::REPLACE));
                    }
                    for record in records.iter().filter(|w| w[0] == 2) {
                        let c = match cull {
                            CullMode::None => 0,
                            CullMode::Front => 1,
                            CullMode::Back => 2,
                        };
                        let f = u32::from(face == FrontFace::Clockwise) << 2;
                        assert_eq!((record[22] >> 2) & 7, c | f);
                    }
                }
            }
        }
    }
    let (resources, pipeline, mut ops) = fixture(
        BlendState::REPLACE,
        RasterState::new(CullMode::None, FrontFace::CounterClockwise),
    );
    ops.push(Operation::SetColorWriteMask(16));
    assert_eq!(
        compile_ops(&resources, &pipeline, &ops, 256),
        Err(CompileError::InvalidResource)
    );
    assert_eq!(
        compile_ops(
            &resources,
            &pipeline,
            &[Operation::SetColorWriteMask(1)],
            256
        ),
        Err(CompileError::InvalidState)
    );
}

#[test]
fn narrow_target_clears_preserve_logical_components_in_physical_bgra_storage() {
    for (format, physical) in [
        (TextureFormat::R8Unorm, [0.0_f32, 0., 0., 0.2]),
        (TextureFormat::Rg8Unorm, [0.2_f32, 0.4, 0., 1.]),
    ] {
        let (mut resources, pipeline, _) = fixture(
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        );
        let ResourceKind::Image(image) = &mut resources[0].kind else {
            unreachable!()
        };
        image.format = format;
        let ops = [
            Operation::BeginRenderPass(RenderPass {
                target: resources[0].id,
                area: PixelRect::new(0, 0, 32, 32).unwrap(),
                load: LoadOp::Clear(Color::rgba(0.2, 0.4, 0.6, 0.8).unwrap()),
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::EndRenderPass,
        ];
        let compiled = compile_ops(&resources, &pipeline, &ops, 64).unwrap();
        assert_eq!(compiled.words[0], 1);
        assert_eq!(&compiled.words[32..36], &physical.map(f32::to_bits));
    }
}

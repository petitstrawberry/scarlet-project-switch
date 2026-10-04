//! Portable fixed state must survive compilation and wire serialization without
//! becoming unrestricted GPU descriptor or address fields.
use sgfx_codegen_maxwell::*;
use sgfx_core::ir::*;

fn draw(
    sampler: SamplerDesc,
    indexed: Option<(IndexFormat, i32)>,
    vertex_size: u64,
) -> core::result::Result<RelocatableCommands, CompileError> {
    draw_with_image(sampler, indexed, vertex_size, None)
}

fn draw_with_image(
    sampler: SamplerDesc,
    indexed: Option<(IndexFormat, i32)>,
    vertex_size: u64,
    sampled: Option<ImageMeta>,
) -> core::result::Result<RelocatableCommands, CompileError> {
    draw_fragment(
        sampler,
        indexed,
        vertex_size,
        sampled,
        FragmentProgram::Texture(TextureSampleMode::Rgba),
    )
}

fn draw_fragment(
    sampler: SamplerDesc,
    indexed: Option<(IndexFormat, i32)>,
    vertex_size: u64,
    sampled: Option<ImageMeta>,
    fragment: FragmentProgram,
) -> core::result::Result<RelocatableCommands, CompileError> {
    draw_fragment_target(
        sampler,
        indexed,
        vertex_size,
        sampled,
        fragment,
        TextureFormat::Bgra8Unorm,
    )
}

fn draw_fragment_target(
    sampler: SamplerDesc,
    indexed: Option<(IndexFormat, i32)>,
    vertex_size: u64,
    sampled: Option<ImageMeta>,
    fragment: FragmentProgram,
    target_format: TextureFormat,
) -> core::result::Result<RelocatableCommands, CompileError> {
    let target = ObjectId::new(0);
    let texture = ObjectId::new(1);
    let vertex = ObjectId::new(2);
    let index = ObjectId::new(3);
    let image = |usage| {
        ResourceKind::Image(ImageMeta {
            subresources: None,
            format: if usage == TextureUsage::RENDER_ATTACHMENT {
                target_format
            } else {
                TextureFormat::Bgra8Unorm
            },
            storage_format: TextureFormat::Bgra8Unorm,
            extent: Extent2D::new(16, 16).unwrap(),
            usage,
            modifier: ImageModifier::Linear,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: 64,
                size: 1024,
            }],
        })
    };
    let resources = [
        ResourceMeta {
            id: target,
            size: 1024,
            kind: image(TextureUsage::RENDER_ATTACHMENT),
        },
        ResourceMeta {
            id: texture,
            size: sampled.as_ref().map_or(1024, |image| image.planes[0].size),
            kind: sampled.map_or_else(|| image(TextureUsage::SAMPLED), ResourceKind::Image),
        },
        ResourceMeta {
            id: vertex,
            size: vertex_size,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX,
            },
        },
        ResourceMeta {
            id: index,
            size: 24,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::INDEX,
            },
        },
    ];
    let pipeline = PipelineMeta {
        id: PipelineId::new(0),
        descriptor: RenderPipelineDesc::new(
            target_format,
            PrimitiveTopology::TriangleList,
            if matches!(fragment, FragmentProgram::TextureVertexColor(_)) {
                VertexBufferLayout::new(
                    40,
                    vec![
                        VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x4, 16),
                        VertexAttribute::new(2, VertexFormat::Float32x2, 32),
                    ],
                )
                .unwrap()
            } else {
                VertexBufferLayout::new(
                    16,
                    vec![
                        VertexAttribute::new(0, VertexFormat::Float32x2, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x2, 8),
                    ],
                )
                .unwrap()
            },
            fragment,
            BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap(),
    };
    let mut operations = vec![
        Operation::BeginRenderPass(RenderPass {
            target,
            area: PixelRect::new(0, 0, 16, 16).unwrap(),
            load: LoadOp::Load,
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(pipeline.id),
        Operation::SetVertexBuffer {
            buffer: vertex,
            offset: 0,
        },
        Operation::SetTexture(texture),
        Operation::SetSampler(sampler),
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1., 1., 1., 1.).unwrap(),
        )),
    ];
    if let Some((format, base_vertex)) = indexed {
        operations.push(Operation::SetIndexBuffer {
            buffer: index,
            offset: 0,
            format,
        });
        operations.push(Operation::DrawIndexed {
            index_count: 3,
            first_index: 2,
            base_vertex,
        });
    } else {
        operations.push(Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        });
    }
    operations.push(Operation::EndRenderPass);
    compile(CompileInput {
        capabilities: Capabilities::gm20b(1024),
        resources: &resources,
        pipelines: &[pipeline],
        operations: &operations,
    })
}

fn nearest() -> SamplerDesc {
    SamplerDesc::new(
        FilterMode::Nearest,
        FilterMode::Nearest,
        AddressMode::ClampToEdge,
        AddressMode::ClampToEdge,
    )
}

fn wire_roundtrip(commands: &RelocatableCommands) {
    use maxwell_submit_wire as wire;
    let resources: Vec<_> = commands
        .accesses
        .iter()
        .map(|a| wire::Resource {
            attachment_token: 1,
            range_offset: a.offset,
            range_size: a.size,
            access: u32::from(a.access.bits()),
        })
        .collect();
    let relocations: Vec<_> = commands
        .fixups
        .iter()
        .map(|f| {
            let (resource_index, access) = commands
                .accesses
                .iter()
                .enumerate()
                .find(|(_, a)| a.object == f.object)
                .unwrap();
            wire::Relocation {
                commands_word_offset: f.word_offset,
                source: wire::RelocationSource::Attachment(resource_index as u32),
                resource_offset: f.object_offset - access.offset,
                required_size: f.required_size,
                access: u32::from(f.access.bits()),
                encoding: wire::AddressEncoding::GpuVa64,
            }
        })
        .collect();
    let submit = wire::Submit {
        commands: &commands.words,
        resources: &resources,
        relocations: &relocations,
    };
    let mut bytes = vec![0; wire::encoded_len(submit).unwrap()];
    wire::encode(submit, &mut bytes).unwrap();
    let decoded = wire::decode(&bytes).unwrap();
    for (i, &word) in commands.words.iter().enumerate() {
        assert_eq!(decoded.commands_word(i), Some(word));
    }
}

#[test]
fn all_filter_and_address_combinations_have_exact_portable_records() {
    let filters = [FilterMode::Nearest, FilterMode::Linear];
    let addresses = [
        AddressMode::ClampToEdge,
        AddressMode::Repeat,
        AddressMode::MirrorRepeat,
    ];
    for (min, &min_filter) in filters.iter().enumerate() {
        for (mag, &mag_filter) in filters.iter().enumerate() {
            for (u, &address_u) in addresses.iter().enumerate() {
                for (v, &address_v) in addresses.iter().enumerate() {
                    let commands = draw(
                        SamplerDesc::new(min_filter, mag_filter, address_u, address_v),
                        None,
                        48,
                    )
                    .unwrap();
                    assert_eq!(
                        commands.words[22],
                        1 | ((min as u32) << 1)
                            | (((min != mag) as u32) << 6)
                            | ((u as u32) << 7)
                            | ((v as u32) << 9)
                    );
                    assert_eq!(&commands.words[62..64], &[0, 0]);
                    wire_roundtrip(&commands);
                }
            }
        }
    }
}

#[test]
fn mip_filter_and_lod_clamps_survive_wire_serialization() {
    for filter in [FilterMode::Nearest, FilterMode::Linear] {
        let sampler = nearest().with_mip_filter(filter, 1.0, 3.5).unwrap();
        let commands = draw(sampler, None, 48).unwrap();
        assert_eq!(commands.words[1], 1.0_f32.to_bits());
        assert_eq!(commands.words[63], 3.5_f32.to_bits());
        assert_eq!(
            commands.words[22] & (1 << 11),
            if filter == FilterMode::Linear {
                1 << 11
            } else {
                0
            }
        );
        wire_roundtrip(&commands);
    }
}

#[test]
fn negative_lod_limits_are_normalized_independently_after_interval_validation() {
    for filter in [FilterMode::Nearest, FilterMode::Linear] {
        for (min, max, canonical) in [
            (-3.0, -1.0, [0.0_f32, 0.0]),
            (-1.0, 0.0, [0.0_f32, 0.0]),
            (-1.0, 2.5, [0.0_f32, 2.5]),
            (1.0, 3.5, [1.0_f32, 3.5]),
        ] {
            let sampler = nearest().with_mip_filter(filter, min, max).unwrap();
            let commands = draw(sampler, None, 48).unwrap();
            assert_eq!(sampler.min_lod(), min);
            assert_eq!(sampler.max_lod(), max);
            assert_eq!(
                [commands.words[1], commands.words[63]],
                canonical.map(f32::to_bits)
            );
            assert_eq!(
                commands.words[22] & (1 << 11),
                u32::from(filter == FilterMode::Linear) << 11
            );
            wire_roundtrip(&commands);
        }
    }
    // An inverted all-negative interval must fail before both limits could
    // otherwise collapse to the same canonical zero value.
    assert!(
        nearest()
            .with_mip_filter(FilterMode::Nearest, -1.0, -2.0)
            .is_err()
    );
    assert!(
        nearest()
            .with_mip_filter(FilterMode::Nearest, f32::NAN, 0.0)
            .is_err()
    );
    assert!(
        nearest()
            .with_mip_filter(FilterMode::Nearest, 0.0, f32::INFINITY)
            .is_err()
    );
}

#[test]
fn fixed_comparison_without_reference_is_rejected() {
    assert_eq!(
        draw(
            nearest().with_compare(Some(CompareFunction::Less)),
            None,
            48
        ),
        Err(CompileError::UnsupportedFeature)
    );
}

#[test]
fn indexed_signed_base_is_preserved_and_vertex_authority_remains_bounded() {
    for format in [IndexFormat::Uint16, IndexFormat::Uint32] {
        let commands = draw(nearest(), Some((format, -1)), 48).unwrap();
        assert_eq!(commands.words[27], (-1_i32) as u32);
        assert_eq!(commands.words[28], 48);
        let vertex = commands.fixups.iter().find(|f| f.word_offset == 4).unwrap();
        assert_eq!((vertex.object_offset, vertex.required_size), (0, 48));
        wire_roundtrip(&commands);
        assert_eq!(
            draw(nearest(), Some((format, -1)), 15),
            Err(CompileError::OutOfBounds)
        );
        assert_eq!(
            draw(nearest(), Some((format, 3)), 48),
            Err(CompileError::OutOfBounds)
        );
    }
    assert_eq!(
        draw(nearest(), Some((IndexFormat::Uint16, -65_536)), 48),
        Err(CompileError::OutOfBounds)
    );
    assert_eq!(
        draw(nearest(), Some((IndexFormat::Uint32, i32::MIN)), 48)
            .unwrap()
            .words[27],
        i32::MIN as u32
    );
}

#[test]
fn checked_mip_surfaces_preserve_narrow_srgb_and_depth_sampling_formats() {
    use maxwell_image_layout::{Descriptor, LayoutKind, plan};
    let layout = plan(
        Descriptor {
            width: 16,
            height: 16,
            mip_levels: 5,
            array_layers: 1,
            bytes_per_pixel: 4,
        },
        LayoutKind::BlockLinear {
            base_y_log2: 1,
            clamp_mips: true,
        },
    )
    .unwrap();
    for (format, code) in [
        (TextureFormat::Bgra8Unorm, 0),
        (TextureFormat::Rgba8Unorm, 1),
        (TextureFormat::R8Unorm, 2),
        (TextureFormat::Rg8Unorm, 3),
        (TextureFormat::Bgra8UnormSrgb, 4),
        (TextureFormat::Rgba8UnormSrgb, 5),
        (TextureFormat::Depth32Float, 6),
    ] {
        let depth = format == TextureFormat::Depth32Float;
        let meta = ImageMeta {
            subresources: Some(layout),
            format,
            storage_format: if depth {
                TextureFormat::Depth32Float
            } else {
                TextureFormat::Bgra8Unorm
            },
            extent: Extent2D::new(16, 16).unwrap(),
            usage: TextureUsage::SAMPLED,
            modifier: if depth {
                ImageModifier::NvidiaZf32BlockLinear { tile_y_log2: 1 }
            } else {
                ImageModifier::NvidiaBlockLinear { tile_y_log2: 1 }
            },
            planes: vec![PlaneLayout {
                offset: 0,
                stride: layout.levels[0].row_pitch,
                size: layout.total_size,
            }],
        };
        let commands = draw_with_image(
            nearest()
                .with_mip_filter(FilterMode::Linear, 0.0, 4.0)
                .unwrap(),
            None,
            48,
            Some(meta),
        )
        .unwrap();
        assert_eq!(commands.words[53], 0x110);
        assert_eq!(commands.words[62], code | (4 << 8));
        assert_eq!(
            commands.words[22] & (1 << 5),
            0,
            "ordinary R8 sampling must not use alpha-mask shader semantics"
        );
        assert_eq!(
            commands
                .fixups
                .iter()
                .find(|f| f.word_offset == 6)
                .unwrap()
                .required_size,
            layout.total_size
        );
        wire_roundtrip(&commands);
    }
}

#[test]
fn r8_alpha_masks_and_ordinary_red_sampling_keep_distinct_semantics() {
    for fragment in [
        FragmentProgram::Texture(TextureSampleMode::AlphaMask),
        FragmentProgram::TextureVertexColor(TextureSampleMode::AlphaMask),
        FragmentProgram::TextureVertexColor(TextureSampleMode::Rgba),
    ] {
        let image = ImageMeta {
            subresources: None,
            format: TextureFormat::R8Unorm,
            storage_format: TextureFormat::Bgra8Unorm,
            extent: Extent2D::new(16, 16).unwrap(),
            usage: TextureUsage::SAMPLED,
            modifier: ImageModifier::Linear,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: 64,
                size: 1024,
            }],
        };
        let command = draw_fragment(nearest(), None, 120, Some(image), fragment).unwrap();
        let mask = matches!(
            fragment,
            FragmentProgram::Texture(TextureSampleMode::AlphaMask)
                | FragmentProgram::TextureVertexColor(TextureSampleMode::AlphaMask)
        );
        assert_eq!(command.words[22] & (1 << 5), if mask { 1 << 5 } else { 0 });
        assert_eq!(command.words[62], 2);
        wire_roundtrip(&command);
    }
}

#[test]
fn fixed_bgra_and_rgba_targets_use_the_same_semantic_color_surface() {
    for format in [TextureFormat::Bgra8Unorm, TextureFormat::Rgba8Unorm] {
        let command =
            draw_fragment_target(nearest(), None, 48, None, FragmentProgram::Solid, format)
                .unwrap();
        assert_eq!(&command.words[10..13], &[16, 16, 64]);
        assert_eq!(command.words[52], 0);
        wire_roundtrip(&command);
    }
}

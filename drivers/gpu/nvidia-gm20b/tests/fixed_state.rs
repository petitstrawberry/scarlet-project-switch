//! Run the production portable-state checks and TSC template on the host.
#[path = "../src/method.rs"]
mod method;

fn viewport_record(values: [f32; 6]) -> [u32; 64] {
    let mut words = [0; 64];
    words[0] = 7;
    for (word, value) in words[32..38].iter_mut().zip(values) {
        *word = value.to_bits();
    }
    words
}

#[test]
fn fixed_viewport_record_has_closed_v2_grammar_and_rejects_invalid_values() {
    let values = [2.25, 27.5, 17.5, -20.25, 0.8, 0.2];
    let words = viewport_record(values);
    assert_eq!(method::fixed_viewport_record(&words, true), Ok(values));
    assert!(method::fixed_viewport_record(&words, false).is_err());
    for index in (1..32).chain(38..64) {
        let mut bad = words;
        bad[index] = 1;
        assert!(method::fixed_viewport_record(&bad, true).is_err());
    }
    for (index, invalid) in [
        (0, -1.0),
        (1, -1.0),
        (2, 0.0),
        (2, -1.0),
        (4, -0.1),
        (5, 1.1),
    ] {
        let mut bad = values;
        bad[index] = invalid;
        assert!(method::fixed_viewport_record(&viewport_record(bad), true).is_err());
    }
    for index in 0..6 {
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut bad = values;
            bad[index] = invalid;
            assert!(method::fixed_viewport_record(&viewport_record(bad), true).is_err());
        }
    }
}

#[test]
fn native_fixed_viewport_preserves_fractional_signed_and_reversed_depth_values() {
    let native =
        method::fixed_viewport_native([2.25, 27.5, 17.5, -20.25, 0.75, 0.25], 32, 32).unwrap();
    assert_eq!(
        native.translate,
        [11_f32.to_bits(), 17.375_f32.to_bits(), 0.5_f32.to_bits()]
    );
    assert_eq!(
        native.scale,
        [
            8.75_f32.to_bits(),
            10.125_f32.to_bits(),
            (-0.25_f32).to_bits()
        ]
    );
    assert_eq!(native.clip, [18 << 16 | 2, 21 << 16 | 7]);
    assert_eq!(native.depth, [0.25_f32.to_bits(), 0.75_f32.to_bits()]);
    let zero = method::fixed_viewport_native([0., 16., 16., 0., 0., 1.], 32, 32).unwrap();
    assert_eq!(zero.clip, [16 << 16, 16]);
    assert_eq!(zero.scale[1], (-0.0_f32).to_bits());
    for values in [
        [24., 0., 9., 16., 0., 1.],
        [0., 4., 16., -5., 0., 1.],
        [0., 30., 16., 3., 0., 1.],
    ] {
        assert!(method::fixed_viewport_native(values, 32, 32).is_none());
    }
}

#[test]
fn viewport_applies_only_to_an_adjacent_fixed_draw_and_cannot_leak() {
    let words = viewport_record([0., 0., 16., 16., 0., 1.]);
    let mut draw = [0; 64];
    draw[0] = 2;
    draw[10] = 32;
    draw[11] = 32;
    let mut state = method::FixedViewportState::new();
    state.validate_operation(&words, true).unwrap();
    assert!(state.finish().is_err());
    state.validate_operation(&draw, true).unwrap();
    state.finish().unwrap();
    // A subsequent smaller draw has the default, rather than the prior state.
    draw[10] = 8;
    draw[11] = 8;
    state.validate_operation(&draw, true).unwrap();
    for opcode in [1, 3, 4, 5, 6, 7, 8] {
        let mut state = method::FixedViewportState::new();
        state.validate_operation(&words, true).unwrap();
        let mut next = [0; 64];
        next[0] = opcode;
        assert!(state.validate_operation(&next, true).is_err());
    }
    let mut state = method::FixedViewportState::new();
    state.validate_operation(&words, true).unwrap();
    assert!(state.validate_operation(&draw, true).is_err());
}

#[test]
fn all_portable_sampler_states_generate_exact_bounded_tsc_fields() {
    let wraps = [2, 0, 1]; // ClampToEdge, Repeat, MirrorRepeat in G80 TSC.
    for min in 0..2 {
        for mag in 0..2 {
            for u in 0..3 {
                for v in 0..3 {
                    let flags = (min << 1) | (u32::from(min != mag) << 6) | (u << 7) | (v << 9);
                    let descriptor = method::draw_sampler_descriptor(flags).unwrap();
                    assert_eq!(
                        descriptor[0],
                        0x26000 | wraps[u as usize] | (wraps[v as usize] << 3) | (2 << 6)
                    );
                    assert_eq!(descriptor[1], 0x40 | ((min + 1) << 4) | (mag + 1));
                    assert_eq!(&descriptor[2..], &[0; 6]);
                    // Blend/cull/front-face/mask never alter the TSC template.
                    assert_eq!(
                        method::draw_sampler_descriptor(flags | 0x35),
                        Some(descriptor)
                    );
                }
            }
        }
    }
    assert_eq!(method::draw_sampler_descriptor(0).unwrap()[1], 0x51);
    assert_eq!(method::draw_sampler_descriptor(2).unwrap()[1], 0x62);
}

#[test]
fn unassigned_address_modes_cull_values_and_state_bits_are_rejected() {
    for flags in [3 << 7, 3 << 9, 3 << 2, 1 << 12, 1 << 31, u32::MAX] {
        assert!(!method::draw_state_valid(flags));
        assert_eq!(method::draw_sampler_descriptor(flags), None);
    }
}

#[test]
fn signed_index_resolution_rejects_underflow_overflow_and_partial_records() {
    use method::indexed_vertex_in_bounds as valid;
    assert!(valid(1, -1, 16, 48));
    assert!(valid(3, -1, 16, 48));
    assert!(!valid(0, -1, 16, 48));
    assert!(!valid(4, -1, 16, 48));
    assert!(!valid(3, -1, 16, 47));
    assert!(!valid(0, 0, 0, 48));
    assert!(valid(2_147_483_648, i32::MIN, 16, 16));
    assert!(!valid(2_147_483_647, i32::MIN, 16, 16));
    assert!(!valid(u32::MAX, i32::MAX, u32::MAX, u64::MAX));
    assert!(!valid(u32::MAX, 1, 16, 48));
}

fn blend_record(mask: u32, components: [u32; 6]) -> [u32; 64] {
    let mut words = [0; 64];
    words[0] = 8;
    words[21] = mask;
    words[22] = 1;
    words[23..29].copy_from_slice(&components);
    words
}

#[test]
fn fixed_blend_maps_every_factor_equation_and_component_mask_to_native_fields() {
    let factors = [0x4000, 0x4001, 0x4302, 0x4303, 0x4304, 0x4305];
    let equations = [0x8006, 0x800a, 0x800b];
    for src in 0..6 {
        for dst in 0..6 {
            for eq in 0..3 {
                for mask in 0..16 {
                    let asrc = (src + 1) % 6;
                    let adst = (dst + 2) % 6;
                    let aeq = (eq + 1) % 3;
                    let native = method::fixed_blend_record(
                        &blend_record(mask, [src, dst, eq, asrc, adst, aeq]),
                        true,
                    )
                    .unwrap();
                    assert_eq!(native.enabled, 1);
                    assert_eq!(
                        native.color,
                        [
                            equations[eq as usize],
                            factors[src as usize],
                            factors[dst as usize]
                        ]
                    );
                    assert_eq!(
                        native.alpha,
                        [
                            equations[aeq as usize],
                            factors[asrc as usize],
                            factors[adst as usize]
                        ]
                    );
                    assert_eq!(
                        native.color_mask,
                        (0..4).map(|c| ((mask >> c) & 1) << (c * 4)).sum::<u32>()
                    );
                }
            }
        }
    }
    let legacy = method::fixed_legacy_blend(true);
    assert_eq!(legacy.color, [0x8006, 0x4302, 0x4303]);
    assert_eq!(legacy.alpha, [0x8006, 0x4001, 0x4303]);
    assert_eq!(legacy.color_mask, 0x1111);
    assert_eq!(method::fixed_legacy_blend(false).enabled, 0);
}

#[test]
fn fixed_blend_is_a_closed_v2_record_with_canonical_disabled_state() {
    let good = blend_record(3, [0, 2, 0, 0, 2, 0]);
    assert!(method::fixed_blend_record(&good, true).is_ok());
    assert!(method::fixed_blend_record(&good, false).is_err());
    for index in (1..21).chain(29..64) {
        let mut bad = good;
        bad[index] = 1;
        assert!(method::fixed_blend_record(&bad, true).is_err());
    }
    for (index, value) in [
        (21, 16),
        (22, 2),
        (23, 6),
        (24, 6),
        (25, 3),
        (26, 6),
        (27, 6),
        (28, 3),
    ] {
        let mut bad = good;
        bad[index] = value;
        assert!(method::fixed_blend_record(&bad, true).is_err());
    }
    let mut disabled = good;
    disabled[22] = 0;
    assert!(method::fixed_blend_record(&disabled, true).is_err());
    disabled[23..29].copy_from_slice(&[1, 0, 0, 1, 0, 0]);
    assert_eq!(
        method::fixed_blend_record(&disabled, true).unwrap().enabled,
        0
    );
    assert!(method::fixed_blend_record(&disabled[..63], true).is_err());
}

#[test]
fn blend_and_viewport_are_scoped_to_one_fixed_draw_in_either_combination() {
    let blend = blend_record(1, [0, 2, 0, 0, 2, 0]);
    let viewport = viewport_record([0., 0., 16., 16., 0., 1.]);
    let mut draw = [0; 64];
    draw[0] = 2;
    draw[10] = 32;
    draw[11] = 32;
    for with_viewport in [false, true] {
        let mut state = method::FixedViewportState::new();
        state.validate_operation(&blend, true).unwrap();
        assert!(state.finish().is_err());
        if with_viewport {
            state.validate_operation(&viewport, true).unwrap();
        }
        state.validate_operation(&draw, true).unwrap();
        state.finish().unwrap();
        state.validate_operation(&draw, false).unwrap();
        state.finish().unwrap();
    }
    for opcode in [1, 3, 4, 5, 6, 8] {
        let mut state = method::FixedViewportState::new();
        state.validate_operation(&blend, true).unwrap();
        let mut next = [0; 64];
        next[0] = opcode;
        assert!(state.validate_operation(&next, true).is_err());
    }
}

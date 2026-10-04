// SPDX-License-Identifier: GPL-2.0-only
//! Actual WGSL -> VirGL frontend -> pinned NAK SM52 output fixtures.
use maxwell_program_wire::*;

#[test]
fn frontend_golden_stage_packages_pass_kernel_verifier() {
    let fixtures: &[&[u8]] = &[
        include_bytes!("fixtures/1f3ac132539630e4.mxp"),
        include_bytes!("fixtures/2731989d690b8b00.mxp"),
        include_bytes!("fixtures/3d273852bb50ab3d.mxp"),
        include_bytes!("fixtures/424cc1b38b2e5e4a.mxp"),
        include_bytes!("fixtures/48b49c8866f93609.mxp"),
        include_bytes!("fixtures/4c282f2dd9cb9b88.mxp"),
        include_bytes!("fixtures/5260333a84a1c937.mxp"),
        include_bytes!("fixtures/73e693e2073ffee6.mxp"),
        include_bytes!("fixtures/aaea911f7f09a3a5.mxp"),
        include_bytes!("fixtures/b00ad2d6492af09d.mxp"),
        include_bytes!("fixtures/cacd7a143188d3d8.mxp"),
        include_bytes!("fixtures/da143feb137334f7.mxp"),
        include_bytes!("fixtures/f28b5a8ab5dd21ed.mxp"),
        include_bytes!("fixtures/fec023b589726751.mxp"),
    ];
    for (index, bytes) in fixtures.iter().enumerate() {
        let p = Program::parse(bytes).unwrap();
        let m = p.metadata;
        let valid = validate(
            &p,
            &Limits {
                cb_sizes: m.cb_sizes,
                resource_mask: m.resource_mask,
            },
        );
        assert!(valid.is_ok(), "fixture {index}: {:?}", valid.err());
    }
}

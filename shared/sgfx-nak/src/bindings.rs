// SPDX-License-Identifier: MIT
// Safe Rust mirrors of the C records referenced by upstream NAK IR/SPH.
// No FFI symbols or native Mesa dependencies are used.
#![allow(non_camel_case_types)]
pub const NAK_TS_DOMAIN_ISOLINE: u8 = 0;
pub const NAK_TS_DOMAIN_TRIANGLE: u8 = 1;
pub const NAK_TS_DOMAIN_QUAD: u8 = 2;
pub const NAK_TS_SPACING_INTEGER: u8 = 0;
pub const NAK_TS_SPACING_FRACT_ODD: u8 = 1;
pub const NAK_TS_SPACING_FRACT_EVEN: u8 = 2;
pub type nak_mesh_topology = u8;
#[derive(Debug)]
pub struct nak_xfb_info {
    pub stride: [u32; 4],
    pub stream: [u8; 4],
    pub attr_count: [u8; 4],
    pub attr_index: [[u8; 128]; 4],
}
#[derive(Default)]
pub struct nak_fs_key {
    pub zs_self_dep: bool,
    pub force_sample_shading: bool,
    pub uses_underestimate: bool,
}

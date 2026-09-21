// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! Tegra210 SE DRBG, seeded by the hardware ring-oscillator entropy source.
//! Register protocol and DMA retirement: Hekate e487de8fdd6ca9c3f608d1d18c097a86355912b9,
//! bdk/sec/se.{c,h}, se_t210.h; bdk/soc/{clock,t210}.h.
//! Explicit DRBG instantiation: Atmosphere 6e6af694244002fd6799703a54a5e24f0a0b9ac1,
//! libraries/libexosphere/source/se/se_rng.cpp.

pub mod engine;
#[cfg(target_os = "none")]
extern crate alloc;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

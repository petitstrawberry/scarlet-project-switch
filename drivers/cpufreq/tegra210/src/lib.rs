// SPDX-License-Identifier: GPL-2.0-only
//! Erista A57 cluster frequency control using PLLX and the MAX77621 CPU rail.
//! Register layouts, PLL limits and CVB voltages follow NVIDIA's Linux driver.
//! DFLL, overclocking, EMC scaling and thermal policy are not implemented here.

#![no_std]
extern crate alloc;

#[cfg(target_os = "none")]
mod clock;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;

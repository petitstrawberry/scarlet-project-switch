// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! Tegra210 SDMMC1 binding; card protocol and SDHCI transfers live in Scarlet.
//! Reference: Switchroot Linux 2d0059fd, drivers/mmc/host/sdhci-tegra.c,
//! and the ODIN board's legacy 3.3 V production settings.

extern crate alloc;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

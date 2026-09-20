// SPDX-License-Identifier: GPL-2.0-only
#![no_std]

//! Tegra210 peripheral transport, kept outside the common kernel.
//! UART flow control and FIFO handling follow Switchroot Linux 5.1.2,
//! serial-tegra.c at 2d0059fd3167a8df756de2aa0489d4aa70a9fc15.
//! Board register definitions also follow Hekate
//! e487de8fdd6ca9c3f608d1d18c097a86355912b9,
//! bdk/soc/{i2c,clock,pinmux,gpio,uart}.{c,h}. I2C1/I2C3/I2C5 and UARTB/C
//! are enabled here; display, memory, and other firmware clocks are preserved.

extern crate alloc;
#[cfg(target_os = "none")]
mod gpu;
pub mod packet;
#[cfg(target_os = "none")]
pub use gpu::{GpuPlatform, GpuPlatformState};
#[cfg(target_os = "none")]
mod vic;
#[cfg(target_os = "none")]
pub use vic::VicPlatform;
#[cfg(target_os = "none")]
mod nvdec;
#[cfg(target_os = "none")]
pub use nvdec::NvdecPlatform;
#[cfg(target_os = "none")]
mod host1x;
#[cfg(target_os = "none")]
pub use host1x::Host1xSyncpoint;
#[cfg(target_os = "none")]
mod sdmmc;
#[cfg(target_os = "none")]
pub use sdmmc::SdmmcPlatform;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::*;
#[cfg(target_os = "none")]
mod soctherm;
#[cfg(target_os = "none")]
mod thermal;
#[cfg(target_os = "none")]
pub use thermal::maybe_register_gpu_zone;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

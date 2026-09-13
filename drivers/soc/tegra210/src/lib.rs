// SPDX-License-Identifier: GPL-2.0-only
#![no_std]

//! Tegra210 peripheral transport, kept outside the common kernel.
//! UART flow control and FIFO handling follow Switchroot Linux 5.1.2,
//! serial-tegra.c at 2d0059fd3167a8df756de2aa0489d4aa70a9fc15.
//! Board register definitions also follow Hekate
//! e487de8fdd6ca9c3f608d1d18c097a86355912b9,
//! bdk/soc/{i2c,clock,pinmux,gpio,uart}.{c,h}. Only I2C3/I2C5 and UARTB/C
//! are enabled here; display, memory, and other firmware clocks are preserved.

extern crate alloc;
pub mod packet;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::*;

#[cfg(not(target_os = "none"))]
pub fn force_link() {}

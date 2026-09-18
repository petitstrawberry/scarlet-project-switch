// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! Tegra210 DC scanout through the ordinary Scarlet display interface.
//! Adopts an already running DSI mode; panel power, DSI and firmware clocks
//! are preserved. This is not yet a cold panel initialization or modesetter.

extern crate alloc;
#[cfg(target_os = "none")]
mod block_linear;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

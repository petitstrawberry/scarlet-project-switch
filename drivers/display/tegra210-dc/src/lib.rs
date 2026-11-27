// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! Tegra210 DC scanout through the ordinary Scarlet display interface.
//! Adopts the running DSI panel and drives the independent dock DP head.

extern crate alloc;
#[cfg(test)]
extern crate std;
#[cfg(any(target_os = "none", test))]
mod backlight;
#[cfg(target_os = "none")]
mod block_linear;
#[cfg(target_os = "none")]
mod dock_surface;
#[cfg(any(target_os = "none", test))]
mod dp_link;
#[cfg(any(target_os = "none", test))]
mod dpaux;
#[cfg(target_os = "none")]
mod external;
#[cfg(any(target_os = "none", test))]
mod head;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(any(target_os = "none", test))]
mod sor;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

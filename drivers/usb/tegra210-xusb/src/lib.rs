// SPDX-License-Identifier: GPL-2.0-only
#![no_std]

//! Tegra210 XUSB firmware and host controller integration.
//!
//! The Falcon boot ABI and CSB register protocol follow Linux xhci-tegra.c
//! at 70293240c5ce675a67bfc48f419b093023b862b3.

extern crate alloc;

#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub mod charger;
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub mod falcon;
pub mod firmware;
pub mod mailbox;
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub mod pd;
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub mod typec;

#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod config;
pub mod controller;
pub mod interrupt;
#[cfg(target_os = "none")]
pub mod native;
pub mod resources;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

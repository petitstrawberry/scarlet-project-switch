// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
extern crate alloc;
#[cfg(target_os = "none")]
mod codec;
#[cfg(target_os = "none")]
mod pcm;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

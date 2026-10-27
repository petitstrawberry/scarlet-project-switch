// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;
pub mod chip;
pub mod control;
pub mod data;
mod dma_copy;
pub mod interrupt;
pub mod msgbuf;
pub mod pcie;
pub mod security;
pub mod wireless;

#[cfg(target_os = "none")]
mod activity;

#[cfg(target_os = "none")]
pub mod native;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;

// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! Attached official Joy-Con input. Wire protocol follows Hekate
//! e487de8fdd6ca9c3f608d1d18c097a86355912b9, bdk/input/joycon.c.
//! The kernel emits gamepad events. Menu navigation/repeat belong in userspace.
extern crate alloc;
pub mod protocol;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

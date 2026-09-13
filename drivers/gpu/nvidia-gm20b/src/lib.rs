// SPDX-License-Identifier: GPL-2.0-only
#![no_std]

//! GM20B hardware bring-up through Scarlet's common GPU control interface.
//! This first stage owns GPU power/reset, reads hardware identity and flushes
//! the MC GPU client. GR firmware execution, GMMU/channel management and SGFX
//! command execution are not yet implemented or advertised.

extern crate alloc;

#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

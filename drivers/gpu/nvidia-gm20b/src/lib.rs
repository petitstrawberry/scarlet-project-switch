// SPDX-License-Identifier: GPL-2.0-only
#![no_std]

//! GM20B hardware bring-up through Scarlet's common GPU control interface.
//! This first stage owns GPU power/reset, reads hardware identity and flushes
//! the MC GPU client, checks a private GMMU/BAR1 address space and executes
//! private PFIFO host semaphore/reference methods, then boots signed PMU/FECS
//! firmware and ordinary GPCCS, and saves a private golden graphics context.
//! Public address-space/channel management and SGFX
//! command execution are not yet implemented or advertised.

extern crate alloc;

#[cfg(target_os = "none")]
mod context;
#[cfg(target_os = "none")]
mod executor;
#[cfg(target_os = "none")]
mod fifo;
#[cfg(target_os = "none")]
mod firmware;
#[cfg(target_os = "none")]
mod gmmu;
#[cfg(target_os = "none")]
mod gr;
#[cfg(target_os = "none")]
mod graphics;
#[cfg(target_os = "none")]
mod method;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

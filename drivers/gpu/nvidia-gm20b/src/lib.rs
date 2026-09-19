// SPDX-License-Identifier: GPL-2.0-only
#![no_std]

//! GM20B hardware bring-up through Scarlet's common GPU control interface.
//! This first stage owns GPU power/reset, reads hardware identity and flushes
//! the MC GPU client, checks a private GMMU/BAR1 address space and executes
//! prepares private PFIFO state, boots signed PMU/FECS firmware and ordinary
//! GPCCS, saves a private golden graphics context, then proves PFIFO host
//! semaphore/reference execution.
//! Ready execution is withheld until PFIFO, authenticated GR and every real
//! shader/draw/copy admission check completes on the physical GPU.

extern crate alloc;

#[cfg(target_os = "none")]
mod asynchronous;
#[cfg(target_os = "none")]
mod clock;
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
mod hardware;
#[cfg(target_os = "none")]
mod method;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
mod utilization;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

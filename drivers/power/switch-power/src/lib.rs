// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! Switch battery telemetry, preserving the firmware's charger and gauge setup.
//! MAX17050 conversions follow its data sheet and Hekate e487de8f's effective
//! 10-milliohm shunt (physical 5 milliohms with the board's 2x current gain).
//! BQ24193 status/limits follow TI SLUSBG7A registers 00, 08 and 0A.

#[cfg(target_os = "none")]
mod runtime;
pub mod telemetry;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

#![no_std]
#![no_main]

// Exercise the production entry and framebuffer parser with a minimal kernel.
#[path = "../../../../projects/aarch64-switch-l4t-console/bsp/src/early.rs"]
mod early;
include!("../../../../projects/aarch64-switch-l4t-console/bsp/src/entry.rs");

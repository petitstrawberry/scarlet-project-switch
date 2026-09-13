#![no_std]
#![no_main]

#[path = "../../../aarch64-switch-l4t/bsp/src/early.rs"]
mod early;
include!("../../../aarch64-switch-l4t/bsp/src/entry.rs");

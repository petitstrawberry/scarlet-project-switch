//! Dynamically loaded Scarlet Maxwell backend using the SGFX C ABI v2.
//!
//! Each opaque object and its destructor belong to this library. Rust resource
//! tables, command buffers, allocations, and completion implementations never
//! cross the dynamic-library boundary.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(any(
    target_os = "scarlet",
    all(target_os = "linux", target_arch = "aarch64"),
    test
))]
mod boundary;

#[cfg(any(
    target_os = "scarlet",
    all(target_os = "linux", target_arch = "aarch64"),
    test
))]
mod ycbcr;

#[cfg(any(
    target_os = "scarlet",
    all(target_os = "linux", target_arch = "aarch64")
))]
mod runtime;

#[cfg(any(
    target_os = "scarlet",
    all(target_os = "linux", target_arch = "aarch64")
))]
pub use runtime::{
    sgfx_backend_get_api_v2, sgfx_backend_get_driver_api_v2, sgfx_backend_get_ycbcr_api_v2,
};

// ThinLTO can merge std's executable startup into a cdylib. Keep the private
// runtime exports hidden so scarlet-ld cannot interpose this std on the host.
#[cfg(target_os = "scarlet")]
core::arch::global_asm!(".hidden __scarlet_getauxval", ".hidden __scarlet_start");

use super::*;
use crate::ycbcr::{YcbcrApi, YcbcrConversion, decode};

unsafe extern "C" fn import_ycbcr(p: Object, slot: u32, raw: i32, value: YcbcrConversion) -> i32 {
    status((|| {
        // Adopt before every argument check: the ABI transfers handle ownership
        // even for invalid conversion records, slots, or poisoned sessions.
        let handle = unsafe { maxwell::Handle::from_raw(raw) }.map_err(handle_error)?;
        let s = unsafe { session(p) }?;
        let conversion = decode(value)?;
        let id = s.table.abi_texture(slot).map_err(ir_error)?.id();
        s.inner
            .import_ycbcr_texture(id, handle, conversion)
            .map_err(error)
    })())
}

/// Negotiate the optional YCbCr ABI v2 table after the main entry point.
///
/// # Safety
/// `out` must be writable for `size` bytes. The import callback accepts only
/// session objects from this library and consumes one owned Scarlet handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sgfx_backend_get_ycbcr_api_v2(
    version: u32,
    size: usize,
    out: *mut YcbcrApi,
) -> i32 {
    if let Err(code) = check_api(version, size, out) {
        return code;
    }
    unsafe {
        out.write(YcbcrApi {
            version: abi::VERSION,
            size: core::mem::size_of::<YcbcrApi>() as u32,
            import_ycbcr,
        });
    }
    abi::OK
}

//! Validation shared by the Scarlet entry points and syscall-free host tests.
use sgfx_backend_abi::{self as abi, Object, Span};
use sgfx_core::ir;

pub(crate) fn ir_error(e: ir::Error) -> i32 {
    match e {
        ir::Error::OutOfMemory
        | ir::Error::ResourceLimitExceeded
        | ir::Error::CommandLimitExceeded => abi::OUT_OF_MEMORY,
        _ => abi::INVALID,
    }
}

pub(crate) fn valid_pointer<T>(p: *const T) -> bool {
    !p.is_null()
        && (p as usize).is_multiple_of(core::mem::align_of::<T>())
        && (p as usize)
            .checked_add(core::mem::size_of::<T>())
            .is_some()
}

pub(crate) fn validate_span<T>(p: *const T, len: usize) -> Result<(), i32> {
    if len == 0 {
        return Ok(());
    }
    let bytes = len
        .checked_mul(core::mem::size_of::<T>())
        .filter(|bytes| *bytes <= isize::MAX as usize)
        .ok_or(abi::INVALID)?;
    if !valid_pointer(p) || (p as usize).checked_add(bytes).is_none() {
        return Err(abi::INVALID);
    }
    Ok(())
}

pub(crate) unsafe fn object_mut<'a, T>(p: Object) -> Result<&'a mut T, i32> {
    let p = p.cast::<T>();
    if !valid_pointer(p) {
        return Err(abi::INVALID);
    }
    // SAFETY: The ABI caller guarantees the object's kind, life, and exclusive access.
    Ok(unsafe { &mut *p })
}

pub(crate) unsafe fn object_ref<'a, T>(p: *const T) -> Result<&'a T, i32> {
    if !valid_pointer(p) {
        return Err(abi::INVALID);
    }
    // SAFETY: The ABI caller guarantees a live, readable object of the indicated kind.
    Ok(unsafe { &*p })
}

pub(crate) unsafe fn span<'a, T>(s: Span<T>) -> Result<&'a [T], i32> {
    validate_span(s.data, s.len)?;
    // SAFETY: The ABI caller supplies readable initialized storage for the call.
    Ok(unsafe { s.as_slice() })
}

pub(crate) const fn name<const N: usize>(s: &[u8]) -> [u8; N] {
    assert!(s.len() < N);
    let mut out = [0; N];
    let mut i = 0;
    while i < s.len() {
        out[i] = s[i];
        i += 1;
    }
    out
}

pub(crate) fn check_api<T>(version: u32, size: usize, out: *mut T) -> Result<(), i32> {
    if version != abi::VERSION || size < core::mem::size_of::<T>() {
        return Err(abi::ABI_MISMATCH);
    }
    if !valid_pointer(out) {
        return Err(abi::INVALID);
    }
    Ok(())
}

pub(crate) unsafe fn host_info<'a>(p: *const abi::HostInfo) -> Result<&'a abi::HostInfo, i32> {
    let host = unsafe { object_ref(p) }?;
    if host.size as usize != core::mem::size_of::<abi::HostInfo>() || host.reserved != 0 {
        return Err(abi::ABI_MISMATCH);
    }
    Ok(host)
}

pub(crate) fn retirement_failure(changed: bool, code: i32) -> (bool, i32) {
    if code == abi::BUSY {
        if changed {
            // A partially retired mirror cannot offer a retryable rejection.
            (true, abi::DEVICE_LOST)
        } else {
            (false, abi::BUSY)
        }
    } else {
        (true, code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        mem::{MaybeUninit, size_of},
        ptr,
    };

    #[test]
    fn borrowed_spans_reject_bad_alignment_capacity_and_address_wrap() {
        assert_eq!(validate_span::<u64>(ptr::null(), 1), Err(abi::INVALID));
        assert_eq!(
            validate_span::<u64>(1usize as *const u64, 1),
            Err(abi::INVALID)
        );
        let value = 1u64;
        assert_eq!(validate_span(&value, usize::MAX), Err(abi::INVALID));
        assert_eq!(
            validate_span((usize::MAX - 7) as *const u64, 1),
            Err(abi::INVALID)
        );
        assert_eq!(
            unsafe {
                span::<u64>(Span {
                    data: ptr::null(),
                    len: 0,
                })
            }
            .unwrap(),
            &[]
        );
        let words = [1u64, 2];
        let borrowed = unsafe { span(Span::from_slice(&words)) }.unwrap();
        assert_eq!(borrowed, &words);
        assert_eq!(borrowed.as_ptr(), words.as_ptr());
    }

    #[test]
    fn opaque_objects_reject_null_and_misaligned_pointers_before_access() {
        assert_eq!(
            unsafe { object_mut::<u64>(ptr::null_mut()) }.unwrap_err(),
            abi::INVALID
        );
        assert_eq!(
            unsafe { object_ref::<u64>(1usize as *const u64) }.unwrap_err(),
            abi::INVALID
        );
        let mut value = 1u64;
        *unsafe { object_mut::<u64>((&mut value as *mut u64).cast()) }.unwrap() = 2;
        assert_eq!(*unsafe { object_ref(&value) }.unwrap(), 2);
    }

    #[test]
    fn negotiation_preserves_version_size_alignment_and_host_contract() {
        let mut out = MaybeUninit::<abi::BackendApi>::uninit();
        assert_eq!(
            check_api(1, size_of::<abi::BackendApi>(), out.as_mut_ptr()),
            Err(abi::ABI_MISMATCH)
        );
        assert_eq!(
            check_api(
                abi::VERSION,
                size_of::<abi::BackendApi>() - 1,
                out.as_mut_ptr()
            ),
            Err(abi::ABI_MISMATCH)
        );
        assert_eq!(
            check_api::<abi::BackendApi>(
                abi::VERSION,
                size_of::<abi::BackendApi>(),
                ptr::null_mut()
            ),
            Err(abi::INVALID)
        );
        assert_eq!(
            check_api(
                abi::VERSION,
                size_of::<abi::BackendApi>(),
                1usize as *mut abi::BackendApi
            ),
            Err(abi::INVALID)
        );
        assert_eq!(
            check_api(abi::VERSION, size_of::<abi::BackendApi>(), out.as_mut_ptr()),
            Ok(())
        );
        let mut extension = MaybeUninit::<abi::DriverApi>::uninit();
        assert_eq!(
            check_api(
                abi::VERSION,
                size_of::<abi::DriverApi>(),
                extension.as_mut_ptr()
            ),
            Ok(())
        );
        let host = abi::HostInfo {
            size: size_of::<abi::HostInfo>() as u32,
            reserved: 0,
            cpu_features: abi::CPU_AARCH64_LSE,
        };
        assert_eq!(unsafe { host_info(ptr::null()) }.unwrap_err(), abi::INVALID);
        assert_eq!(
            unsafe {
                host_info(&abi::HostInfo {
                    reserved: 1,
                    ..host
                })
            }
            .unwrap_err(),
            abi::ABI_MISMATCH
        );
        assert_eq!(
            unsafe { host_info(&abi::HostInfo { size: 0, ..host }) }.unwrap_err(),
            abi::ABI_MISMATCH
        );
        assert_eq!(
            unsafe { host_info(&host) }.unwrap().cpu_features,
            abi::CPU_AARCH64_LSE
        );
    }

    #[test]
    fn logical_allocation_errors_and_identifiers_keep_abi_meaning() {
        assert_eq!(ir_error(ir::Error::InvalidDescriptor), abi::INVALID);
        assert_eq!(ir_error(ir::Error::OutOfMemory), abi::OUT_OF_MEMORY);
        assert_eq!(
            ir_error(ir::Error::ResourceLimitExceeded),
            abi::OUT_OF_MEMORY
        );
        assert_eq!(
            ir_error(ir::Error::CommandLimitExceeded),
            abi::OUT_OF_MEMORY
        );
        let backend = name::<64>(b"scarlet-maxwell");
        assert_eq!(&backend[..b"scarlet-maxwell".len()], b"scarlet-maxwell");
        assert!(backend[b"scarlet-maxwell".len()..].iter().all(|v| *v == 0));
    }

    #[test]
    fn retirement_contention_is_retryable_only_before_physical_mutation() {
        assert_eq!(retirement_failure(false, abi::BUSY), (false, abi::BUSY));
        assert_eq!(
            retirement_failure(true, abi::BUSY),
            (true, abi::DEVICE_LOST)
        );
        assert_eq!(
            retirement_failure(true, abi::OUT_OF_MEMORY),
            (true, abi::OUT_OF_MEMORY)
        );
        assert_eq!(
            retirement_failure(false, abi::INVALID),
            (true, abi::INVALID)
        );
    }
}

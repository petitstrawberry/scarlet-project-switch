use crate::boundary::*;
use abi::{Object, Span};
use sgfx_backend_abi as abi;
use sgfx_backend_scarlet_maxwell as maxwell;
use sgfx_core::{
    backend::{CommandExecutor, CommandSubmitter, CompletionStatus, SubmitError},
    ir,
};
use std::{rc::Rc, time::Duration};

mod driver;
pub use driver::sgfx_backend_get_driver_api_v2;
mod ycbcr;
pub use ycbcr::sgfx_backend_get_ycbcr_api_v2;

struct Session {
    inner: maxwell::MappedTargetSession,
    table: Rc<ir::ResourceTable>,
    source: u64,
    revision: u64,
    poisoned: bool,
}

// Copy the stable C record fields across independently pinned Rust ABI crates.
// No pointer is dereferenced and no Rust type identity is assumed here.
fn core_batch(batch: abi::Batch) -> ir::abi::Batch {
    ir::abi::Batch {
        table: batch.table,
        words: ir::abi::Span {
            data: batch.words.data,
            len: batch.words.len,
        },
        count: batch.count,
    }
}

fn error(e: maxwell::IrSubmitError) -> i32 {
    use maxwell::IrSubmitError as E;
    match e {
        E::InvalidIr(e) => ir_error(e),
        E::ResourceTableMismatch
        | E::ContextMismatch
        | E::TargetExtentMismatch
        | E::ImageNotMapped
        | E::TextureAlreadyMapped
        | E::ImageAlreadyMapped => abi::INVALID,
        E::Unsupported(_) | E::AsyncUnsupported | E::ShaderCompile(_) => abi::UNSUPPORTED,
        E::OutOfMemory | E::SubmissionTooLarge => abi::OUT_OF_MEMORY,
        E::ResourceBusy => abi::BUSY,
        E::Backend(e) => handle_error(e),
        E::Codegen(e) => {
            use sgfx_codegen_maxwell::CompileError as C;
            match e {
                C::UnsupportedFeature => abi::UNSUPPORTED,
                C::OutOfMemory | C::CommandBudgetExceeded => abi::OUT_OF_MEMORY,
                _ => abi::INVALID,
            }
        }
        E::SubmitWire(maxwell_submit_wire::Error::UnsupportedVersion) => abi::UNSUPPORTED,
        E::SubmitWire(_) => abi::INVALID,
        E::CompletionUnavailable | E::CompletionFailed(_) => abi::DEVICE_LOST,
    }
}

fn handle_error(e: maxwell::HandleError) -> i32 {
    use maxwell::HandleError as E;
    match e {
        E::InvalidHandle | E::InvalidParameter => abi::INVALID,
        E::Unsupported => abi::UNSUPPORTED,
        E::OutOfResources => abi::OUT_OF_MEMORY,
        E::NotFound => abi::INITIALIZATION_FAILED,
        E::PermissionDenied | E::SystemError(_) => abi::DEVICE_LOST,
    }
}

fn status(result: Result<(), i32>) -> i32 {
    result.err().unwrap_or(abi::OK)
}

unsafe fn session<'a>(p: Object) -> Result<&'a mut Session, i32> {
    let s = unsafe { object_mut::<Session>(p) }?;
    if s.poisoned {
        Err(abi::DEVICE_LOST)
    } else {
        Ok(s)
    }
}

unsafe extern "C" fn open(path: Span<u8>, out: *mut Object, caps: *mut u64) -> i32 {
    let out_valid = valid_pointer(out);
    let caps_valid = valid_pointer(caps);
    if out_valid {
        unsafe { out.write(core::ptr::null_mut()) };
    }
    if caps_valid {
        unsafe { caps.write(0) };
    }
    if !out_valid || !caps_valid {
        return abi::INVALID;
    }
    status((|| {
        let path = std::str::from_utf8(unsafe { span(path) }?).map_err(|_| abi::INVALID)?;
        let device = maxwell::Device::open(path).map_err(handle_error)?;
        let c = device.capabilities();
        let flags = if c.supports_rendering() {
            abi::RENDERING
        } else {
            0
        } | if c.supports_presentation() {
            abi::PRESENTATION
        } else {
            0
        } | if c.supports_image_upload() {
            abi::IMAGE_UPLOAD
        } else {
            0
        } | if c.supports_image_readback() {
            abi::IMAGE_READBACK
        } else {
            0
        } | if c.supports_depth() { abi::DEPTH } else { 0 }
            | if c.supports_programmable_graphics() {
                abi::PROGRAMMABLE_GRAPHICS
                    | abi::READ_ONLY_STORAGE_BUFFERS
                    | abi::TYPED_TEXTURE_VIEWS
                    | abi::SRGB_TEXTURE_VIEWS
                    | abi::EXTENDED_VERTEX_FORMATS
                    | abi::RGBA8_COLOR_ATTACHMENT
                    | abi::IMAGE_BLITS
                    | abi::PUSH_CONSTANTS_128
                    | abi::COLOR_ATTACHMENTS_8
            } else {
                0
            }
            | if c.supports_texture_arrays() {
                abi::TEXTURE_ARRAYS
            } else {
                0
            }
            | if c.supports_depth_sampling() {
                abi::DEPTH_SAMPLING
            } else {
                0
            }
            | if c.supports_image_mips() {
                abi::IMAGE_MIPS
            } else {
                0
            };
        unsafe {
            caps.write(flags);
            out.write(Box::into_raw(Box::new(device)).cast());
        }
        Ok(())
    })())
}

unsafe extern "C" fn drop_device(p: Object) {
    if valid_pointer(p.cast::<maxwell::Device>()) {
        // SAFETY: The owning object was allocated by this library and is no longer used.
        unsafe { drop(Box::from_raw(p.cast::<maxwell::Device>())) };
    }
}

unsafe extern "C" fn create_context(p: Object, out: *mut Object) -> i32 {
    if !valid_pointer(out) {
        return abi::INVALID;
    }
    unsafe { out.write(core::ptr::null_mut()) };
    status((|| {
        let value = unsafe { object_mut::<maxwell::Device>(p) }?
            .create_context()
            .map_err(handle_error)?;
        unsafe { out.write(Box::into_raw(Box::new(value)).cast()) };
        Ok(())
    })())
}

unsafe extern "C" fn drop_context(p: Object) {
    if valid_pointer(p.cast::<maxwell::Context>()) {
        unsafe { drop(Box::from_raw(p.cast::<maxwell::Context>())) };
    }
}

unsafe extern "C" fn create_session(
    p: Object,
    metadata: Span<u64>,
    targets: Span<u32>,
    out: *mut Object,
    caps: *mut u64,
) -> i32 {
    let out_valid = valid_pointer(out);
    let caps_valid = valid_pointer(caps);
    if out_valid {
        unsafe { out.write(core::ptr::null_mut()) };
    }
    if caps_valid {
        unsafe { caps.write(0) };
    }
    if !out_valid || !caps_valid {
        return abi::INVALID;
    }
    status((|| {
        let context = unsafe { object_mut::<maxwell::Context>(p) }?;
        let table = Rc::new(ir::ResourceTable::new());
        let (source, revision) = table
            .sync_abi_snapshot(unsafe { span(metadata) }?)
            .map_err(ir_error)?;
        let targets = unsafe { span(targets) }?
            .iter()
            .map(|slot| table.abi_texture(*slot).map(|r| r.id()))
            .collect::<ir::Result<Vec<_>>>()
            .map_err(ir_error)?;
        let mut inner = context
            .create_mapped_target_session(table.clone(), &targets)
            .map_err(error)?;
        let flags = if inner.executor().supports_async_submission() {
            abi::ASYNC
        } else {
            0
        };
        let value = Session {
            inner,
            table,
            source,
            revision,
            poisoned: false,
        };
        unsafe {
            caps.write(flags);
            out.write(Box::into_raw(Box::new(value)).cast());
        }
        Ok(())
    })())
}

unsafe extern "C" fn drop_session(p: Object) {
    if valid_pointer(p.cast::<Session>()) {
        unsafe { drop(Box::from_raw(p.cast::<Session>())) };
    }
}

unsafe extern "C" fn sync_resources(p: Object, metadata: Span<u64>) -> i32 {
    status((|| {
        let s = unsafe { session(p) }?;
        let words = unsafe { span(metadata) }?;
        if words.len() < 3 || words[1] != s.source || words[2] < s.revision {
            return Err(abi::INVALID);
        }
        if words[2] == s.revision {
            return Ok(());
        }
        let (textures, buffers) = s.table.abi_retired_resources(words).map_err(ir_error)?;
        // Appending descriptors remains safe during pending work. Retirement
        // must stay nonblocking and never detach a resource owned by GPU work.
        if (!textures.is_empty() || !buffers.is_empty()) && !s.inner.is_idle().map_err(error)? {
            return Err(abi::BUSY);
        }
        let mut changed = false;
        for id in textures {
            match s.inner.release_texture(id) {
                Ok(()) => changed = true,
                Err(e) => {
                    // A first BUSY rejects before mutation and remains retryable.
                    // Other failures can leave the physical mirror partially retired.
                    let (poisoned, code) = retirement_failure(changed, error(e));
                    s.poisoned = poisoned;
                    return Err(code);
                }
            }
        }
        for id in buffers {
            match s.inner.release_buffer(id) {
                Ok(()) => changed = true,
                Err(e) => {
                    let (poisoned, code) = retirement_failure(changed, error(e));
                    s.poisoned = poisoned;
                    return Err(code);
                }
            }
        }
        match s.table.sync_abi_snapshot(words) {
            Ok((_, revision)) => s.revision = revision,
            Err(e) => {
                // ResourceTable documents that an error can follow a mutated prefix.
                s.poisoned = true;
                return Err(ir_error(e));
            }
        }
        Ok(())
    })())
}

unsafe extern "C" fn image(p: Object, slot: u32, out: *mut abi::ImageInfo) -> i32 {
    if !valid_pointer(out) {
        return abi::INVALID;
    }
    unsafe {
        out.write(abi::ImageInfo {
            handle: -1,
            ..Default::default()
        })
    };
    status((|| {
        let s = unsafe { session(p) }?;
        let id = s.table.abi_texture(slot).map_err(ir_error)?.id();
        let image = s.inner.image(id).map_err(error)?;
        let handle = image.shared_handle().duplicate().map_err(handle_error)?;
        let value = abi::ImageInfo {
            width: image.width(),
            height: image.height(),
            handle: handle.as_raw(),
            reserved: 0,
        };
        // Transfer the duplicate to the host, which closes it with its runtime.
        std::mem::forget(handle);
        unsafe { out.write(value) };
        Ok(())
    })())
}

unsafe extern "C" fn readback(
    p: Object,
    slot: u32,
    out: *mut u8,
    len: usize,
    stride: u32,
    rect: abi::Rect,
) -> i32 {
    status((|| {
        let s = unsafe { session(p) }?;
        validate_span(out, len)?;
        let destination = if len == 0 {
            &mut []
        } else {
            // SAFETY: The caller supplies exclusively writable output storage.
            unsafe { std::slice::from_raw_parts_mut(out, len) }
        };
        let id = s.table.abi_texture(slot).map_err(ir_error)?.id();
        let rect = ir::PixelRect::new(rect.x, rect.y, rect.width, rect.height).map_err(ir_error)?;
        s.inner
            .readback_bgra(id, destination, stride, rect)
            .map_err(error)
    })())
}

unsafe extern "C" fn import_bgra(p: Object, slot: u32, raw: i32) -> i32 {
    status((|| {
        // Ownership transfers on every return path, including poisoned sessions
        // and invalid slot/object arguments. from_raw also closes on query failure.
        let handle = unsafe { maxwell::Handle::from_raw(raw) }.map_err(handle_error)?;
        let s = unsafe { session(p) }?;
        let id = s.table.abi_texture(slot).map_err(ir_error)?.id();
        s.inner
            .import_shared_bgra_texture(id, handle)
            .map_err(error)
    })())
}

unsafe extern "C" fn release_import(p: Object, slot: u32) -> i32 {
    status((|| {
        let s = unsafe { session(p) }?;
        let id = s.table.abi_texture(slot).map_err(ir_error)?.id();
        s.inner.release_imported_texture(id).map_err(|failure| {
            eprintln!("scarlet-maxwell: imported texture retirement failed: {failure:?}");
            error(failure)
        })
    })())
}

unsafe extern "C" fn execute(p: Object, batch: *const abi::Batch) -> i32 {
    status((|| {
        let s = unsafe { session(p) }?;
        // Borrow a table clone so the executor may mutate the independent cache.
        let table = Rc::clone(&s.table);
        let batch = *unsafe { object_ref(batch) }?;
        validate_span(batch.words.data, batch.words.len)?;
        let commands = unsafe { ir::CommandBuffer::from_abi(&table, s.source, core_batch(batch)) }
            .map_err(ir_error)?;
        s.inner.executor().execute(&commands).map_err(error)
    })())
}

unsafe extern "C" fn submit(p: Object, batch: *const abi::Batch, out: *mut abi::SubmitResult) {
    if !valid_pointer(out) {
        return;
    }
    unsafe { out.write(abi::SubmitResult::default()) };
    let result = (|| {
        let s = unsafe { session(p) }?;
        let table = Rc::clone(&s.table);
        let batch = *unsafe { object_ref(batch) }?;
        validate_span(batch.words.data, batch.words.len)?;
        let commands = unsafe { ir::CommandBuffer::from_abi(&table, s.source, core_batch(batch)) }
            .map_err(ir_error)?;
        let (disposition, code, receipt) = match s.inner.executor().submit(&commands) {
            Ok(receipt) => (abi::ACCEPTED, abi::OK, receipt),
            Err(SubmitError::Busy) => return Err(abi::BUSY),
            Err(SubmitError::Rejected(e)) => return Err(error(e)),
            Err(SubmitError::Failed {
                error: e,
                completion,
            }) => (abi::PARTIAL, error(e), completion),
            Err(_) => return Err(abi::DEVICE_LOST),
        };
        Ok(abi::SubmitResult {
            disposition,
            error: code,
            // Arc ownership and its Completion implementation stay in this DSO.
            receipt: receipt.into_abi_object(),
        })
    })();
    unsafe {
        out.write(match result {
            Ok(value) => value,
            Err(error) => abi::SubmitResult {
                error,
                ..Default::default()
            },
        });
    }
}

unsafe extern "C" fn wait(p: Object, timeout: u64, out: *mut u32) -> i32 {
    if !valid_pointer(out) {
        return abi::INVALID;
    }
    unsafe { out.write(abi::PENDING) };
    status((|| {
        if !valid_pointer(p.cast::<maxwell::Submission>()) {
            return Err(abi::INVALID);
        }
        // This backend-owned Arc supports concurrent observation and cloning.
        let state = unsafe {
            maxwell::Submission::wait_abi_object(
                p,
                if timeout == u64::MAX {
                    None
                } else {
                    Some(Duration::from_nanos(timeout))
                },
            )
        }
        .map_err(error)?;
        unsafe {
            out.write(if state == CompletionStatus::Complete {
                abi::COMPLETE
            } else {
                abi::PENDING
            });
        }
        Ok(())
    })())
}

unsafe extern "C" fn drop_receipt(p: Object) {
    if valid_pointer(p.cast::<maxwell::Submission>()) {
        // The ABI guarantees all concurrent observers have returned before destruction.
        unsafe { maxwell::Submission::drop_abi_object(p) };
    }
}

/// Obtain this library's SGFX backend ABI v2 table.
///
/// # Safety
/// Non-null output storage must be writable for `size` bytes. `host` must be
/// readable and its CPU feature bits must describe the initialized host CPU.
#[cfg_attr(target_os = "scarlet", unsafe(no_mangle))]
pub unsafe extern "C" fn sgfx_backend_get_api_v2(
    version: u32,
    size: usize,
    host: *const abi::HostInfo,
    out: *mut abi::BackendApi,
) -> i32 {
    if let Err(code) = check_api(version, size, out) {
        return code;
    }
    let host = match unsafe { host_info(host) } {
        Ok(host) => host,
        Err(code) => return code,
    };
    // A cdylib has no executable std startup or private auxv. Match the host's
    // confirmed LSE support through compiler-builtins' atomic feature hook.
    #[cfg(all(target_arch = "aarch64", target_os = "scarlet"))]
    if host.cpu_features & abi::CPU_AARCH64_LSE != 0 {
        unsafe extern "C" {
            fn __rust_enable_lse();
        }
        unsafe { __rust_enable_lse() };
    }
    unsafe {
        out.write(abi::BackendApi {
            version: abi::VERSION,
            size: core::mem::size_of::<abi::BackendApi>() as u32,
            name: name(b"scarlet-maxwell"),
            gpu_backend: name(maxwell::BACKEND_ID),
            open,
            drop_device,
            create_context,
            drop_context,
            create_session,
            drop_session,
            sync_resources,
            image,
            readback,
            import_bgra,
            release_import,
            execute,
            submit,
            wait,
            drop_receipt,
        });
    }
    abi::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        mem::{MaybeUninit, size_of},
        ptr,
    };

    #[test]
    fn malformed_spans_reject_null_alignment_length_and_address_wrap() {
        assert_eq!(validate_span::<u64>(ptr::null(), 1), Err(abi::INVALID));
        assert_eq!(
            validate_span::<u64>(1usize as *const u64, 1),
            Err(abi::INVALID)
        );
        let value = 1u64;
        assert_eq!(validate_span(&value, usize::MAX), Err(abi::INVALID));
        let wrapped = (usize::MAX - 7) as *const u64;
        assert_eq!(validate_span(wrapped, 1), Err(abi::INVALID));
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
        assert_eq!(
            unsafe { span(Span::from_slice(&[1u64, 2])) }.unwrap(),
            &[1, 2]
        );
    }

    #[test]
    fn failures_keep_validation_capacity_support_and_device_loss_distinct() {
        use maxwell::IrSubmitError as E;
        use sgfx_codegen_maxwell::CompileError as C;
        assert_eq!(
            error(E::InvalidIr(ir::Error::InvalidDescriptor)),
            abi::INVALID
        );
        assert_eq!(
            error(E::InvalidIr(ir::Error::OutOfMemory)),
            abi::OUT_OF_MEMORY
        );
        assert_eq!(error(E::Codegen(C::UnsupportedFeature)), abi::UNSUPPORTED);
        assert_eq!(
            error(E::Codegen(C::CommandBudgetExceeded)),
            abi::OUT_OF_MEMORY
        );
        assert_eq!(error(E::Codegen(C::InvalidState)), abi::INVALID);
        assert_eq!(
            error(E::SubmitWire(maxwell_submit_wire::Error::InvalidSize)),
            abi::INVALID
        );
        assert_eq!(error(E::AsyncUnsupported), abi::UNSUPPORTED);
        assert_eq!(
            error(E::ShaderCompile(maxwell::ShaderCompileError(
                "unsupported shader".into()
            ))),
            abi::UNSUPPORTED
        );
        assert_eq!(error(E::SubmissionTooLarge), abi::OUT_OF_MEMORY);
        assert_eq!(error(E::ResourceBusy), abi::BUSY);
        assert_eq!(error(E::CompletionUnavailable), abi::DEVICE_LOST);
        assert_eq!(
            handle_error(maxwell::HandleError::NotFound),
            abi::INITIALIZATION_FAILED
        );
    }

    #[test]
    fn negotiation_rejects_without_touching_output_then_returns_named_table() {
        let host = abi::HostInfo {
            size: size_of::<abi::HostInfo>() as u32,
            reserved: 0,
            cpu_features: 0,
        };
        let mut out = MaybeUninit::<abi::BackendApi>::uninit();
        unsafe {
            ptr::write_bytes(
                out.as_mut_ptr().cast::<u8>(),
                0xa5,
                size_of::<abi::BackendApi>(),
            )
        };
        for (version, size, host) in [
            (1, size_of::<abi::BackendApi>(), &host as *const _),
            (
                abi::VERSION,
                size_of::<abi::BackendApi>() - 1,
                &host as *const _,
            ),
        ] {
            assert_eq!(
                unsafe { sgfx_backend_get_api_v2(version, size, host, out.as_mut_ptr()) },
                abi::ABI_MISMATCH
            );
            let bytes = unsafe {
                core::slice::from_raw_parts(out.as_ptr().cast::<u8>(), size_of::<abi::BackendApi>())
            };
            assert!(bytes.iter().all(|v| *v == 0xa5));
        }
        assert_eq!(
            unsafe {
                sgfx_backend_get_api_v2(
                    abi::VERSION,
                    size_of::<abi::BackendApi>(),
                    ptr::null(),
                    out.as_mut_ptr(),
                )
            },
            abi::INVALID
        );
        let invalid_host = abi::HostInfo {
            reserved: 1,
            ..host
        };
        assert_eq!(
            unsafe {
                sgfx_backend_get_api_v2(
                    abi::VERSION,
                    size_of::<abi::BackendApi>(),
                    &invalid_host,
                    out.as_mut_ptr(),
                )
            },
            abi::ABI_MISMATCH
        );
        assert_eq!(
            unsafe {
                sgfx_backend_get_api_v2(
                    abi::VERSION,
                    size_of::<abi::BackendApi>(),
                    &host,
                    out.as_mut_ptr(),
                )
            },
            abi::OK
        );
        let api = unsafe { out.assume_init() };
        assert_eq!(api.version, 2);
        assert_eq!(api.size as usize, size_of::<abi::BackendApi>());
        assert_eq!(
            &api.name[..b"scarlet-maxwell\0".len()],
            b"scarlet-maxwell\0"
        );
        assert_eq!(
            &api.gpu_backend[..b"nvidia-gm20b\0".len()],
            b"nvidia-gm20b\0"
        );
    }

    #[test]
    fn rejected_calls_initialize_owned_outputs_without_gpu_access() {
        let mut image_out = abi::ImageInfo {
            width: 1,
            height: 1,
            handle: 123,
            reserved: 1,
        };
        assert_eq!(
            unsafe { image(ptr::null_mut(), 0, &mut image_out) },
            abi::INVALID
        );
        assert_eq!(
            (
                image_out.width,
                image_out.height,
                image_out.handle,
                image_out.reserved
            ),
            (0, 0, -1, 0)
        );
        let mut receipt = abi::SubmitResult {
            disposition: abi::ACCEPTED,
            error: abi::OK,
            receipt: ptr::dangling_mut(),
        };
        unsafe { submit(ptr::null_mut(), ptr::null(), &mut receipt) };
        assert_eq!(receipt.disposition, abi::REJECTED);
        assert_eq!(receipt.error, abi::INVALID);
        assert!(receipt.receipt.is_null());
        let mut completion = abi::COMPLETE;
        assert_eq!(
            unsafe { wait(ptr::null_mut(), 0, &mut completion) },
            abi::INVALID
        );
        assert_eq!(completion, abi::PENDING);
        let mut device = ptr::dangling_mut();
        let mut caps = u64::MAX;
        assert_eq!(
            unsafe {
                open(
                    Span {
                        data: ptr::null(),
                        len: 1,
                    },
                    &mut device,
                    &mut caps,
                )
            },
            abi::INVALID
        );
        assert!(device.is_null());
        assert_eq!(caps, 0);
    }
}

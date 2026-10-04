//! Optional low-level resource/queue extension; objects retain their owning DSO.
use super::*;
use std::sync::Arc;

struct Resources {
    inner: maxwell::IrResources,
    context: maxwell::Context,
    table: Rc<ir::ResourceTable>,
    source: u64,
    revision: u64,
    poisoned: bool,
}

struct Queue {
    inner: maxwell::Queue,
    context: maxwell::Context,
}

unsafe fn resources<'a>(p: Object) -> Result<&'a mut Resources, i32> {
    let r = unsafe { object_mut::<Resources>(p) }?;
    if r.poisoned {
        Err(abi::DEVICE_LOST)
    } else {
        Ok(r)
    }
}

unsafe extern "C" fn create_resources(p: Object, metadata: Span<u64>, out: *mut Object) -> i32 {
    if !valid_pointer(out) {
        return abi::INVALID;
    }
    unsafe { out.write(core::ptr::null_mut()) };
    status((|| {
        let context = unsafe { object_mut::<maxwell::Context>(p) }?;
        let table = Rc::new(ir::ResourceTable::new());
        let (source, revision) = table
            .sync_abi_snapshot(unsafe { span(metadata) }?)
            .map_err(ir_error)?;
        let inner = context
            .create_ir_resources(Rc::clone(&table))
            .map_err(error)?;
        let value = Resources {
            inner,
            context: context.clone(),
            table,
            source,
            revision,
            poisoned: false,
        };
        unsafe { out.write(Box::into_raw(Box::new(value)).cast()) };
        Ok(())
    })())
}

unsafe extern "C" fn drop_resources(p: Object) {
    if valid_pointer(p.cast::<Resources>()) {
        unsafe { drop(Box::from_raw(p.cast::<Resources>())) };
    }
}

unsafe extern "C" fn sync_resources(p: Object, metadata: Span<u64>) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let words = unsafe { span(metadata) }?;
        if words.len() < 3 || words[1] != r.source || words[2] < r.revision {
            return Err(abi::INVALID);
        }
        if words[2] == r.revision {
            return Ok(());
        }
        let (textures, buffers) = r.table.abi_retired_resources(words).map_err(ir_error)?;
        if (!textures.is_empty() || !buffers.is_empty()) && !r.context.is_idle().map_err(error)? {
            return Err(abi::BUSY);
        }
        let mut changed = false;
        for id in textures {
            match r.inner.release_texture(id) {
                Ok(()) => changed = true,
                Err(e) => {
                    let (poisoned, code) = retirement_failure(changed, error(e));
                    r.poisoned = poisoned;
                    return Err(code);
                }
            }
        }
        for id in buffers {
            match r.inner.release_buffer(id) {
                Ok(()) => changed = true,
                Err(e) => {
                    let (poisoned, code) = retirement_failure(changed, error(e));
                    r.poisoned = poisoned;
                    return Err(code);
                }
            }
        }
        match r.table.sync_abi_snapshot(words) {
            Ok((_, revision)) => r.revision = revision,
            Err(e) => {
                r.poisoned = true;
                return Err(ir_error(e));
            }
        }
        Ok(())
    })())
}

unsafe extern "C" fn release_buffer(p: Object, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_buffer(slot).map_err(ir_error)?.id();
        if !r.context.is_idle().map_err(error)? {
            return Err(abi::BUSY);
        }
        r.inner.release_buffer(id).map_err(error)
    })())
}

unsafe extern "C" fn release_texture(p: Object, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_texture(slot).map_err(ir_error)?.id();
        if !r.context.is_idle().map_err(error)? {
            return Err(abi::BUSY);
        }
        r.inner.release_texture(id).map_err(error)
    })())
}

unsafe extern "C" fn release_bind_group(p: Object, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_bind_group(slot).map_err(ir_error)?.id();
        r.inner.release_bind_group(id).map_err(error)
    })())
}

unsafe extern "C" fn validate(p: Object, kind: u32, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        match kind {
            abi::VALIDATE_SHADER => r
                .inner
                .validate_shader_module(r.table.abi_shader_module(slot).map_err(ir_error)?.id()),
            abi::VALIDATE_RENDER_PIPELINE => r.inner.validate_programmable_render_pipeline(
                r.table
                    .abi_programmable_render_pipeline(slot)
                    .map_err(ir_error)?
                    .id(),
            ),
            abi::VALIDATE_COMPUTE_PIPELINE => r.inner.validate_compute_pipeline(
                r.table.abi_compute_pipeline(slot).map_err(ir_error)?.id(),
            ),
            _ => return Err(abi::INVALID),
        }
        .map_err(error)
    })())
}

unsafe fn output<'a>(p: *mut u8, len: usize) -> Result<&'a mut [u8], i32> {
    validate_span(p, len)?;
    if len == 0 {
        return Ok(&mut []);
    }
    // SAFETY: The ABI caller provides exclusive initialized output storage.
    Ok(unsafe { core::slice::from_raw_parts_mut(p, len) })
}

unsafe extern "C" fn read_buffer(
    p: Object,
    slot: u32,
    offset: u64,
    out: *mut u8,
    len: usize,
) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_buffer(slot).map_err(ir_error)?.id();
        r.inner
            .read_buffer_into(id, offset, unsafe { output(out, len) }?)
            .map_err(error)
    })())
}

unsafe extern "C" fn create_image(
    p: Object,
    width: u32,
    height: u32,
    out: *mut Object,
    info: *mut abi::ImageInfo,
) -> i32 {
    let out_valid = valid_pointer(out);
    let info_valid = valid_pointer(info);
    if out_valid {
        unsafe { out.write(core::ptr::null_mut()) };
    }
    if info_valid {
        unsafe {
            info.write(abi::ImageInfo {
                handle: -1,
                ..Default::default()
            })
        };
    }
    if !out_valid || !info_valid {
        return abi::INVALID;
    }
    status((|| {
        let context = unsafe { object_mut::<maxwell::Context>(p) }?;
        let image = context
            .create_shared_image(width, height)
            .map_err(handle_error)?;
        let handle = image.shared_handle().duplicate().map_err(handle_error)?;
        let value = abi::ImageInfo {
            width: image.width(),
            height: image.height(),
            handle: handle.as_raw(),
            reserved: 0,
        };
        // The image owner and imported mapping share a driver-private Arc.
        let owner = Box::into_raw(Box::new(Arc::new(image))).cast();
        std::mem::forget(handle);
        unsafe {
            info.write(value);
            out.write(owner);
        }
        Ok(())
    })())
}

unsafe extern "C" fn drop_image(p: Object) {
    if valid_pointer(p.cast::<Arc<maxwell::Image>>()) {
        unsafe { drop(Box::from_raw(p.cast::<Arc<maxwell::Image>>())) };
    }
}

unsafe extern "C" fn map_image(p: Object, slot: u32, image: Object) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let image = unsafe { object_ref(image.cast::<Arc<maxwell::Image>>()) }?;
        let id = r.table.abi_texture(slot).map_err(ir_error)?.id();
        r.inner.map_image(id, Arc::clone(image)).map_err(error)
    })())
}

unsafe extern "C" fn unmap_image(p: Object, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_texture(slot).map_err(ir_error)?.id();
        r.inner.unmap_image(id).map_err(error)
    })())
}

unsafe extern "C" fn create_queue(p: Object, out: *mut Object) -> i32 {
    if !valid_pointer(out) {
        return abi::INVALID;
    }
    unsafe { out.write(core::ptr::null_mut()) };
    status((|| {
        let context = unsafe { object_mut::<maxwell::Context>(p) }?;
        let inner = context.create_queue().map_err(handle_error)?;
        let value = Queue {
            inner,
            context: context.clone(),
        };
        unsafe { out.write(Box::into_raw(Box::new(value)).cast()) };
        Ok(())
    })())
}

unsafe extern "C" fn drop_queue(p: Object) {
    if valid_pointer(p.cast::<Queue>()) {
        unsafe { drop(Box::from_raw(p.cast::<Queue>())) };
    }
}

unsafe extern "C" fn submit(
    p: Object,
    owner: Object,
    batch: *const abi::Batch,
    out: *mut abi::SubmitResult,
) {
    if !valid_pointer(out) {
        return;
    }
    unsafe { out.write(abi::SubmitResult::default()) };
    let result = (|| {
        let r = unsafe { resources(owner) }?;
        let q = unsafe { object_mut::<Queue>(p) }?;
        let batch = *unsafe { object_ref(batch) }?;
        validate_span(batch.words.data, batch.words.len)?;
        let commands =
            unsafe { ir::CommandBuffer::from_abi(&r.table, r.source, batch) }.map_err(ir_error)?;
        let (disposition, code, receipt) =
            match q.inner.submit_ir_async(&q.context, &mut r.inner, &commands) {
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

unsafe extern "C" fn read_texture(
    p: Object,
    owner: Object,
    slot: u32,
    out: *mut u8,
    len: usize,
) -> i32 {
    status((|| {
        let r = unsafe { resources(owner) }?;
        let context = unsafe { object_mut::<maxwell::Context>(p) }?;
        let id = r.table.abi_texture(slot).map_err(ir_error)?.id();
        context
            .read_texture_into(&mut r.inner, id, unsafe { output(out, len) }?)
            .map_err(error)
    })())
}

unsafe extern "C" fn clone_receipt(p: Object) {
    if valid_pointer(p.cast::<maxwell::Submission>()) {
        unsafe { maxwell::Submission::clone_abi_object(p) };
    }
}

/// Negotiate the optional low-level ABI v2 table after the main entry point.
///
/// # Safety
/// `out` must be writable for `size` bytes. All subsequently used opaque objects
/// must originate from this DSO and obey the table's ownership contracts.
#[cfg_attr(target_os = "scarlet", unsafe(no_mangle))]
pub unsafe extern "C" fn sgfx_backend_get_driver_api_v2(
    version: u32,
    size: usize,
    out: *mut abi::DriverApi,
) -> i32 {
    if let Err(code) = check_api(version, size, out) {
        return code;
    }
    unsafe {
        out.write(abi::DriverApi {
            version: abi::VERSION,
            size: core::mem::size_of::<abi::DriverApi>() as u32,
            create_resources,
            drop_resources,
            sync_resources,
            release_buffer,
            validate,
            read_buffer,
            create_image,
            drop_image,
            map_image,
            unmap_image,
            create_queue,
            drop_queue,
            submit,
            read_texture,
            clone_receipt,
            release_texture,
            release_bind_group,
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
    fn driver_negotiation_and_empty_outputs_do_not_access_gpu() {
        let mut out = MaybeUninit::<abi::DriverApi>::uninit();
        assert_eq!(
            unsafe {
                sgfx_backend_get_driver_api_v2(1, size_of::<abi::DriverApi>(), out.as_mut_ptr())
            },
            abi::ABI_MISMATCH
        );
        assert_eq!(
            unsafe {
                sgfx_backend_get_driver_api_v2(
                    abi::VERSION,
                    size_of::<abi::DriverApi>() - 1,
                    out.as_mut_ptr(),
                )
            },
            abi::ABI_MISMATCH
        );
        assert_eq!(
            unsafe {
                sgfx_backend_get_driver_api_v2(
                    abi::VERSION,
                    size_of::<abi::DriverApi>(),
                    ptr::null_mut(),
                )
            },
            abi::INVALID
        );
        assert_eq!(
            unsafe {
                sgfx_backend_get_driver_api_v2(
                    abi::VERSION,
                    size_of::<abi::DriverApi>(),
                    out.as_mut_ptr(),
                )
            },
            abi::OK
        );
        let api = unsafe { out.assume_init() };
        assert_eq!(api.version, abi::VERSION);
        assert_eq!(api.size as usize, size_of::<abi::DriverApi>());
        assert!(unsafe { output(ptr::null_mut(), 0) }.unwrap().is_empty());
        assert_eq!(
            unsafe { output(ptr::null_mut(), 1) }.unwrap_err(),
            abi::INVALID
        );
        let mut image = ptr::dangling_mut();
        let mut info = abi::ImageInfo {
            width: 1,
            height: 1,
            handle: 1,
            reserved: 1,
        };
        assert_eq!(
            unsafe { (api.create_image)(ptr::null_mut(), 1, 1, &mut image, &mut info) },
            abi::INVALID
        );
        assert!(image.is_null());
        assert_eq!(
            (info.width, info.height, info.handle, info.reserved),
            (0, 0, -1, 0)
        );
    }
}

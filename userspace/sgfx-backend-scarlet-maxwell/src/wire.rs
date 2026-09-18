//! Translation from pure codegen artifacts to the kernel submit wire.

use alloc::{vec, vec::Vec};

use maxwell_submit_wire as submit_wire;
use sgfx_codegen_maxwell::{Access, ObjectRef, RelocatableCommands};

use crate::{IrSubmitError, UnsupportedIrFeature};

#[derive(Clone, Copy)]
pub(crate) struct BoundObject {
    pub(crate) object: ObjectRef,
    pub(crate) attachment_token: u64,
    pub(crate) allocation_offset: u64,
    pub(crate) size: u64,
}

pub(crate) fn encode(
    compiled: &RelocatableCommands,
    bindings: &[BoundObject],
) -> Result<Vec<u8>, IrSubmitError> {
    let mut resources = Vec::new();
    resources
        .try_reserve_exact(compiled.accesses.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;

    for access in &compiled.accesses {
        let bound = binding(bindings, access.object)?;
        let access_end =
            access
                .offset
                .checked_add(access.size)
                .ok_or(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ResourceState,
                ))?;
        if access.size == 0 || access_end > bound.size {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ));
        }
        resources.push(submit_wire::Resource {
            attachment_token: bound.attachment_token,
            range_offset: bound.allocation_offset.checked_add(access.offset).ok_or(
                IrSubmitError::Unsupported(UnsupportedIrFeature::ResourceState),
            )?,
            range_size: access.size,
            access: wire_access(access.access)?,
        });
    }

    let mut relocations = Vec::new();
    relocations
        .try_reserve_exact(compiled.fixups.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for fixup in &compiled.fixups {
        if let ObjectRef::CanonicalShader(variant) = fixup.object {
            relocations.push(submit_wire::Relocation {
                commands_word_offset: fixup.word_offset,
                source: submit_wire::RelocationSource::CanonicalShader(variant),
                resource_offset: fixup.object_offset,
                required_size: fixup.required_size,
                access: wire_access(fixup.access)?,
                encoding: submit_wire::AddressEncoding::GpuVa64,
            });
            continue;
        }
        let bound = binding(bindings, fixup.object)?;
        let required_end = fixup.object_offset.checked_add(fixup.required_size).ok_or(
            IrSubmitError::Unsupported(UnsupportedIrFeature::ResourceState),
        )?;
        if fixup.required_size == 0 || required_end > bound.size {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ));
        }
        let fixup_access = wire_access(fixup.access)?;
        let resource_index = compiled
            .accesses
            .iter()
            .enumerate()
            .find(|(_, access)| {
                if access.object != fixup.object || !access.access.contains(fixup.access) {
                    return false;
                }
                let Some(end) = access.offset.checked_add(access.size) else {
                    return false;
                };
                access.offset <= fixup.object_offset && required_end <= end
            })
            .map(|(index, _)| index)
            .ok_or(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ))?;
        let resource = resources
            .get(resource_index)
            .ok_or(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ))?;
        let absolute_offset = bound
            .allocation_offset
            .checked_add(fixup.object_offset)
            .ok_or(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ))?;
        let resource_offset = absolute_offset.checked_sub(resource.range_offset).ok_or(
            IrSubmitError::Unsupported(UnsupportedIrFeature::ResourceState),
        )?;
        relocations.push(submit_wire::Relocation {
            commands_word_offset: fixup.word_offset,
            source: submit_wire::RelocationSource::Attachment(
                u32::try_from(resource_index)
                    .map_err(|_| IrSubmitError::Unsupported(UnsupportedIrFeature::ResourceState))?,
            ),
            resource_offset,
            required_size: fixup.required_size,
            access: fixup_access,
            encoding: submit_wire::AddressEncoding::GpuVa64,
        });
    }

    let submit = submit_wire::Submit {
        commands: &compiled.words,
        resources: &resources,
        relocations: &relocations,
    };
    let encoded_len = submit_wire::encoded_len(submit)?;
    let mut output = vec![0; encoded_len];
    submit_wire::encode(submit, &mut output)?;
    Ok(output)
}

fn binding(bindings: &[BoundObject], object: ObjectRef) -> Result<BoundObject, IrSubmitError> {
    bindings
        .iter()
        .copied()
        .find(|binding| binding.object == object)
        .ok_or(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ResourceState,
        ))
}

fn wire_access(access: Access) -> Result<u32, IrSubmitError> {
    let mut result = 0;
    if access.contains(Access::READ) {
        result |= submit_wire::ACCESS_READ;
    }
    if access.contains(Access::WRITE) {
        result |= submit_wire::ACCESS_WRITE;
    }
    if result == 0 {
        Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ResourceState,
        ))
    } else {
        Ok(result)
    }
}

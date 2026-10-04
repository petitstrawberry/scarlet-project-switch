// SPDX-License-Identifier: GPL-2.0-only
//! Lower address-free image transfers only after resolving attachment authority.

use maxwell_image_layout::{
    Descriptor, Layout, LayoutKind, Subresource,
    wire::{Command, Filter, Opcode},
};

#[cfg(target_os = "none")]
use scarlet::device::gpu::{
    GPU_IMAGE_FORMAT_BGRA8_UNORM, GPU_IMAGE_MODIFIER_LINEAR, GPU_IMAGE_USAGE_TRANSFER_DST,
    GPU_IMAGE_USAGE_TRANSFER_SRC, GpuBackendImageLayout, GpuBackendImagePlaneLayout,
    GpuImageCreateInfo,
};
#[cfg(all(test, not(target_os = "none")))]
use test_abi::*;

const VA_LIMIT: u64 = 1 << 48;

/// The descriptor and layout come from the attached kernel image, while the
/// range is the byte authority explicitly declared in this submission.
#[derive(Clone, Copy, Debug)]
pub(super) struct ImageAuthority {
    pub create: GpuImageCreateInfo,
    pub layout: GpuBackendImageLayout,
    pub va: u64,
    pub range_offset: u64,
    pub range_size: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BufferAuthority {
    pub va: u64,
    pub range_offset: u64,
    pub range_size: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum SourceAuthority {
    Buffer(BufferAuthority),
    Image(ImageAuthority),
}

/// Resolve the exact token and required rights in the untrusted submission
/// resource table. The caller then resolves that token in its own context and
/// checks this range against the retained backing allocation.
pub(super) fn declared_range(
    entries: impl IntoIterator<Item = (u64, u32, u64, u64)>,
    token: u64,
    required_access: u32,
) -> Result<(u64, u64), &'static str> {
    if token == 0 || required_access == 0 || required_access & !3 != 0 {
        return Err("image command authority request invalid");
    }
    let (_, _, offset, size) = entries
        .into_iter()
        .find(|&(entry_token, access, _, _)| {
            entry_token == token && access & required_access == required_access
        })
        .ok_or("image command attachment access not declared")?;
    if size == 0 || offset.checked_add(size).is_none() {
        return Err("image command authority range invalid");
    }
    Ok((offset, size))
}

/// Produce a trusted physical BGRA transfer for the GPU executor.
///
/// The caller resolves both attachment tokens and checks the resource table's
/// read/write permissions before calling this function. Logical format
/// expansion is not conveyed by this wire format: userspace must ensure that
/// both images have compatible logical semantics before encoding a blit.
pub(super) fn lower(
    command: Command,
    destination: ImageAuthority,
    source: SourceAuthority,
) -> Result<[u32; 96], &'static str> {
    command
        .validate()
        .map_err(|_| "invalid image transfer metadata")?;
    let destination_layout = image_layout(destination, GPU_IMAGE_USAGE_TRANSFER_DST)?;
    let dst = destination_layout
        .subresource(command.destination_mip, command.destination_layer)
        .map_err(|_| "image transfer destination subresource out of range")?;
    rectangle(command.destination_rect, dst.level.width, dst.level.height)?;
    let dst_va = image_address(destination, dst)?;
    let (src_va, src_width, src_height, src_pitch, src_mode, src_size) =
        match (command.opcode, source) {
            (Opcode::Upload, SourceAuthority::Buffer(buffer)) => {
                if command.source_stride & 31 != 0 {
                    return Err("image upload pitch must be aligned to 32 bytes");
                }
                let span = command
                    .upload_span()
                    .map_err(|_| "invalid image upload span")?;
                let address = authorized_address(
                    buffer.va,
                    buffer.range_offset,
                    buffer.range_size,
                    command.source_offset,
                    span,
                )?;
                (
                    address,
                    command.source_rect[2],
                    command.source_rect[3],
                    command.source_stride,
                    0,
                    span,
                )
            }
            (Opcode::Blit, SourceAuthority::Image(image)) => {
                let planned = image_layout(image, GPU_IMAGE_USAGE_TRANSFER_SRC)?;
                let sub = planned
                    .subresource(command.source_mip, command.source_layer)
                    .map_err(|_| "image transfer source subresource out of range")?;
                rectangle(command.source_rect, sub.level.width, sub.level.height)?;
                let address = image_address(image, sub)?;
                (
                    address,
                    sub.level.width,
                    sub.level.height,
                    sub.level.row_pitch,
                    tile_mode(planned.kind, sub),
                    sub.level.size,
                )
            }
            _ => return Err("image transfer source object type mismatch"),
        };
    // Reading and writing overlapping backing ranges has no execution order
    // guarantee. Distinct mip levels or layers of one image remain usable.
    if overlaps(dst_va, dst.level.size, src_va, src_size)? {
        return Err("image transfer source and destination alias");
    }

    let mut words = [0; 96];
    words[0] = 6;
    words[2] = dst_va as u32;
    words[3] = (dst_va >> 32) as u32;
    words[4] = src_va as u32;
    words[5] = (src_va >> 32) as u32;
    words[10] = dst.level.width;
    words[11] = dst.level.height;
    words[12] = dst.level.row_pitch;
    words[13..17].copy_from_slice(&command.destination_rect);
    words[17..21].copy_from_slice(&command.source_rect);
    words[21] = match command.filter {
        Filter::Nearest => 0,
        Filter::Linear => 1,
    } | (u32::from(command.flip_x) << 2)
        | (u32::from(command.flip_y) << 3);
    words[29] = src_width;
    words[30] = src_height;
    words[31] = src_pitch;
    words[52] = tile_mode(destination_layout.kind, dst);
    words[53] = src_mode;
    Ok(words)
}

fn tile_mode(kind: LayoutKind, sub: Subresource) -> u32 {
    match kind {
        LayoutKind::Linear => 0,
        LayoutKind::BlockLinear { .. } => 0x100 | (u32::from(sub.level.tile_y_log2) << 4),
    }
}

fn image_layout(image: ImageAuthority, usage: u32) -> Result<Layout, &'static str> {
    let create = image.create;
    let layout = image.layout;
    if create.format != GPU_IMAGE_FORMAT_BGRA8_UNORM || create.usage & usage != usage {
        return Err("image transfer requires BGRA image and transfer usage");
    }
    if create.cube && (create.width != create.height || create.array_layers % 6 != 0) {
        return Err("image transfer cube descriptor invalid");
    }
    let kind = if layout.modifier == GPU_IMAGE_MODIFIER_LINEAR {
        LayoutKind::Linear
    } else {
        if layout.modifier & !0xf != maxwell_image_layout::NVIDIA_COLOR_MODIFIER_BASE {
            return Err("image transfer requires a color image modifier");
        }
        let y = maxwell_image_layout::modifier_tile_y(layout.modifier)
            .ok_or("unsupported image transfer modifier")?;
        LayoutKind::BlockLinear {
            base_y_log2: y,
            clamp_mips: create.mip_levels > 1 || create.array_layers > 1 || create.cube,
        }
    };
    let planned = maxwell_image_layout::plan(
        Descriptor {
            width: create.width,
            height: create.height,
            mip_levels: create.mip_levels,
            array_layers: create.array_layers,
            bytes_per_pixel: 4,
        },
        kind,
    )
    .map_err(|_| "image transfer descriptor invalid")?;
    let plane = layout.planes[0];
    if layout.plane_count != 1
        || layout.alignment != 4096
        || layout.total_size != planned.total_size
        || plane.offset != 0
        || plane.size != planned.total_size
        || plane.row_pitch != planned.levels[0].row_pitch
        || plane.array_pitch != planned.array_pitch
        || plane.block_width != 1
        || plane.block_height != 1
        || plane.bytes_per_block != 4
        || layout.planes[1..]
            .iter()
            .any(|p| *p != GpuBackendImagePlaneLayout::EMPTY)
    {
        return Err("image transfer descriptor/layout mismatch");
    }
    Ok(planned)
}

fn rectangle(rect: [u32; 4], width: u32, height: u32) -> Result<(), &'static str> {
    if rect[2] == 0
        || rect[3] == 0
        || rect[0].checked_add(rect[2]).is_none_or(|end| end > width)
        || rect[1].checked_add(rect[3]).is_none_or(|end| end > height)
    {
        return Err("image transfer rectangle out of range");
    }
    Ok(())
}

fn image_address(image: ImageAuthority, sub: Subresource) -> Result<u64, &'static str> {
    authorized_address(
        image.va,
        image.range_offset,
        image.range_size,
        sub.offset,
        sub.level.size,
    )
}

fn authorized_address(
    va: u64,
    range_offset: u64,
    range_size: u64,
    offset: u64,
    size: u64,
) -> Result<u64, &'static str> {
    let range_end = range_offset
        .checked_add(range_size)
        .ok_or("image transfer declared range overflow")?;
    let end = offset
        .checked_add(size)
        .ok_or("image transfer byte range overflow")?;
    if size == 0 || range_size == 0 || offset < range_offset || end > range_end {
        return Err("image transfer exceeds declared resource range");
    }
    let address = va
        .checked_add(offset)
        .ok_or("image transfer address overflow")?;
    let address_end = va
        .checked_add(end)
        .ok_or("image transfer address overflow")?;
    if address >= VA_LIMIT || address_end > VA_LIMIT {
        return Err("image transfer address exceeds GPU VA width");
    }
    Ok(address)
}

fn overlaps(a: u64, a_size: u64, b: u64, b_size: u64) -> Result<bool, &'static str> {
    let a_end = a
        .checked_add(a_size)
        .ok_or("image transfer alias range overflow")?;
    let b_end = b
        .checked_add(b_size)
        .ok_or("image transfer alias range overflow")?;
    Ok(a < b_end && b < a_end)
}

// The kernel dependency is bare-metal only. Mirror its POD metadata for host
// tests so the same authority and arithmetic code is exercised without MMIO.
#[cfg(all(test, not(target_os = "none")))]
mod test_abi {
    pub const GPU_IMAGE_FORMAT_BGRA8_UNORM: u32 = 1;
    pub const GPU_IMAGE_MODIFIER_LINEAR: u64 = 0;
    pub const GPU_IMAGE_USAGE_TRANSFER_DST: u32 = 1 << 3;
    pub const GPU_IMAGE_USAGE_TRANSFER_SRC: u32 = 1 << 5;
    #[derive(Clone, Copy, Debug)]
    pub struct GpuImageCreateInfo {
        pub format: u32,
        pub usage: u32,
        pub width: u32,
        pub height: u32,
        pub mip_levels: u32,
        pub array_layers: u32,
        pub cube: bool,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct GpuBackendImagePlaneLayout {
        pub offset: u64,
        pub size: u64,
        pub row_pitch: u32,
        pub array_pitch: u32,
        pub block_width: u16,
        pub block_height: u16,
        pub bytes_per_block: u16,
    }
    impl GpuBackendImagePlaneLayout {
        pub const EMPTY: Self = Self {
            offset: 0,
            size: 0,
            row_pitch: 0,
            array_pitch: 0,
            block_width: 0,
            block_height: 0,
            bytes_per_block: 0,
        };
    }
    #[derive(Clone, Copy, Debug)]
    pub struct GpuBackendImageLayout {
        pub modifier: u64,
        pub total_size: u64,
        pub alignment: u64,
        pub plane_count: u32,
        pub planes: [GpuBackendImagePlaneLayout; 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(
        width: u32,
        height: u32,
        mips: u32,
        layers: u32,
        kind: LayoutKind,
        va: u64,
    ) -> ImageAuthority {
        let create = GpuImageCreateInfo {
            format: GPU_IMAGE_FORMAT_BGRA8_UNORM,
            usage: GPU_IMAGE_USAGE_TRANSFER_SRC | GPU_IMAGE_USAGE_TRANSFER_DST,
            width,
            height,
            mip_levels: mips,
            array_layers: layers,
            cube: false,
        };
        let p = maxwell_image_layout::plan(
            Descriptor {
                width,
                height,
                mip_levels: mips,
                array_layers: layers,
                bytes_per_pixel: 4,
            },
            kind,
        )
        .unwrap();
        let mut planes = [GpuBackendImagePlaneLayout::EMPTY; 4];
        planes[0] = GpuBackendImagePlaneLayout {
            offset: 0,
            size: p.total_size,
            row_pitch: p.levels[0].row_pitch,
            array_pitch: p.array_pitch,
            block_width: 1,
            block_height: 1,
            bytes_per_block: 4,
        };
        ImageAuthority {
            create,
            layout: GpuBackendImageLayout {
                modifier: match kind {
                    LayoutKind::Linear => 0,
                    LayoutKind::BlockLinear { base_y_log2, .. } => {
                        maxwell_image_layout::color_modifier(base_y_log2).unwrap()
                    }
                },
                total_size: p.total_size,
                alignment: 4096,
                plane_count: 1,
                planes,
            },
            va,
            range_offset: 0,
            range_size: p.total_size,
        }
    }
    fn tiled() -> LayoutKind {
        LayoutKind::BlockLinear {
            base_y_log2: 4,
            clamp_mips: true,
        }
    }
    fn upload() -> Command {
        Command {
            opcode: Opcode::Upload,
            source_attachment: 1,
            destination_attachment: 2,
            source_offset: 64,
            source_stride: 32,
            source_mip: 0,
            source_layer: 0,
            destination_mip: 0,
            destination_layer: 0,
            source_rect: [0, 0, 4, 3],
            destination_rect: [1, 2, 4, 3],
            filter: Filter::Nearest,
            flip_x: false,
            flip_y: false,
        }
    }
    fn buffer() -> SourceAuthority {
        SourceAuthority::Buffer(BufferAuthority {
            va: 0x100000,
            range_offset: 64,
            range_size: 80,
        })
    }
    fn blit() -> Command {
        Command {
            opcode: Opcode::Blit,
            source_offset: 0,
            source_stride: 0,
            source_mip: 2,
            source_layer: 1,
            destination_mip: 1,
            destination_layer: 2,
            source_rect: [1, 2, 8, 4],
            destination_rect: [3, 4, 16, 8],
            filter: Filter::Linear,
            flip_x: true,
            flip_y: true,
            ..upload()
        }
    }

    #[test]
    fn transport_authority_requires_the_context_token_and_exact_read_write_rights() {
        let entries = [(11, 1, 0, 128), (22, 2, 64, 256), (33, 3, 0, 512)];
        assert_eq!(declared_range(entries, 11, 1).unwrap(), (0, 128));
        assert_eq!(declared_range(entries, 22, 2).unwrap(), (64, 256));
        assert_eq!(declared_range(entries, 33, 3).unwrap(), (0, 512));
        assert!(declared_range(entries, 11, 2).is_err());
        assert!(declared_range(entries, 22, 1).is_err());
        assert!(declared_range(entries, 11, 3).is_err());
        assert!(declared_range(entries, 44, 1).is_err());
        assert!(declared_range(entries, 0, 1).is_err());
        assert!(declared_range([(11, 1, u64::MAX, 1)], 11, 1).is_err());
        assert!(declared_range([(11, 1, 0, 0)], 11, 1).is_err());
    }

    #[test]
    fn upload_uses_padded_destination_and_exact_source_span() {
        let dst = image(17, 7, 1, 1, LayoutKind::Linear, 0x200000);
        let words = lower(upload(), dst, buffer()).unwrap();
        assert_eq!(&words[2..6], &[0x200000, 0, 0x100040, 0]);
        assert_eq!(&words[10..13], &[17, 7, 256]);
        assert_eq!(&words[29..32], &[4, 3, 32]);
        assert_eq!((words[0], words[52], words[53]), (6, 0, 0));
        let short = SourceAuthority::Buffer(BufferAuthority {
            va: 0x100000,
            range_offset: 64,
            range_size: 79,
        });
        assert!(lower(upload(), dst, short).is_err());
        let shifted = SourceAuthority::Buffer(BufferAuthority {
            va: 0x100000,
            range_offset: 65,
            range_size: 80,
        });
        assert!(lower(upload(), dst, shifted).is_err());
        let mut bad_pitch = upload();
        bad_pitch.source_stride = 24;
        assert!(lower(bad_pitch, dst, buffer()).is_err());
    }

    #[test]
    fn distinct_mips_and_layers_use_their_own_pitch_extent_and_tile_height() {
        let img = image(129, 65, 8, 3, tiled(), 0x200000);
        let planned = image_layout(img, GPU_IMAGE_USAGE_TRANSFER_DST).unwrap();
        let src = planned.subresource(2, 1).unwrap();
        let dst = planned.subresource(1, 2).unwrap();
        let words = lower(blit(), img, SourceAuthority::Image(img)).unwrap();
        assert_eq!(words[2] as u64, img.va + dst.offset);
        assert_eq!(words[4] as u64, img.va + src.offset);
        assert_eq!(&words[10..13], &[64, 32, 256]);
        assert_eq!(&words[29..32], &[32, 16, 128]);
        assert_eq!((words[21], words[52], words[53]), (13, 0x120, 0x110));
    }

    #[test]
    fn image_authority_must_enclose_the_full_selected_subresource() {
        let src = image(129, 65, 8, 3, tiled(), 0x100000);
        let dst = image(129, 65, 8, 3, tiled(), 0x200000);
        let planned = image_layout(dst, GPU_IMAGE_USAGE_TRANSFER_DST).unwrap();
        let sub = planned.subresource(1, 2).unwrap();
        let exact = ImageAuthority {
            range_offset: sub.offset,
            range_size: sub.level.size,
            ..dst
        };
        assert!(lower(blit(), exact, SourceAuthority::Image(src)).is_ok());
        // Generic allocations may include page padding after the exact image
        // layout. The caller checks the allocation size; no padding is read.
        let padded = ImageAuthority {
            range_size: dst.range_size + 4096,
            ..dst
        };
        assert!(lower(blit(), padded, SourceAuthority::Image(src)).is_ok());
        for narrow in [
            ImageAuthority {
                range_size: sub.level.size - 1,
                ..exact
            },
            ImageAuthority {
                range_offset: sub.offset + 1,
                ..exact
            },
            ImageAuthority {
                range_offset: 0,
                range_size: sub.level.size,
                ..exact
            },
        ] {
            assert!(lower(blit(), narrow, SourceAuthority::Image(src)).is_err());
        }
    }

    #[test]
    fn wrong_usage_depth_and_layout_padding_are_rejected() {
        let dst = image(17, 7, 1, 1, LayoutKind::Linear, 0x200000);
        let mut bad = dst;
        bad.create.usage = GPU_IMAGE_USAGE_TRANSFER_SRC;
        assert!(lower(upload(), bad, buffer()).is_err());
        bad = dst;
        bad.create.format = 2;
        assert!(lower(upload(), bad, buffer()).is_err());
        bad = dst;
        bad.layout.planes[0].row_pitch = 68;
        assert!(lower(upload(), bad, buffer()).is_err());
        bad = dst;
        bad.layout.planes[0].array_pitch += 256;
        assert!(lower(upload(), bad, buffer()).is_err());
        bad = dst;
        bad.layout.modifier = maxwell_image_layout::NVIDIA_DEPTH_MODIFIER_BASE | 4;
        assert!(lower(upload(), bad, buffer()).is_err());
        let src = image(129, 65, 8, 3, tiled(), 0x100000);
        let dst = image(129, 65, 8, 3, tiled(), 0x200000);
        let mut bad_src = src;
        bad_src.create.usage = GPU_IMAGE_USAGE_TRANSFER_DST;
        assert!(lower(blit(), dst, SourceAuthority::Image(bad_src)).is_err());
    }

    #[test]
    fn out_of_bounds_subresources_rectangles_and_aliases_are_rejected() {
        let img = image(129, 65, 8, 3, tiled(), 0x200000);
        for command in [
            Command {
                source_mip: 8,
                ..blit()
            },
            Command {
                destination_layer: 3,
                ..blit()
            },
            Command {
                source_rect: [31, 2, 8, 4],
                ..blit()
            },
            Command {
                destination_rect: [3, 31, 16, 8],
                ..blit()
            },
            Command {
                source_mip: 1,
                source_layer: 2,
                ..blit()
            },
        ] {
            assert!(lower(command, img, SourceAuthority::Image(img)).is_err());
        }
        assert!(lower(upload(), img, SourceAuthority::Image(img)).is_err());
        assert!(lower(blit(), img, buffer()).is_err());
    }

    #[test]
    fn gpu_addresses_and_declared_source_ranges_cannot_overflow() {
        let dst = image(17, 7, 1, 1, LayoutKind::Linear, 0x200000);
        for source in [
            BufferAuthority {
                va: u64::MAX,
                range_offset: 64,
                range_size: 80,
            },
            BufferAuthority {
                va: VA_LIMIT - 127,
                range_offset: 64,
                range_size: 80,
            },
            BufferAuthority {
                va: 0x100000,
                range_offset: u64::MAX - 32,
                range_size: 80,
            },
        ] {
            assert!(lower(upload(), dst, SourceAuthority::Buffer(source)).is_err());
        }
        assert!(
            lower(
                Command {
                    source_offset: u64::MAX - 32,
                    ..upload()
                },
                dst,
                buffer()
            )
            .is_err()
        );
        assert!(
            lower(
                upload(),
                ImageAuthority {
                    va: VA_LIMIT - dst.layout.total_size + 1,
                    ..dst
                },
                buffer()
            )
            .is_err()
        );
        let exact = SourceAuthority::Buffer(BufferAuthority {
            va: VA_LIMIT - 144,
            range_offset: 64,
            range_size: 80,
        });
        assert!(lower(upload(), dst, exact).is_ok());
    }

    #[test]
    fn every_supported_color_block_height_can_be_lowered() {
        for y in 0..=4 {
            let dst = image(
                17,
                7,
                1,
                1,
                LayoutKind::BlockLinear {
                    base_y_log2: y,
                    clamp_mips: false,
                },
                0x200000,
            );
            assert_eq!(
                lower(upload(), dst, buffer()).unwrap()[52],
                0x100 | (u32::from(y) << 4)
            );
        }
    }
}

// SPDX-License-Identifier: GPL-2.0-only
//! Capability-authorized SGFX queue and independent GPU DMA backing.
//! Like the Chromebook backend, object identities are separate from attachment
//! tokens. User bytes can never select a GPU method, physical address or shader.

use crate::{
    gmmu::{VA_LIMIT, clean, pages, pages_aligned},
    runtime::Power,
};
use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use maxwell_shader_pack::PipelineVariant;
use maxwell_submit_wire as wire;
use scarlet::device::graphics::shared_image::*;
use scarlet::{
    arch,
    device::{
        devfreq::{DeviceFrequencyDriver, DeviceFrequencyUtilization},
        gpu::*,
        graphics::{GpuDisplayResource, PixelFormat},
    },
    mem::page::ContiguousPages,
    sync::Mutex,
    time,
};

const COOKIE: u64 = 0x474d323042535031;
const DIALECT_TOKEN: u64 = 0x474d325347465801;
const DIALECT: &[u8] = b"maxwell-sgfx-ops-v1";
const PROGRAMMABLE_DIALECT_TOKEN: u64 = 0x474d325347465802;
const PROGRAMMABLE_DIALECT: &[u8] = b"maxwell-sgfx-ops-v2";
const SUPPORT: u32 = GPU_EXECUTION_SUPPORT_ADDRESS_SPACE
    | GPU_EXECUTION_SUPPORT_MEMORY
    | GPU_EXECUTION_SUPPORT_QUEUE
    | GPU_EXECUTION_SUPPORT_TIMELINE
    | GPU_EXECUTION_SUPPORT_PRESENTATION
    | GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD
    | GPU_EXECUTION_SUPPORT_IMAGE_READBACK
    | GPU_EXECUTION_SUPPORT_IMAGE_MIPS
    | GPU_EXECUTION_SUPPORT_TEXTURE_ARRAYS
    | GPU_EXECUTION_SUPPORT_DEPTH;

#[derive(Clone)]
pub(super) enum Kind {
    Buffer {
        paddr: u64,
    },
    SharedImage {
        create: GpuImageCreateInfo,
        layout: GpuBackendImageLayout,
        image: Arc<SharedImage>,
        color: ImageColor,
    },
    Image {
        create: GpuImageCreateInfo,
        layout: GpuBackendImageLayout,
        backing: GpuImageBackingInfo,
    },
}
pub(super) struct Memory {
    pub pages: Option<ContiguousPages>,
    pub va: usize,
    pub size: u64,
    pub kind: Kind,
}
struct State {
    power: Power,
    next_object: u64,
    next_attachment: u64,
    lost: bool,
    diagnostic_submissions: u64,
    diagnostic_allocation_failures: u64,
}
pub(super) struct Shared {
    state: Mutex<State>,
    utilization: Option<crate::utilization::UtilizationMonitor>,
    pub(super) work: crate::asynchronous::WorkQueue,
}
pub struct Backend {
    shared: Arc<Shared>,
    snapshot: [u8; 64],
    gpu_base: usize,
}
impl Backend {
    pub fn new(
        power: Power,
        snapshot: [u8; 64],
        gpu_base: usize,
        utilization: Option<crate::utilization::UtilizationMonitor>,
    ) -> Result<Self, &'static str> {
        let backend = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    power,
                    next_object: 1,
                    next_attachment: 1,
                    lost: false,
                    diagnostic_submissions: 0,
                    diagnostic_allocation_failures: 0,
                }),
                utilization,
                work: crate::asynchronous::WorkQueue::new()?,
            }),
            snapshot,
            gpu_base,
        };
        crate::asynchronous::register(&backend.shared)?;
        Ok(backend)
    }
}

impl DeviceFrequencyDriver for Backend {
    fn sample_utilization(&self) -> Result<DeviceFrequencyUtilization, &'static str> {
        self.shared
            .utilization
            .as_ref()
            .ok_or("GM20B PMU activity counters unavailable")?
            .sample()
    }
    fn current_frequency_khz(&self) -> Result<u64, &'static str> {
        let state = self.shared.state.lock();
        if state.lost {
            return Err("GM20B device lost");
        }
        crate::clock::current_rate_khz(self.gpu_base, state.power.platform.reference_hz())
    }

    fn set_frequency_khz(&self, freq_khz: u64) -> Result<(), &'static str> {
        // Every queue submission, allocation and release uses this same
        // sleepable mutex. A completed synchronous submit has retired its
        // GPU fence; verify GR idle before touching the PLL post-divider.
        let mut state = self.shared.state.lock();
        if state.lost {
            return Err("GM20B device lost");
        }
        state.power.dma.as_ref().unwrap().idle()?;
        match crate::clock::set_rate_khz(
            self.gpu_base,
            state.power.platform.reference_hz(),
            freq_khz,
        ) {
            Ok(()) => Ok(()),
            Err(crate::clock::RateChangeError::Reverted(reason)) => Err(reason),
            Err(crate::clock::RateChangeError::Unsafe(reason)) => {
                state.fault();
                Err(reason)
            }
        }
    }
}
impl State {
    fn dma(&self) -> &crate::gmmu::Gmmu {
        self.power.dma.as_ref().unwrap()
    }
    fn allocate(&mut self, size: u64, kind: Kind) -> Result<(u64, Arc<Memory>), &'static str> {
        if self.lost {
            return Err("GM20B device lost");
        }
        let count = usize::try_from(size)
            .map_err(|_| "GPU object too large")?
            .div_ceil(4096);
        if count == 0 || count > 0x1000000 / 4096 {
            return Err(self.allocation_error(size, "GPU object size exceeds address-space budget"));
        }
        // The private 0x500000..0x501fff proof surface stays mapped for the
        // lifetime of the graphics context; public allocations start after it.
        let tiled = matches!(
            &kind,
            Kind::Image { layout, .. }
                if layout.modifier != GPU_IMAGE_MODIFIER_LINEAR
        );
        let alignment = if tiled { 8192 } else { 4096 };
        let mut va: usize = 0x502000;
        // First fit over sorted retained mappings; each submission is fully
        // retired under this mutex before allocation or mapping retirement.
        for (&start, entry) in &self.dma().retained {
            va = (va + alignment - 1) & !(alignment - 1);
            if va + count * 4096 <= start {
                break;
            }
            va = va.max(start + entry.size as usize);
        }
        va = (va + alignment - 1) & !(alignment - 1);
        if va
            .checked_add(count * 4096)
            .is_none_or(|end| end > VA_LIMIT as usize)
        {
            return Err(self.allocation_error(size, "GM20B GPU address space exhausted"));
        }
        let memory = (if tiled {
            pages_aligned(count, alignment)
        } else {
            pages(count)
        })
        .map_err(|error| self.allocation_error(size, error))?;
        // The allocator zeroes through the cached CPU mapping. Publish those
        // lines before DMA so a later eviction cannot overwrite GPU output.
        clean(&memory);
        let object = self.next_object;
        self.next_object = self
            .next_object
            .checked_add(1)
            .ok_or("GPU object identity exhausted")?;
        let page_kind = match &kind {
            Kind::Image { layout, .. }
                if maxwell_image_layout::modifier_tile_y(layout.modifier).is_some()
                    && layout.modifier & !0xf
                        == maxwell_image_layout::NVIDIA_DEPTH_MODIFIER_BASE =>
            {
                0x7b
            }
            Kind::Image { layout, .. }
                if maxwell_image_layout::modifier_tile_y(layout.modifier).is_some()
                    && layout.modifier & !0xf
                        == maxwell_image_layout::NVIDIA_COLOR_MODIFIER_BASE =>
            {
                0xfe
            }
            _ => 0,
        };
        let memory = Arc::new(Memory {
            pages: Some(memory),
            va,
            size,
            kind,
        });
        // Retain independently of generic capabilities before publishing PTEs.
        self.power
            .dma
            .as_mut()
            .unwrap()
            .retained
            .insert(va, Arc::clone(&memory));
        if let Err(error) = self
            .dma()
            .map_private_kind(va, memory.pages.as_ref().unwrap(), page_kind)
            .and_then(|_| self.dma().invalidate_all())
        {
            self.fault();
            return Err(error);
        }
        if object <= 4 {
            scarlet::println!("gm20b: object={} mapped va={:#x} size={}", object, va, size);
        }
        Ok((object, memory))
    }
    fn import_image(
        &mut self,
        image: Arc<SharedImage>,
        color: ImageColor,
    ) -> Result<(u64, Arc<Memory>, GpuBackendImageLayout), &'static str> {
        if self.lost {
            return Err("GM20B device lost");
        }
        let d = image.descriptor();
        d.validate()?;
        if d.format != IMAGE_FORMAT_NV12
            || !matches!(d.modifier, 0 | 0x0300_0000_000f_e011)
            || d.width > 4096
            || d.height > 4096
            || !matches!(color.matrix, COLOR_MATRIX_BT601 | COLOR_MATRIX_BT709)
            || !matches!(color.range, COLOR_RANGE_LIMITED | COLOR_RANGE_FULL)
            || !matches!(color.chroma_x, CHROMA_COSITED | CHROMA_MIDPOINT)
            || !matches!(color.chroma_y, CHROMA_COSITED | CHROMA_MIDPOINT)
            || color.reserved != [0; 2]
            || !matches!(color.primaries, 0 | 1 | 5 | 6)
            || !matches!(color.transfer, 0 | 1 | 6 | 13)
        {
            return Err("GM20B shared image format/modifier/conversion unsupported");
        }
        let mut starts = [0u64; 4];
        let mut size = 0u64;
        for i in 0..d.buffer_count as usize {
            starts[i] = size;
            size = size
                .checked_add(
                    d.buffer_sizes[i]
                        .checked_add(4095)
                        .ok_or("image overflow")?
                        & !4095,
                )
                .ok_or("image overflow")?;
        }
        if size > 0x1000000 {
            return Err("GM20B imported image too large");
        }
        let mut planes = [GpuBackendImagePlaneLayout::EMPTY; 4];
        for i in 0..2 {
            let p = d.planes[i];
            let rows = if i == 0 {
                d.height
            } else {
                d.height.div_ceil(2)
            };
            let row_bytes = if i == 0 {
                d.width
            } else {
                d.width.div_ceil(2) * 2
            };
            let tiled = d.modifier != 0;
            let min_size =
                u64::from(p.row_pitch) * u64::from(if tiled { (rows + 15) & !15 } else { rows });
            if p.row_pitch < row_bytes
                || p.row_pitch % (if tiled { 64 } else { 32 }) != 0
                || p.offset % (if tiled { 512 } else { 32 }) != 0
                || p.size < min_size
                || p.size > u64::from(u32::MAX)
            {
                return Err("GM20B imported plane layout unsupported");
            }
            planes[i] = GpuBackendImagePlaneLayout {
                offset: starts[p.buffer_index as usize] + p.offset,
                size: p.size,
                row_pitch: p.row_pitch,
                array_pitch: p.size as u32,
                block_width: 1,
                block_height: 1,
                bytes_per_block: if i == 0 { 1 } else { 2 },
            };
        }
        let layout = GpuBackendImageLayout {
            modifier: d.modifier,
            total_size: size,
            alignment: 4096,
            plane_count: 2,
            planes,
        };
        if !layout.is_valid() {
            return Err("GM20B imported image layout invalid");
        }
        let mut va = 0x502000usize;
        for (&start, entry) in &self.dma().retained {
            va = (va + 8191) & !8191;
            if va + size as usize <= start {
                break;
            }
            va = va.max(start + entry.size as usize);
        }
        va = (va + 8191) & !8191;
        if va
            .checked_add(size as usize)
            .is_none_or(|end| end > VA_LIMIT as usize)
        {
            return Err("GM20B GPU address space exhausted");
        }
        let id = self.next_object;
        self.next_object = id.checked_add(1).ok_or("GPU object identity exhausted")?;
        let create = GpuImageCreateInfo::new(
            GPU_IMAGE_FORMAT_NV12,
            GPU_IMAGE_USAGE_SAMPLED,
            d.visible.width,
            d.visible.height,
        );
        let memory = Arc::new(Memory {
            pages: None,
            va,
            size,
            kind: Kind::SharedImage {
                create,
                layout,
                image: image.clone(),
                color,
            },
        });
        // The retained mapping owns the lease even if invalidation/isolation fails.
        self.power
            .dma
            .as_mut()
            .unwrap()
            .retained
            .insert(va, memory.clone());
        let result = (|| {
            let backing = image.backing();
            for i in 0..d.buffer_count as usize {
                let length = (d.buffer_sizes[i] + 4095) & !4095;
                self.dma().map_shared(
                    va + starts[i] as usize,
                    length as usize,
                    &backing.buffer_segments(i),
                    if d.modifier == 0 { 0 } else { 0xfe },
                )?;
            }
            self.dma().invalidate_all()
        })();
        if let Err(error) = result {
            self.fault();
            return Err(error);
        }
        self.power
            .dma
            .as_mut()
            .unwrap()
            .objects
            .insert(id, memory.clone());
        Ok((id, memory, layout))
    }

    fn allocation_error(&mut self, size: u64, reason: &'static str) -> &'static str {
        self.diagnostic_allocation_failures = self.diagnostic_allocation_failures.saturating_add(1);
        let count = self.diagnostic_allocation_failures;
        if count <= 4 {
            let retained_bytes: usize = self
                .dma()
                .retained
                .values()
                .map(|memory| memory.size as usize)
                .sum();
            scarlet::println!(
                "gm20b: allocation failed: {} request={} retained={} objects={}",
                reason,
                size,
                retained_bytes,
                self.dma().retained.len()
            );
        }
        reason
    }
    fn retire_private(&mut self, memory: &[Arc<Memory>]) -> Result<(), GpuBackendSubmitError> {
        if self.lost {
            return Err(GpuBackendSubmitError::DeviceLost("GM20B device lost"));
        }
        for arena in memory {
            if let Err(reason) = self
                .dma()
                .unmap(arena.va, (arena.size as usize).div_ceil(4096))
            {
                self.fault();
                return Err(GpuBackendSubmitError::DeviceLost(reason));
            }
            self.power.dma.as_mut().unwrap().retained.remove(&arena.va);
        }
        Ok(())
    }
    fn fault(&mut self) {
        self.lost = true;
        self.dma().disable_completion_irq();
        // DMA only touches independently owned pages. Failed isolation retains
        // them in Gmmu until Power can safely drain, or leaks them on rollback.
        if let Err(error) = self.power.platform.isolate() {
            scarlet::println!("gm20b: lost device DMA backing retained: {}", error);
        }
    }
}
impl Shared {
    fn release(&self, memory: &Memory) {
        let mut s = self.state.lock();
        if s.lost {
            return;
        }
        match s
            .dma()
            .unmap(memory.va, (memory.size as usize).div_ceil(4096))
        {
            Ok(()) => {
                s.power
                    .dma
                    .as_mut()
                    .unwrap()
                    .objects
                    .retain(|_, m| m.va != memory.va);
                s.power.dma.as_mut().unwrap().retained.remove(&memory.va);
            }
            Err(error) => {
                scarlet::println!("gm20b: mapping retirement failed: {}", error);
                s.fault();
            }
        }
    }
}
struct Buffer {
    shared: Arc<Shared>,
    id: u64,
    memory: Arc<Memory>,
}
struct Image {
    shared: Arc<Shared>,
    id: u64,
    memory: Arc<Memory>,
}
impl Drop for Buffer {
    fn drop(&mut self) {
        self.shared.release(&self.memory);
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        self.shared.release(&self.memory);
    }
}
impl GpuBackendBuffer for Buffer {
    fn backend_cookie(&self) -> u64 {
        COOKIE
    }
    fn query_info(&self) -> GpuBackendBufferInfo {
        GpuBackendBufferInfo::new(self.id, self.memory.size)
    }
}
impl GpuBackendImage for Image {
    fn backend_cookie(&self) -> u64 {
        COOKIE
    }
    fn query_info(&self) -> GpuBackendImageInfo {
        let create = match &self.memory.kind {
            Kind::Image { create, .. } | Kind::SharedImage { create, .. } => create,
            _ => unreachable!(),
        };
        GpuBackendImageInfo::new(*create, self.id, self.memory.size)
    }
    fn display_resource(&self) -> Option<GpuDisplayResource> {
        let Kind::Image { create, layout, .. } = &self.memory.kind else {
            return None;
        };
        if create.usage & GPU_IMAGE_USAGE_PRESENTABLE == 0 {
            return None;
        }
        let owner: Arc<dyn scarlet::device::gpu::GpuDisplayBackingOwner> = self.memory.clone();
        let paddr = self.memory.pages.as_ref().unwrap().as_paddr();
        let stride = layout.planes[0].row_pitch;
        if layout.modifier == GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4 {
            GpuDisplayResource::new_modified(
                paddr,
                self.memory.size,
                create.width,
                create.height,
                stride,
                PixelFormat::BGRA8888,
                layout.modifier,
                owner,
            )
            .ok()
        } else {
            GpuDisplayResource::new_linear(
                paddr,
                self.memory.size,
                create.width,
                create.height,
                stride,
                PixelFormat::BGRA8888,
                owner,
            )
            .ok()
        }
    }
}

/// Rebuild the same immutable physical chain used for image allocation.
/// Staging uses linear rows inside each padded level; private GPU bytes use
/// its GOB swizzle. Both sides share identical level and layer offsets.
pub(super) fn image_subresources(
    create: GpuImageCreateInfo,
    layout: GpuBackendImageLayout,
) -> Result<maxwell_image_layout::Layout, &'static str> {
    let kind = if layout.modifier == GPU_IMAGE_MODIFIER_LINEAR {
        maxwell_image_layout::LayoutKind::Linear
    } else {
        let y = maxwell_image_layout::modifier_tile_y(layout.modifier)
            .ok_or("unsupported GM20B image modifier")?;
        maxwell_image_layout::LayoutKind::BlockLinear {
            base_y_log2: y,
            clamp_mips: create.mip_levels > 1 || create.array_layers > 1 || create.cube,
        }
    };
    let planned = maxwell_image_layout::plan(
        maxwell_image_layout::Descriptor {
            width: create.width,
            height: create.height,
            mip_levels: create.mip_levels,
            array_layers: create.array_layers,
            bytes_per_pixel: 4,
        },
        kind,
    )
    .map_err(|_| "invalid GM20B image subresources")?;
    let p = layout.planes[0];
    if layout.plane_count != 1
        || p.offset != 0
        || p.size != planned.total_size
        || layout.total_size != planned.total_size
        || p.row_pitch != planned.levels[0].row_pitch
        || p.array_pitch != planned.array_pitch
        || p.block_width != 1
        || p.block_height != 1
        || p.bytes_per_block != 4
    {
        return Err("GM20B image subresources/layout mismatch");
    }
    Ok(planned)
}
// A context carries real attachment authority; shader IDs and object IDs never
// grant access. IDs are indexed separately to support repeated attachments.
pub(super) struct Attachment {
    object: u64,
    memory: Arc<Memory>,
}
#[derive(Clone)]
pub(super) struct Context {
    pub(super) shared: Arc<Shared>,
    pub(super) attachments: Arc<Mutex<BTreeMap<u64, Attachment>>>,
    pub(super) dialect_index: u32,
    dialect_token: u64,
}
impl Context {
    fn attach(&self, object: u64, cookie: u64) -> Result<u64, &'static str> {
        if cookie != COOKIE || object == 0 {
            return Err("foreign GM20B resource");
        }
        let mut s = self.shared.state.lock();
        if s.lost {
            return Err("GM20B device lost");
        }
        // The generic resource retains its backend object during attachment.
        // Query by an internal object registry, never by userspace-supplied VA.
        let memory = s
            .dma()
            .objects
            .get(&object)
            .cloned()
            .ok_or("GM20B resource expired")?;
        let token = s.next_attachment;
        s.next_attachment = s
            .next_attachment
            .checked_add(1)
            .ok_or("GPU attachment generation exhausted")?;
        drop(s);
        self.attachments
            .lock()
            .insert(token, Attachment { object, memory });
        Ok(token)
    }
    fn detach(&self, object: u64, cookie: u64) -> Result<(), &'static str> {
        if cookie != COOKIE {
            return Err("foreign GM20B resource");
        }
        self.attachments.lock().retain(|_, a| a.object != object);
        Ok(())
    }
    fn image(&self, image: &dyn GpuBackendImage) -> Result<Arc<Memory>, &'static str> {
        if image.backend_cookie() != COOKIE {
            return Err("foreign image");
        }
        let object = image.query_info().command_resource_token;
        self.attachments
            .lock()
            .values()
            .find(|a| a.object == object)
            .map(|a| Arc::clone(&a.memory))
            .ok_or("GM20B image not attached")
    }
    fn transfer(
        &self,
        image: &dyn GpuBackendImage,
        rect: GpuImageUploadInfo,
        readback: bool,
    ) -> Result<(), &'static str> {
        let mem = self.image(image)?;
        let s = self.shared.state.lock();
        if s.lost {
            return Err("GM20B device lost");
        }
        let Kind::Image {
            create,
            layout,
            backing,
        } = &mem.kind
        else {
            return Err("resource is not an image");
        };
        if create.format != GPU_IMAGE_FORMAT_BGRA8_UNORM {
            return Err("CPU transfer requires a color image");
        }
        let planned = image_subresources(*create, *layout)?;
        let sub = planned
            .resolve_transfer(maxwell_image_layout::Transfer {
                offset: rect.backing_offset,
                row_pitch: rect.backing_stride,
                array_pitch: rect.backing_layer_stride,
                x: rect.dst_x,
                y: rect.dst_y,
                width: rect.width,
                height: rect.height,
            })
            .map_err(|_| "image transfer layout mismatch")?;
        let pitch = sub.level.row_pitch;
        if layout.modifier != GPU_IMAGE_MODIFIER_LINEAR {
            // Generic backing is linear CPU staging. The independent private
            // DMA allocation follows the block-linear modifier. Preserve all
            // pixels outside a partial update when acquiring cache lines.
            let gpu_base = mem.pages.as_ref().unwrap().as_vaddr();
            arch::invalidate_dcache_to_poc_range(gpu_base, mem.size as usize);
            for y in rect.dst_y..rect.dst_y + rect.height {
                let mut x = rect.dst_x as usize * 4;
                let end = x + rect.width as usize * 4;
                while x < end {
                    let n = (16 - x % 16).min(end - x);
                    let generic = scarlet::vm::phys_to_virt(
                        backing.paddr + sub.offset + y as u64 * pitch as u64 + x as u64,
                    );
                    let offset = planned
                        .byte_offset(sub, x as u32, y)
                        .map_err(|_| "image transfer GPU offset overflow")?;
                    let gpu = gpu_base
                        + usize::try_from(offset)
                            .map_err(|_| "image transfer offset exceeds address space")?;
                    unsafe {
                        if readback {
                            core::ptr::copy_nonoverlapping(gpu as *const u8, generic as *mut u8, n);
                        } else {
                            core::ptr::copy_nonoverlapping(generic as *const u8, gpu as *mut u8, n);
                        }
                    }
                    x += n;
                }
            }
            if !readback {
                arch::clean_dcache_to_poc_range(gpu_base, mem.size as usize);
            }
            return Ok(());
        }
        for y in 0..rect.height {
            let offset = rect.backing_offset as usize + y as usize * pitch as usize;
            let n = rect.width as usize * 4;
            let generic = scarlet::vm::phys_to_virt(backing.paddr + offset as u64);
            let gpu = mem.pages.as_ref().unwrap().as_vaddr() + offset;
            if readback {
                arch::invalidate_dcache_to_poc_range(gpu, n);
                unsafe {
                    core::ptr::copy_nonoverlapping(gpu as *const u8, generic as *mut u8, n);
                }
            } else {
                // The GPU may have written the surrounding pixels. Fetch the
                // latest cache line before a partial CPU update so cleaning
                // it cannot overwrite untouched pixels with a stale alias.
                arch::invalidate_dcache_to_poc_range(gpu, n);
                unsafe {
                    core::ptr::copy_nonoverlapping(generic as *const u8, gpu as *mut u8, n);
                }
                arch::clean_dcache_to_poc_range(gpu, n);
            }
        }
        // The CPU-access reservation drained admitted GPU work before this transfer.
        // Imported layouts are checked by the same transfer validation.
        Ok(())
    }
}
impl GpuBackendContext for Context {
    fn begin_image_cpu_access(
        &self,
        image: &dyn GpuBackendImage,
    ) -> Result<Option<Box<dyn GpuBackendCpuAccessGuard + '_>>, &'static str> {
        self.image(image)?;
        Ok(Some(Box::new(crate::asynchronous::begin_cpu_access(
            &self.shared,
        )?)))
    }
    fn query_info(&self) -> GpuBackendContextInfo {
        GpuBackendContextInfo::new(self.dialect_index, self.dialect_token)
    }
    fn create_queue(&self) -> Result<Arc<dyn GpuBackendQueue>, &'static str> {
        Ok(Arc::new(Queue {
            context: self.clone(),
        }))
    }
    fn attach_image(&self, image: &dyn GpuBackendImage) -> Result<u64, &'static str> {
        self.attach(
            image.query_info().command_resource_token,
            image.backend_cookie(),
        )
    }
    fn detach_image(&self, image: &dyn GpuBackendImage) -> Result<(), &'static str> {
        self.detach(
            image.query_info().command_resource_token,
            image.backend_cookie(),
        )
    }
    fn attach_buffer(&self, buffer: &dyn GpuBackendBuffer) -> Result<u64, &'static str> {
        self.attach(
            buffer.query_info().command_resource_token,
            buffer.backend_cookie(),
        )
    }
    fn detach_buffer(&self, buffer: &dyn GpuBackendBuffer) -> Result<(), &'static str> {
        self.detach(
            buffer.query_info().command_resource_token,
            buffer.backend_cookie(),
        )
    }
    fn upload_image_bgra(
        &self,
        image: &dyn GpuBackendImage,
        rect: GpuImageUploadInfo,
    ) -> Result<(), &'static str> {
        self.transfer(image, rect, false)
    }
    fn transfer_imported_image_bgra(
        &self,
        image: &dyn GpuBackendImage,
        rect: GpuImageUploadInfo,
    ) -> Result<(), &'static str> {
        self.transfer(image, rect, false)
    }
    fn readback_image_bgra(
        &self,
        image: &dyn GpuBackendImage,
        rect: GpuImageUploadInfo,
    ) -> Result<(), &'static str> {
        self.transfer(image, rect, true)
    }
}
struct Queue {
    context: Context,
}
impl GpuBackendQueue for Queue {
    fn query_info(&self) -> GpuBackendQueueInfo {
        GpuBackendQueueInfo::new(wire::MAX_SUBMIT_SIZE as u32)
    }
    fn submit(&self, bytes: &[u8]) -> Result<(), GpuBackendSubmitError> {
        crate::asynchronous::submit(&self.context, bytes)
    }
    fn async_capacity(&self) -> u32 {
        crate::asynchronous::CAPACITY as u32
    }
    fn enqueue(&self, submission: GpuSubmission) -> Result<(), GpuBackendEnqueueError> {
        crate::asynchronous::enqueue(&self.context, submission)
    }
}

struct BufferSpan {
    memory: Arc<Memory>,
    start: usize,
    end: usize,
}
struct BufferSnapshot {
    span: BufferSpan,
    bytes: Vec<u8>,
}
pub(super) struct Prepared {
    operations: Vec<[u32; 96]>,
    programmable: Vec<crate::programmable_graphics::PreparedDraw>,
    buffers: Vec<BufferSnapshot>,
    writes: Vec<BufferSpan>,
    _images: Vec<Arc<Memory>>,
}
impl Prepared {
    pub(super) fn new(
        bytes: &[u8],
        attached: &BTreeMap<u64, Attachment>,
        programmable: bool,
    ) -> Result<Self, GpuBackendSubmitError> {
        Self::prepare(bytes, attached, programmable).map_err(GpuBackendSubmitError::Rejected)
    }
    fn prepare(
        bytes: &[u8],
        attached: &BTreeMap<u64, Attachment>,
        programmable: bool,
    ) -> Result<Self, &'static str> {
        let mut prepared = Self {
            operations: Vec::new(),
            programmable: Vec::new(),
            buffers: Vec::new(),
            writes: Vec::new(),
            _images: Vec::new(),
        };
        if bytes.is_empty() {
            return Ok(prepared);
        }
        let decoded = wire::decode(bytes).map_err(|_| "invalid GM20B submit wire")?;
        let count = decoded.resource_len();
        let mut spans: Vec<BufferSpan> = Vec::new();
        spans
            .try_reserve_exact(count)
            .map_err(|_| "GPU snapshot allocation failed")?;
        prepared
            .writes
            .try_reserve_exact(count)
            .map_err(|_| "GPU snapshot allocation failed")?;
        prepared
            ._images
            .try_reserve_exact(count)
            .map_err(|_| "GPU snapshot allocation failed")?;
        for i in 0..count {
            let resource = decoded.resource(i).unwrap();
            let memory = &attached
                .get(&resource.attachment_token)
                .ok_or("unauthorized attachment")?
                .memory;
            let end = resource
                .range_offset
                .checked_add(resource.range_size)
                .ok_or("resource range overflow")?;
            if end > memory.size {
                return Err("resource range exceeds attachment");
            }
            if matches!(memory.kind, Kind::Buffer { .. }) {
                spans.push(BufferSpan {
                    memory: memory.clone(),
                    start: resource.range_offset as usize,
                    end: end as usize,
                });
                if resource.access & wire::ACCESS_WRITE != 0 {
                    prepared.writes.push(BufferSpan {
                        memory: memory.clone(),
                        start: resource.range_offset as usize,
                        end: end as usize,
                    });
                }
            } else {
                prepared._images.push(memory.clone());
            }
        }
        // Merge overlapping aliases before sampling user backing. Otherwise a
        // second snapshot could overwrite index bytes after they were checked.
        // Disjoint ranges stay disjoint: reserved buffer capacity is not copied.
        spans.sort_unstable_by_key(|span| (span.memory.va, span.start));
        let mut merged: Vec<BufferSpan> = Vec::new();
        merged
            .try_reserve_exact(spans.len())
            .map_err(|_| "GPU snapshot allocation failed")?;
        for span in spans {
            if let Some(last) = merged.last_mut() {
                if last.memory.va == span.memory.va && span.start <= last.end {
                    last.end = last.end.max(span.end);
                    continue;
                }
            }
            merged.push(span);
        }
        prepared
            .buffers
            .try_reserve_exact(merged.len())
            .map_err(|_| "GPU snapshot allocation failed")?;
        let mut total = 0usize;
        for span in merged {
            let size = span.end - span.start;
            total = total
                .checked_add(size)
                .ok_or("GPU snapshot size overflow")?;
            if total > 32 * 1024 * 1024 {
                return Err("GPU snapshot budget exceeded");
            }
            let Kind::Buffer { paddr } = span.memory.kind else {
                unreachable!()
            };
            let mut data = Vec::new();
            data.try_reserve_exact(size)
                .map_err(|_| "GPU buffer snapshot allocation failed")?;
            // Snapshot immutable CPU-owned bytes without touching DMA backing
            // still being read by an earlier accepted submission.
            unsafe {
                data.extend_from_slice(core::slice::from_raw_parts(
                    (scarlet::vm::phys_to_virt(paddr) + span.start) as *const u8,
                    size,
                ));
            }
            prepared.buffers.push(BufferSnapshot { span, bytes: data });
        }
        prepared.operations = validate(
            &decoded,
            attached,
            &prepared.buffers,
            &mut prepared.programmable,
            programmable,
        )?;
        Ok(prepared)
    }
}
impl Shared {
    pub(super) fn execute_prepared(
        &self,
        prepared: &Prepared,
    ) -> Result<(), GpuBackendSubmitError> {
        let started = time::current_time_ns();
        let mut s = self.state.lock();
        if s.lost {
            return Err(GpuBackendSubmitError::DeviceLost("GM20B device lost"));
        }
        // Empty requests are ordered checkpoints, never early acknowledgments.
        if prepared.operations.is_empty() {
            return Ok(());
        }
        s.diagnostic_submissions = s.diagnostic_submissions.saturating_add(1);
        let sequence = s.diagnostic_submissions;
        let trace = sequence <= 4;
        for buffer in &prepared.buffers {
            let destination =
                buffer.span.memory.pages.as_ref().unwrap().as_vaddr() + buffer.span.start;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buffer.bytes.as_ptr(),
                    destination as *mut u8,
                    buffer.bytes.len(),
                );
            }
            arch::clean_dcache_to_poc_range(destination, buffer.bytes.len());
        }
        let uploaded = time::current_time_ns();
        let mut arenas = Vec::new();
        let mut programs = Vec::new();
        arenas
            .try_reserve_exact(prepared.programmable.len())
            .map_err(|_| {
                GpuBackendSubmitError::Rejected("private program arena allocation failed")
            })?;
        programs
            .try_reserve_exact(prepared.programmable.len())
            .map_err(|_| {
                GpuBackendSubmitError::Rejected("private program method allocation failed")
            })?;
        let publication = (|| -> Result<(), &'static str> {
            for program in &prepared.programmable {
                // This mapping is deliberately absent from dma.objects. It
                // has no public object identity and cannot become an attachment.
                let (_, memory) =
                    s.allocate(program.arena.len() as u64, Kind::Buffer { paddr: 0 })?;
                let destination = memory.pages.as_ref().unwrap().as_vaddr();
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        program.arena.as_ptr(),
                        destination as *mut u8,
                        program.arena.len(),
                    );
                }
                arch::clean_dcache_to_poc_range(destination, program.arena.len());
                let published = program.publish(memory.va as u64);
                arenas.push(memory);
                programs.push(published?);
            }
            Ok(())
        })();
        if let Err(reason) = publication {
            if s.lost {
                return Err(GpuBackendSubmitError::DeviceLost(reason));
            }
            s.retire_private(&arenas)?;
            return Err(GpuBackendSubmitError::Rejected(reason));
        }
        let result = s
            .power
            .dma
            .as_mut()
            .unwrap()
            .execute_graphics_with_programs(&prepared.operations, &programs);
        if matches!(result, Err(GpuBackendSubmitError::DeviceLost(_))) {
            // Completion was not proven. Keep all executable, index and
            // descriptor mappings pinned while the platform isolates DMA.
            s.fault();
            return result;
        }
        // execute_graphics has proved the PGRAPH fence and GR idle, or rejected
        // before submission. Only either of those permits mapping retirement.
        s.retire_private(&arenas)?;
        result?;
        // Reflect only declared writable ranges, after actual GPU retirement.
        for span in &prepared.writes {
            let Kind::Buffer { paddr } = span.memory.kind else {
                unreachable!()
            };
            let size = span.end - span.start;
            let source = span.memory.pages.as_ref().unwrap().as_vaddr() + span.start;
            arch::invalidate_dcache_to_poc_range(source, size);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source as *const u8,
                    (scarlet::vm::phys_to_virt(paddr) + span.start) as *mut u8,
                    size,
                );
            }
        }
        if trace {
            s.power.dma.as_ref().unwrap().report_completion();
            scarlet::println!(
                "gm20b: submit={} ops={} upload_us={} execute_us={} copied={}",
                sequence,
                prepared.operations.len(),
                uploaded.saturating_sub(started) / 1000,
                time::current_time_ns().saturating_sub(uploaded) / 1000,
                prepared
                    .buffers
                    .iter()
                    .map(|buffer| buffer.bytes.len())
                    .sum::<usize>()
            );
        }
        Ok(())
    }
}

impl GpuBackend for Backend {
    fn query_info(&self) -> GpuBackendInfo {
        let lost = self.shared.state.lock().lost;
        GpuBackendInfo::new(
            GpuDeviceInfo::new(
                if lost {
                    GpuDeviceState::Lost
                } else {
                    GpuDeviceState::Ready
                },
                SUPPORT,
                wire::MAX_SUBMIT_SIZE as u32,
            ),
            0,
            b"nvidia-gm20b",
            &self.snapshot,
        )
    }
    fn query_dialect(&self, index: u32) -> Result<GpuBackendDialectInfo, &'static str> {
        match index {
            0 => Ok(GpuBackendDialectInfo::new(0, DIALECT_TOKEN, DIALECT)),
            1 => Ok(GpuBackendDialectInfo::new(
                1,
                PROGRAMMABLE_DIALECT_TOKEN,
                PROGRAMMABLE_DIALECT,
            )),
            _ => Err("unknown GM20B dialect"),
        }
    }
    fn create_context(
        &self,
        dialect: GpuBackendDialectDescriptor,
    ) -> Result<Arc<dyn GpuBackendContext>, &'static str> {
        if !matches!(
            (dialect.index, dialect.token),
            (0, DIALECT_TOKEN) | (1, PROGRAMMABLE_DIALECT_TOKEN)
        ) {
            return Err("invalid GM20B dialect token");
        }
        if self.shared.state.lock().lost {
            return Err("GM20B device lost");
        }
        Ok(Arc::new(Context {
            shared: self.shared.clone(),
            attachments: Arc::new(Mutex::new(BTreeMap::new())),
            dialect_index: dialect.index,
            dialect_token: dialect.token,
        }))
    }
    fn plan_image(
        &self,
        create: GpuImageCreateInfo,
    ) -> Result<GpuBackendImageLayout, &'static str> {
        let depth = create.format == GPU_IMAGE_FORMAT_DEPTH32_FLOAT;
        if (!depth && create.format != GPU_IMAGE_FORMAT_BGRA8_UNORM)
            || create.usage == 0
            || (depth
                && create.usage
                    & !(GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT | GPU_IMAGE_USAGE_SAMPLED)
                    != 0)
            || (create.cube && (create.array_layers != 6 || create.width != create.height))
            || (create.usage & GPU_IMAGE_USAGE_PRESENTABLE != 0
                && (create.mip_levels != 1 || create.array_layers != 1 || create.cube))
        {
            return Err("unsupported GM20B image descriptor");
        }
        let chain = create.mip_levels > 1 || create.array_layers > 1 || create.cube;
        // Old clients keep their original layouts. Mip/array descriptors use
        // the actual tile height at every LOD, as the texture unit does.
        let tiled = chain
            || depth
            || create.usage & GPU_IMAGE_USAGE_DEPTH_COMPATIBLE != 0
            || (create.width == 1280
                && create.height == 720
                && create.usage & (GPU_IMAGE_USAGE_PRESENTABLE | GPU_IMAGE_USAGE_RENDER_TARGET)
                    == GPU_IMAGE_USAGE_PRESENTABLE | GPU_IMAGE_USAGE_RENDER_TARGET);
        let planned = maxwell_image_layout::plan(
            maxwell_image_layout::Descriptor {
                width: create.width,
                height: create.height,
                mip_levels: create.mip_levels,
                array_layers: create.array_layers,
                bytes_per_pixel: 4,
            },
            if tiled {
                maxwell_image_layout::LayoutKind::BlockLinear {
                    base_y_log2: if chain || create.usage & GPU_IMAGE_USAGE_SAMPLED != 0 {
                        maxwell_image_layout::tile_y_log2(create.height, 4)
                    } else {
                        4
                    },
                    clamp_mips: chain,
                }
            } else {
                maxwell_image_layout::LayoutKind::Linear
            },
        )
        .map_err(|_| "GM20B image layout exceeds checked bounds")?;
        let mut planes = [GpuBackendImagePlaneLayout::EMPTY; GPU_IMAGE_MAX_PLANES];
        planes[0] = GpuBackendImagePlaneLayout {
            offset: 0,
            size: planned.total_size,
            row_pitch: planned.levels[0].row_pitch,
            array_pitch: planned.array_pitch,
            block_width: 1,
            block_height: 1,
            bytes_per_block: 4,
        };
        Ok(GpuBackendImageLayout {
            modifier: if tiled {
                (if depth {
                    maxwell_image_layout::NVIDIA_DEPTH_MODIFIER_BASE
                } else {
                    maxwell_image_layout::NVIDIA_COLOR_MODIFIER_BASE
                }) | u64::from(planned.levels[0].tile_y_log2)
            } else {
                GPU_IMAGE_MODIFIER_LINEAR
            },
            total_size: planned.total_size,
            // Generic backing is linear CPU staging; the private GPU pages
            // and VA retain their independent 8192-byte alignment.
            alignment: 4096,
            plane_count: 1,
            planes,
        })
    }
    fn create_image_with_layout(
        &self,
        create: GpuImageCreateInfo,
        layout: GpuBackendImageLayout,
        backing: GpuImageBackingInfo,
    ) -> Result<Arc<dyn GpuBackendImage>, &'static str> {
        if self.plan_image(create)? != layout
            || !backing.is_physically_contiguous()
            || backing.allocation_size < layout.total_size
        {
            return Err("GM20B image backing/layout mismatch");
        }
        let mut s = self.shared.state.lock();
        let (id, memory) = s.allocate(
            backing.allocation_size,
            Kind::Image {
                create,
                layout,
                backing,
            },
        )?;
        s.power
            .dma
            .as_mut()
            .unwrap()
            .objects
            .insert(id, memory.clone());
        drop(s);
        Ok(Arc::new(Image {
            shared: self.shared.clone(),
            id,
            memory,
        }))
    }
    fn import_shared_image(
        &self,
        image: Arc<SharedImage>,
        color: ImageColor,
    ) -> Result<(Arc<dyn GpuBackendImage>, GpuBackendImageLayout), &'static str> {
        let (id, memory, layout) = self.shared.state.lock().import_image(image, color)?;
        Ok((
            Arc::new(Image {
                shared: self.shared.clone(),
                id,
                memory,
            }),
            layout,
        ))
    }

    fn create_buffer(
        &self,
        create: GpuBufferCreateInfo,
    ) -> Result<Arc<dyn GpuBackendBuffer>, &'static str> {
        let mut s = self.shared.state.lock();
        let (id, memory) = s.allocate(
            create.allocation_size,
            Kind::Buffer {
                paddr: create.paddr,
            },
        )?;
        s.power
            .dma
            .as_mut()
            .unwrap()
            .objects
            .insert(id, memory.clone());
        drop(s);
        Ok(Arc::new(Buffer {
            shared: self.shared.clone(),
            id,
            memory,
        }))
    }
}

fn validate(
    decoded: &wire::DecodedSubmit<'_>,
    attached: &BTreeMap<u64, Attachment>,
    buffers: &[BufferSnapshot],
    programmable: &mut Vec<crate::programmable_graphics::PreparedDraw>,
    allow_programmable: bool,
) -> Result<Vec<[u32; 96]>, &'static str> {
    if decoded.commands_len() % wire::OPERATION_WORDS != 0
        || decoded.commands_len() > wire::MAX_COMMAND_WORDS
    {
        return Err("canonical operation count invalid");
    }
    let mut operations = Vec::new();
    operations
        .try_reserve_exact(decoded.commands_len() / 64)
        .map_err(|_| "operation allocation failed")?;
    let mut relocation = 0;
    let mut viewport = crate::method::FixedViewportState::new();
    for base in (0..decoded.commands_len()).step_by(64) {
        let mut w = [0; 96];
        for (i, word) in w[..64].iter_mut().enumerate() {
            *word = decoded.commands_word(base + i).unwrap();
        }
        viewport.validate_operation(&w[..64], allow_programmable)?;
        if matches!(w[0], 5 | 6 | 7 | 8) && !allow_programmable {
            return Err("extended operations require GM20B v2 dialect");
        }
        if w[0] == 5 {
            if w[1] != 0 || w[2..4].iter().any(|&v| v != 0) || w[6..64].iter().any(|&v| v != 0) {
                return Err("programmable draw envelope reserved words nonzero");
            }
            let size = u64::from(w[4]) | u64::from(w[5]) << 32;
            let r = decoded
                .relocation(relocation)
                .ok_or("programmable metadata reference missing")?;
            if r.commands_word_offset as usize != base + 2
                || r.access != wire::ACCESS_READ
                || r.encoding != wire::AddressEncoding::GpuVa64
                || r.required_size != size
                || size < maxwell_program_wire::draw::HEADER_SIZE as u64
                || size > 4 * 1024 * 1024
            {
                return Err("programmable metadata reference invalid");
            }
            let wire::RelocationSource::Attachment(index) = r.source else {
                return Err("programmable metadata requires attached buffer");
            };
            let resource = decoded
                .resource(index as usize)
                .ok_or("programmable metadata resource missing")?;
            let offset = resource
                .range_offset
                .checked_add(r.resource_offset)
                .ok_or("programmable metadata offset overflow")?;
            let authority = ProgrammableAuthority {
                decoded,
                attached,
                buffers,
            };
            let metadata = authority.buffer_snapshot(maxwell_program_wire::draw::Range {
                token: resource.attachment_token,
                offset,
                size,
            })?;
            let draw = crate::programmable_graphics::prepare(metadata, &authority)?;
            programmable
                .try_reserve(1)
                .map_err(|_| "programmable draw allocation failed")?;
            let mut operation = [0u32; 96];
            operation[0] = 5;
            operation[2] = programmable.len() as u32;
            programmable.push(draw);
            operations.push(operation);
            relocation += 1;
            continue;
        }
        if w[0] == 6 {
            if w[1..64].iter().any(|&v| v != 0) {
                return Err("image command envelope reserved words nonzero");
            }
            operations.push(prepare_image_command(
                decoded,
                attached,
                buffers,
                base,
                &mut relocation,
            )?);
            continue;
        }
        if matches!(w[0], 7 | 8) {
            operations.push(w);
            continue;
        }
        let has_depth = w[0] == 2 && w[60] != 0;
        let has_sample = w[0] == 2 && w[29] != 0;
        if (!has_sample && w[1] != 0)
            || (!has_sample && w[62..64].iter().any(|&v| v != 0))
            || (!has_depth && w[54..62].iter().any(|&v| v != 0))
            || (matches!(w[0], 1 | 4) && w[53] != 0)
            || w[60] > 8
            || w[61] > 1
        {
            return Err("canonical reserved words nonzero");
        }
        let mut roles = [None, None, None, None, None];
        let target_access = if w[0] == 2 {
            wire::ACCESS_READ | wire::ACCESS_WRITE
        } else {
            wire::ACCESS_WRITE
        };
        let fields: &[(usize, u32)] = match w[0] {
            1 | 4 => &[(2, target_access)],
            2 => &[
                (2, target_access),
                (4, wire::ACCESS_READ),
                (6, wire::ACCESS_READ),
                (8, wire::ACCESS_READ),
                (
                    54,
                    if w[61] != 0 {
                        wire::ACCESS_READ | wire::ACCESS_WRITE
                    } else {
                        wire::ACCESS_READ
                    },
                ),
            ],
            3 => &[(2, wire::ACCESS_WRITE), (4, wire::ACCESS_READ)],
            _ => return Err("canonical opcode unsupported"),
        };
        for &(field, access) in fields {
            let present = field == 2
                || field == 4
                || field == 6 && w[29] != 0
                || field == 8 && w[26] != 0
                || field == 54 && has_depth;
            if !present {
                if w[field] != 0 || w[field + 1] != 0 {
                    return Err("unused address nonzero");
                }
                continue;
            }
            let r = decoded
                .relocation(relocation)
                .ok_or("missing canonical object reference")?;
            if r.commands_word_offset as usize != base + field
                || r.access != access
                || r.encoding != wire::AddressEncoding::GpuVa64
            {
                return Err("object reference role mismatch");
            }
            let wire::RelocationSource::Attachment(index) = r.source else {
                return Err("userspace cannot address canonical programs");
            };
            let resource = decoded
                .resource(index as usize)
                .ok_or("object reference resource invalid")?;
            let a = attached
                .get(&resource.attachment_token)
                .ok_or("attachment generation invalid")?;
            let offset = resource
                .range_offset
                .checked_add(r.resource_offset)
                .ok_or("object offset overflow")?;
            let end = offset
                .checked_add(r.required_size)
                .ok_or("object range overflow")?;
            if end > a.memory.size {
                return Err("object reference outside backing");
            }
            let address = a.memory.va as u64 + offset;
            w[field] = address as u32;
            w[field + 1] = (address >> 32) as u32;
            roles[if field == 54 { 4 } else { (field - 2) / 2 }] =
                Some((&a.memory, offset, r.required_size));
            relocation += 1;
        }
        let target = roles[0].ok_or("missing target")?;
        {
            surface(
                target,
                w[10],
                w[11],
                w[12],
                w[52],
                if w[0] == 4 {
                    GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT
                } else if w[0] == 3 {
                    GPU_IMAGE_USAGE_TRANSFER_DST
                } else {
                    GPU_IMAGE_USAGE_RENDER_TARGET
                },
            )?;
            rectangle(&w[13..17], w[10], w[11])?;
        }
        match w[0] {
            4 => {
                let value = f32::from_bits(w[32]);
                if w[4..10]
                    .iter()
                    .chain(w[17..32].iter())
                    .chain(w[33..52].iter())
                    .any(|&v| v != 0)
                    || !value.is_finite()
                    || !(0.0..=1.0).contains(&value)
                {
                    return Err("depth clear record invalid");
                }
            }
            1 => {
                if w[4..10]
                    .iter()
                    .chain(w[17..32].iter())
                    .chain(w[36..52].iter())
                    .any(|&v| v != 0)
                    || w[32..36].iter().any(|&v| !f32::from_bits(v).is_finite())
                {
                    return Err("clear record invalid");
                }
            }
            2 => {
                if let Some(depth) = roles[4] {
                    if w[52] != 0x40 && w[52] & 0x100 == 0
                        || w[56] != w[10]
                        || w[57] != w[11]
                        || depth.0.va == target.0.va
                    {
                        return Err("depth/color target mismatch");
                    }
                    surface(
                        depth,
                        w[56],
                        w[57],
                        w[58],
                        w[59],
                        GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT,
                    )?;
                }
                let variant = PipelineVariant::from_raw(w[21]).ok_or("unknown shader pair")?;
                let texture = matches!(
                    variant,
                    PipelineVariant::Stride16TextureRgba
                        | PipelineVariant::Stride16TextureAlphaMask
                        | PipelineVariant::Stride40TextureVertexColorRgba
                        | PipelineVariant::Stride24TextureRgba
                        | PipelineVariant::Stride24TextureRgbIgnoreAlpha
                        | PipelineVariant::Stride24TextureAlphaMask
                );
                if texture != (roles[2].is_some())
                    || w[23] != variant.stride()
                    || !crate::method::draw_state_valid(w[22])
                    || w[25] == 0
                    || w[25] % 3 != 0
                    || w[26] > 2
                    || w[32..52].iter().any(|&v| !f32::from_bits(v).is_finite())
                {
                    return Err("draw record invalid");
                }
                rectangle(&w[17..21], w[10], w[11])?;
                if w[17] < w[13]
                    || w[18] < w[14]
                    || w[17] + w[19] > w[13] + w[15]
                    || w[18] + w[20] > w[14] + w[16]
                {
                    return Err("scissor exceeds render area");
                }
                let vertex = roles[1].ok_or("missing vertex buffer")?;
                if !matches!(vertex.0.kind, Kind::Buffer { .. })
                    || vertex.1 % 4 != 0
                    || vertex.2 != u64::from(w[28])
                    || vertex.2 < u64::from(w[23])
                {
                    return Err("vertex binding invalid");
                }
                if let Some(t) = roles[2] {
                    let (create, shared) = match &t.0.kind {
                        Kind::Image { create, .. } => (create, false),
                        Kind::SharedImage { create, .. } => (create, true),
                        _ => return Err("sampled binding is not an image"),
                    };
                    let format = w[62] & 0xf;
                    let min_lod = f32::from_bits(w[1]);
                    let max_lod = f32::from_bits(w[63]);
                    if w[62] & !0xf0f != 0
                        || format > 7
                        || ((w[62] >> 8) & 0xf) + 1 != create.mip_levels
                        || create.array_layers != 1
                        || create.cube
                        || !min_lod.is_finite()
                        || !max_lod.is_finite()
                        || min_lod < 0.0
                        || max_lod < min_lod
                        || (shared && !matches!(format, 0 | 7))
                        || (!shared
                            && ((format == 6) != (create.format == GPU_IMAGE_FORMAT_DEPTH32_FLOAT)
                                || format == 7))
                        || (w[22] & (1 << 5) != 0 && !matches!(format, 0 | 2))
                    {
                        return Err("sampled image metadata invalid");
                    }
                    surface(t, w[29], w[30], w[31], w[53], GPU_IMAGE_USAGE_SAMPLED)?;
                    if let Kind::SharedImage {
                        image,
                        layout,
                        color,
                        ..
                    } = &t.0.kind
                    {
                        if !matches!(
                            variant,
                            PipelineVariant::Stride16TextureRgba
                                | PipelineVariant::Stride24TextureRgba
                                | PipelineVariant::Stride24TextureRgbIgnoreAlpha
                                | PipelineVariant::Stride40TextureVertexColorRgba
                        ) || w[22] & (1 << 5) != 0
                        {
                            return Err("NV12 requires an RGB sampling program");
                        }
                        let d = image.descriptor();
                        let y = t.0.va as u64 + layout.planes[0].offset;
                        let uv = t.0.va as u64 + layout.planes[1].offset;
                        w[6] = y as u32;
                        w[7] = (y >> 32) as u32;
                        w[64] = 1;
                        w[65] = if d.modifier == 0 {
                            d.width
                        } else {
                            layout.planes[0].row_pitch
                        };
                        w[66] = d.height;
                        w[67] = uv as u32;
                        w[68] = (uv >> 32) as u32;
                        w[69] = layout.planes[1].row_pitch;
                        w[70] = if d.modifier == 0 {
                            d.width.div_ceil(2)
                        } else {
                            layout.planes[1].row_pitch / 2
                        };
                        w[71] = d.height.div_ceil(2);
                        w[72..92].copy_from_slice(&ycbcr_uniforms(d, *color));
                    }
                    if t.0.va == target.0.va {
                        return Err("sampled/render target alias forbidden");
                    }
                } else if w[29..32].iter().any(|&v| v != 0)
                    || w[53] != 0
                    || w[22] & (crate::method::DRAW_SAMPLER_MASK | (1 << 5)) != 0
                {
                    return Err("unused texture state nonzero");
                }
                if w[26] == 0 {
                    if w[27] != 0
                        || u64::from(w[24])
                            .checked_add(u64::from(w[25]))
                            .and_then(|n| n.checked_mul(u64::from(w[23])))
                            .is_none_or(|bytes| bytes > vertex.2)
                    {
                        return Err("vertex draw out of range");
                    }
                } else {
                    let index = roles[3].ok_or("missing index buffer")?;
                    let element = if w[26] == 1 { 2 } else { 4 };
                    let end = w[24].checked_add(w[25]).ok_or("index count overflow")?;
                    if !matches!(index.0.kind, Kind::Buffer { .. })
                        || index.1 % element != 0
                        || u64::from(end) * element > index.2
                    {
                        return Err("index binding out of range");
                    }
                    let snapshot = buffers
                        .iter()
                        .find(|buffer| {
                            buffer.span.memory.va == index.0.va
                                && buffer.span.start <= index.1 as usize
                                && buffer.span.end >= (index.1 + u64::from(end) * element) as usize
                        })
                        .ok_or("index range missing immutable snapshot")?;
                    let p =
                        snapshot.bytes.as_ptr() as usize + index.1 as usize - snapshot.span.start;
                    for i in w[24]..end {
                        let value = if element == 2 {
                            unsafe {
                                core::ptr::read_unaligned((p + i as usize * 2) as *const u16) as u32
                            }
                        } else {
                            unsafe { core::ptr::read_unaligned((p + i as usize * 4) as *const u32) }
                        };
                        if !crate::method::indexed_vertex_in_bounds(
                            value,
                            w[27] as i32,
                            w[23],
                            vertex.2,
                        ) {
                            return Err("index references vertex outside authorized buffer");
                        }
                    }
                }
            }
            3 => {
                let source = roles[1].ok_or("copy source missing")?;
                surface(
                    source,
                    w[29],
                    w[30],
                    w[31],
                    w[53],
                    GPU_IMAGE_USAGE_TRANSFER_SRC,
                )?;
                rectangle(&w[17..21], w[29], w[30])?;
                if target.0.va == source.0.va
                    || w[15] != w[19]
                    || w[16] != w[20]
                    || w[6..10]
                        .iter()
                        .chain(w[21..29].iter())
                        .chain(w[32..52].iter())
                        .any(|&v| v != 0)
                {
                    return Err("copy record invalid");
                }
            }
            _ => unreachable!(),
        }
        operations.push(w);
    }
    viewport.finish()?;
    if relocation != decoded.relocation_len() {
        return Err("extra object references rejected");
    }
    Ok(operations)
}
struct ProgrammableAuthority<'a, 'b> {
    decoded: &'a wire::DecodedSubmit<'b>,
    attached: &'a BTreeMap<u64, Attachment>,
    buffers: &'a [BufferSnapshot],
}
impl ProgrammableAuthority<'_, '_> {
    fn authorize(
        &self,
        token: u64,
        offset: u64,
        size: u64,
        access: u32,
    ) -> Result<&Arc<Memory>, &'static str> {
        let end = offset
            .checked_add(size)
            .ok_or("programmable resource range overflow")?;
        let memory = &self
            .attached
            .get(&token)
            .ok_or("programmable attachment unavailable")?
            .memory;
        if size == 0 || end > memory.size {
            return Err("programmable resource exceeds attachment");
        }
        if !(0..self.decoded.resource_len())
            .filter_map(|i| self.decoded.resource(i))
            .any(|r| {
                r.attachment_token == token
                    && r.access & access == access
                    && r.range_offset <= offset
                    && r.range_offset
                        .checked_add(r.range_size)
                        .is_some_and(|limit| end <= limit)
            })
        {
            return Err("programmable resource view access not declared");
        }
        Ok(memory)
    }
    fn buffer_snapshot(
        &self,
        range: maxwell_program_wire::draw::Range,
    ) -> Result<&[u8], &'static str> {
        let memory = self.authorize(range.token, range.offset, range.size, wire::ACCESS_READ)?;
        if !matches!(memory.kind, Kind::Buffer { .. }) {
            return Err("programmable range is not a buffer");
        }
        let end = range
            .offset
            .checked_add(range.size)
            .ok_or("programmable snapshot range overflow")?;
        let snapshot = self
            .buffers
            .iter()
            .find(|s| {
                Arc::ptr_eq(&s.span.memory, memory)
                    && s.span.start as u64 <= range.offset
                    && end <= s.span.end as u64
            })
            .ok_or("programmable buffer immutable snapshot missing")?;
        let start = (range.offset - snapshot.span.start as u64) as usize;
        Ok(&snapshot.bytes[start..start + range.size as usize])
    }
}
impl crate::programmable_graphics::Authority for ProgrammableAuthority<'_, '_> {
    fn buffer_bytes(
        &self,
        range: maxwell_program_wire::draw::Range,
    ) -> Result<&[u8], &'static str> {
        self.buffer_snapshot(range)
    }
    fn buffer_address(
        &self,
        range: maxwell_program_wire::draw::Range,
    ) -> Result<u64, &'static str> {
        let memory = self.authorize(range.token, range.offset, range.size, wire::ACCESS_READ)?;
        if !matches!(memory.kind, Kind::Buffer { .. }) {
            return Err("programmable range is not a buffer");
        }
        (memory.va as u64)
            .checked_add(range.offset)
            .ok_or("programmable buffer address overflow")
    }
    fn image(
        &self,
        token: u64,
        level: u32,
        level_count: u32,
        layer: u32,
        layer_count: u32,
        access: crate::programmable_graphics::ImageAccess,
    ) -> Result<crate::programmable_graphics::ImageView, &'static str> {
        use crate::programmable_graphics::{ImageAccess, ImageView};
        let memory = &self
            .attached
            .get(&token)
            .ok_or("programmable image attachment missing")?
            .memory;
        let Kind::Image { create, layout, .. } = memory.kind else {
            return Err("programmable attachment is not a native image");
        };
        let (usage, rights) = match access {
            ImageAccess::Sample => (GPU_IMAGE_USAGE_SAMPLED, wire::ACCESS_READ),
            ImageAccess::ColorTarget => (
                GPU_IMAGE_USAGE_RENDER_TARGET,
                wire::ACCESS_READ | wire::ACCESS_WRITE,
            ),
            ImageAccess::Depth { write } => (
                GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT,
                wire::ACCESS_READ | if write { wire::ACCESS_WRITE } else { 0 },
            ),
        };
        if create.usage & usage == 0
            || level_count == 0
            || layer_count == 0
            || level
                .checked_add(level_count)
                .is_none_or(|n| n > create.mip_levels)
            || layer
                .checked_add(layer_count)
                .is_none_or(|n| n > create.array_layers)
        {
            return Err("programmable image view/usage invalid");
        }
        let chain = image_subresources(create, layout)?;
        let first = chain
            .subresource(level, layer)
            .map_err(|_| "programmable image first subresource invalid")?;
        let last = chain
            .subresource(level + level_count - 1, layer + layer_count - 1)
            .map_err(|_| "programmable image last subresource invalid")?;
        let end = last
            .offset
            .checked_add(last.level.size)
            .ok_or("programmable image range overflow")?;
        self.authorize(token, first.offset, end - first.offset, rights)?;
        let address = (memory.va as u64)
            .checked_add(first.offset)
            .ok_or("programmable image address overflow")?;
        let physical = if matches!(access, ImageAccess::Sample) {
            chain
                .subresource(0, layer)
                .map_err(|_| "programmable sampled LOD0 invalid")?
        } else {
            first
        };
        let physical_address = (memory.va as u64)
            .checked_add(physical.offset)
            .ok_or("programmable sampled base address overflow")?;
        Ok(ImageView {
            address: physical_address,
            range_address: address,
            span_size: end - first.offset,
            width: physical.level.width,
            height: physical.level.height,
            pitch: physical.level.row_pitch,
            tiled: layout.modifier != GPU_IMAGE_MODIFIER_LINEAR,
            tile_y: physical.level.tile_y_log2,
            array_pitch: chain.array_pitch,
            mip_levels: create.mip_levels,
            layers: layer_count,
            cube: create.cube,
            depth: create.format == GPU_IMAGE_FORMAT_DEPTH32_FLOAT,
        })
    }
}

fn prepare_image_command(
    decoded: &wire::DecodedSubmit<'_>,
    attached: &BTreeMap<u64, Attachment>,
    buffers: &[BufferSnapshot],
    base: usize,
    relocation: &mut usize,
) -> Result<[u32; 96], &'static str> {
    use crate::image_commands::{BufferAuthority, ImageAuthority, SourceAuthority};
    use maxwell_image_layout::wire::{Command, Opcode, RECORD_SIZE};
    let r = decoded
        .relocation(*relocation)
        .ok_or("image command metadata reference missing")?;
    if r.commands_word_offset as usize != base + 2
        || r.access != wire::ACCESS_READ
        || r.encoding != wire::AddressEncoding::GpuVa64
        || r.required_size != RECORD_SIZE as u64
    {
        return Err("image command metadata reference invalid");
    }
    let wire::RelocationSource::Attachment(index) = r.source else {
        return Err("image command metadata requires an attached buffer");
    };
    let record = decoded
        .resource(index as usize)
        .ok_or("image command metadata resource missing")?;
    if record.access & wire::ACCESS_READ == 0 {
        return Err("image command metadata is not readable");
    }
    let meta = &attached
        .get(&record.attachment_token)
        .ok_or("image command metadata attachment missing")?
        .memory;
    if !matches!(meta.kind, Kind::Buffer { .. }) {
        return Err("image command metadata is not a buffer");
    }
    let offset = record
        .range_offset
        .checked_add(r.resource_offset)
        .ok_or("image command metadata offset overflow")?;
    let end = offset
        .checked_add(RECORD_SIZE as u64)
        .ok_or("image command metadata end overflow")?;
    if end > meta.size {
        return Err("image command metadata outside backing");
    }
    let snapshot = buffers
        .iter()
        .find(|s| {
            Arc::ptr_eq(&s.span.memory, meta)
                && s.span.start as u64 <= offset
                && end <= s.span.end as u64
        })
        .ok_or("image command metadata snapshot missing")?;
    let begin = usize::try_from(offset - snapshot.span.start as u64)
        .map_err(|_| "image command metadata offset too large")?;
    let command = Command::decode(&snapshot.bytes[begin..begin + RECORD_SIZE])
        .map_err(|_| "invalid image command metadata")?;
    let declared = |token, access| -> Result<(&Arc<Memory>, u64, u64), &'static str> {
        let (offset, size) = crate::image_commands::declared_range(
            (0..decoded.resource_len())
                .filter_map(|i| decoded.resource(i))
                .map(|r| (r.attachment_token, r.access, r.range_offset, r.range_size)),
            token,
            access,
        )?;
        let memory = &attached
            .get(&token)
            .ok_or("image command attachment missing")?
            .memory;
        if offset + size > memory.size {
            return Err("image command authority exceeds backing");
        }
        Ok((memory, offset, size))
    };
    let (memory, range_offset, range_size) =
        declared(command.destination_attachment, wire::ACCESS_WRITE)?;
    let Kind::Image { create, layout, .. } = memory.kind else {
        return Err("image command destination is not an image");
    };
    let destination = ImageAuthority {
        create,
        layout,
        va: memory.va as u64,
        range_offset,
        range_size,
    };
    let (memory, range_offset, range_size) =
        declared(command.source_attachment, wire::ACCESS_READ)?;
    let source = match command.opcode {
        Opcode::Upload => {
            if !matches!(memory.kind, Kind::Buffer { .. }) {
                return Err("image upload source is not a buffer");
            }
            SourceAuthority::Buffer(BufferAuthority {
                va: memory.va as u64,
                range_offset,
                range_size,
            })
        }
        Opcode::Blit => {
            let Kind::Image { create, layout, .. } = memory.kind else {
                return Err("image blit source is not an image");
            };
            SourceAuthority::Image(ImageAuthority {
                create,
                layout,
                va: memory.va as u64,
                range_offset,
                range_size,
            })
        }
    };
    let operation = crate::image_commands::lower(command, destination, source)?;
    *relocation += 1;
    Ok(operation)
}

fn surface(
    binding: (&Arc<Memory>, u64, u64),
    width: u32,
    height: u32,
    pitch: u32,
    tile_mode: u32,
    usage: u32,
) -> Result<(), &'static str> {
    if let Kind::SharedImage { create, layout, .. } = &binding.0.kind {
        if usage != GPU_IMAGE_USAGE_SAMPLED
            || create.width != width
            || create.height != height
            || layout.planes[0].row_pitch != pitch
            || binding.1 != 0
            || binding.2 < layout.total_size
            || tile_mode != if layout.modifier == 0 { 0 } else { 0x10 }
        {
            return Err("shared sampled image layout/usage mismatch");
        }
        return Ok(());
    }
    let Kind::Image { create, layout, .. } = &binding.0.kind else {
        return Err("surface requires image");
    };
    let depth = usage == GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT
        || (usage == GPU_IMAGE_USAGE_SAMPLED && create.format == GPU_IMAGE_FORMAT_DEPTH32_FLOAT);
    if create.format
        != if depth {
            GPU_IMAGE_FORMAT_DEPTH32_FLOAT
        } else {
            GPU_IMAGE_FORMAT_BGRA8_UNORM
        }
        || (depth && layout.modifier & !0xf != maxwell_image_layout::NVIDIA_DEPTH_MODIFIER_BASE)
        || create.width != width
        || create.height != height
        || layout.planes[0].row_pitch != pitch
        || tile_mode
            != if let Some(y) = maxwell_image_layout::modifier_tile_y(layout.modifier) {
                (u32::from(y) << 4)
                    | if create.mip_levels > 1
                        || create.array_layers > 1
                        || (layout.modifier != GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4
                            && layout.modifier
                                != GPU_IMAGE_MODIFIER_NVIDIA_ZF32_BLOCK_LINEAR_16BX2_H4)
                    {
                        0x100
                    } else {
                        0
                    }
            } else if layout.modifier == GPU_IMAGE_MODIFIER_LINEAR {
                0
            } else {
                return Err("unsupported surface modifier");
            }
        || create.usage & usage != usage
        || binding.1 != layout.planes[0].offset
        || binding.2 < layout.planes[0].size
    {
        return Err("surface layout/usage mismatch");
    }
    Ok(())
}
fn rectangle(rect: &[u32], width: u32, height: u32) -> Result<(), &'static str> {
    if rect[2] == 0
        || rect[3] == 0
        || rect[0].checked_add(rect[2]).is_none_or(|end| end > width)
        || rect[1].checked_add(rect[3]).is_none_or(|end| end > height)
    {
        return Err("rectangle out of range");
    }
    Ok(())
}

/// UV transforms and encoded RGB conversion, derived only from validated metadata.
pub(super) fn ycbcr_uniforms(d: SharedImageDescriptor, c: ImageColor) -> [u32; 20] {
    // A tiled TIC derives its pitch from its width. NVDEC may pad rows more
    // than texture alignment requires, so describe the full stored row.
    let w = if d.modifier == 0 {
        d.width
    } else {
        d.planes[0].row_pitch
    } as f32;
    let cw = if d.modifier == 0 {
        d.width.div_ceil(2) * 2
    } else {
        d.planes[1].row_pitch
    } as f32;
    let h = d.height as f32;
    let ch = d.height.div_ceil(2) as f32 * 2.0;
    let r = d.visible;
    let mut out = [0f32; 20];
    out[..8].copy_from_slice(&[
        r.width as f32 / w,
        r.height as f32 / h,
        r.x as f32 / w,
        r.y as f32 / h,
        r.width as f32 / cw,
        r.height as f32 / ch,
        (r.x as f32
            + if c.chroma_x == CHROMA_COSITED {
                0.5
            } else {
                0.0
            })
            / cw,
        (r.y as f32
            + if c.chroma_y == CHROMA_COSITED {
                0.5
            } else {
                0.0
            })
            / ch,
    ]);
    let (kr, kb) = if c.matrix == COLOR_MATRIX_BT709 {
        (0.2126, 0.0722)
    } else {
        (0.299, 0.114)
    };
    let kg = 1.0 - kr - kb;
    let (ys, cs, yo) = if c.range == COLOR_RANGE_LIMITED {
        (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0)
    } else {
        (1.0, 1.0, 0.0)
    };
    let rows = [
        [ys, 0.0, 2.0 * (1.0 - kr) * cs],
        [
            ys,
            -2.0 * kb * (1.0 - kb) / kg * cs,
            -2.0 * kr * (1.0 - kr) / kg * cs,
        ],
        [ys, 2.0 * (1.0 - kb) * cs, 0.0],
    ];
    for (i, row) in rows.into_iter().enumerate() {
        out[8 + i * 4..11 + i * 4].copy_from_slice(&row);
        out[11 + i * 4] = -row[0] * yo - (row[1] + row[2]) * (128.0 / 255.0);
    }
    out.map(f32::to_bits)
}

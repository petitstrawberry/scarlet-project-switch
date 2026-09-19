// SPDX-License-Identifier: GPL-2.0-only
//! Capability-authorized SGFX queue and independent GPU DMA backing.
//! Like the Chromebook backend, object identities are separate from attachment
//! tokens. User bytes can never select a GPU method, physical address or shader.

use crate::{
    gmmu::{VA_LIMIT, clean, pages, pages_aligned},
    runtime::Power,
};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use maxwell_shader_pack::PipelineVariant;
use maxwell_submit_wire as wire;
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
const SUPPORT: u32 = GPU_EXECUTION_SUPPORT_ADDRESS_SPACE
    | GPU_EXECUTION_SUPPORT_MEMORY
    | GPU_EXECUTION_SUPPORT_QUEUE
    | GPU_EXECUTION_SUPPORT_TIMELINE
    | GPU_EXECUTION_SUPPORT_PRESENTATION
    | GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD
    | GPU_EXECUTION_SUPPORT_IMAGE_READBACK;

#[derive(Clone)]
pub(super) enum Kind {
    Buffer {
        paddr: u64,
    },
    Image {
        create: GpuImageCreateInfo,
        layout: GpuBackendImageLayout,
        backing: GpuImageBackingInfo,
    },
}
pub(super) struct Memory {
    pub pages: ContiguousPages,
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
struct Shared {
    state: Mutex<State>,
    utilization: Option<crate::utilization::UtilizationMonitor>,
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
    ) -> Self {
        Self {
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
            }),
            snapshot,
            gpu_base,
        }
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
                if layout.modifier == GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4
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
            va = va.max(start + entry.pages.len() * 4096);
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
                if layout.modifier == GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4 =>
            {
                0xfe
            }
            _ => 0,
        };
        let memory = Arc::new(Memory {
            pages: memory,
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
            .map_private_kind(va, &memory.pages, page_kind)
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
    fn allocation_error(&mut self, size: u64, reason: &'static str) -> &'static str {
        self.diagnostic_allocation_failures = self.diagnostic_allocation_failures.saturating_add(1);
        let count = self.diagnostic_allocation_failures;
        if count <= 4 {
            let retained_bytes: usize = self
                .dma()
                .retained
                .values()
                .map(|memory| memory.pages.len() * 4096)
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
    fn fault(&mut self) {
        self.lost = true;
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
        match s.dma().unmap(memory.va, memory.pages.len()) {
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
        let Kind::Image { create, .. } = &self.memory.kind else {
            unreachable!()
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
        let paddr = self.memory.pages.as_paddr();
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
// A context carries real attachment authority; shader IDs and object IDs never
// grant access. IDs are indexed separately to support repeated attachments.
struct Attachment {
    object: u64,
    memory: Arc<Memory>,
}
#[derive(Clone)]
struct Context {
    shared: Arc<Shared>,
    attachments: Arc<Mutex<BTreeMap<u64, Attachment>>>,
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
        let pitch = layout.planes[0].row_pitch;
        if rect.width == 0
            || rect.height == 0
            || rect.backing_stride != pitch
            || rect.backing_layer_stride != layout.planes[0].array_pitch
            || rect
                .dst_x
                .checked_add(rect.width)
                .is_none_or(|n| n > create.width)
            || rect
                .dst_y
                .checked_add(rect.height)
                .is_none_or(|n| n > create.height)
            || rect.backing_offset
                != u64::from(rect.dst_y) * u64::from(pitch) + u64::from(rect.dst_x) * 4
        {
            return Err("image transfer layout mismatch");
        }
        for y in 0..rect.height {
            let offset = rect.backing_offset as usize + y as usize * pitch as usize;
            let n = rect.width as usize * 4;
            let generic = scarlet::vm::phys_to_virt(backing.paddr + offset as u64);
            let gpu = mem.pages.as_vaddr() + offset;
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
        // DMA retirement is guaranteed by synchronous submit holding this lock.
        // Imported layouts are checked by the same transfer validation.
        Ok(())
    }
}
impl GpuBackendContext for Context {
    fn query_info(&self) -> GpuBackendContextInfo {
        GpuBackendContextInfo::new(0, DIALECT_TOKEN)
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
        if bytes.is_empty() {
            return Ok(());
        }
        let started = time::current_time_ns();
        let decoded = wire::decode(bytes)
            .map_err(|_| GpuBackendSubmitError::Rejected("invalid GM20B submit wire"))?;
        let attached = self.context.attachments.lock();
        let mut s = self.context.shared.state.lock();
        if s.lost {
            return Err(GpuBackendSubmitError::DeviceLost("GM20B device lost"));
        }
        s.diagnostic_submissions = s.diagnostic_submissions.saturating_add(1);
        let sequence = s.diagnostic_submissions;
        let trace = sequence <= 4;
        let acquired = time::current_time_ns();
        let mut copied_bytes = 0u64;
        let mut referenced_bytes = 0u64;
        // Snapshot only the authorized ranges used by this submission, not
        // the capacity reserved for future frames. The wire decoder checks
        // every relocation lies inside its declared resource range.
        // Keep independently retained GPU backing even for a small range.
        // Validate indices from this snapshot, so a mutable user alias cannot
        // race validation and cause a GPU fetch outside its authorized VBO.
        for i in 0..decoded.resource_len() {
            let resource = decoded.resource(i).unwrap();
            let a = attached
                .get(&resource.attachment_token)
                .ok_or(GpuBackendSubmitError::Rejected("unauthorized attachment"))?;
            if resource
                .range_offset
                .checked_add(resource.range_size)
                .is_none_or(|end| end > a.memory.size)
            {
                return Err(GpuBackendSubmitError::Rejected(
                    "resource range exceeds attachment",
                ));
            }
            if let Kind::Buffer { paddr } = &a.memory.kind {
                let offset = resource.range_offset as usize;
                let size = resource.range_size as usize;
                let destination = a.memory.pages.as_vaddr() + offset;
                copied_bytes += resource.range_size;
                referenced_bytes += resource.range_size;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (scarlet::vm::phys_to_virt(*paddr) + offset) as *const u8,
                        destination as *mut u8,
                        size,
                    );
                }
                arch::clean_dcache_to_poc_range(destination, size);
            }
        }
        let snapshotted = time::current_time_ns();
        let operations = validate(&decoded, &attached).map_err(|error| {
            if trace {
                scarlet::println!("gm20b: submit={} rejected: {}", sequence, error);
            }
            GpuBackendSubmitError::Rejected(error)
        })?;
        let validated = time::current_time_ns();
        if trace {
            let draws = operations.iter().filter(|w| w[0] == 2).count();
            scarlet::println!(
                "gm20b: submit={} ops={} draws={} objects={}",
                sequence,
                operations.len(),
                draws,
                decoded.resource_len()
            );
        }
        if let Err(error) = s.power.dma.as_mut().unwrap().execute_graphics(&operations) {
            if trace {
                scarlet::println!("gm20b: submit={} execution failed: {:?}", sequence, error);
            }
            if matches!(error, GpuBackendSubmitError::DeviceLost(_)) {
                s.fault();
            }
            return Err(error);
        }
        if trace {
            let retired = time::current_time_ns();
            scarlet::println!(
                "gm20b: submit={} retired wait_us={} snapshot_us={} validate_us={} execute_us={} copied={} referenced={}",
                sequence,
                acquired.saturating_sub(started) / 1000,
                snapshotted.saturating_sub(acquired) / 1000,
                validated.saturating_sub(snapshotted) / 1000,
                retired.saturating_sub(validated) / 1000,
                copied_bytes,
                referenced_bytes
            );
        }
        // Written buffer uploads are reflected back only after real GPU retire.
        for i in 0..decoded.resource_len() {
            let resource = decoded.resource(i).unwrap();
            if resource.access & wire::ACCESS_WRITE == 0 {
                continue;
            }
            let a = &attached[&resource.attachment_token];
            if let Kind::Buffer { paddr } = &a.memory.kind {
                let offset = resource.range_offset as usize;
                let size = resource.range_size as usize;
                let source = a.memory.pages.as_vaddr() + offset;
                arch::invalidate_dcache_to_poc_range(source, size);
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        source as *const u8,
                        (scarlet::vm::phys_to_virt(*paddr) + offset) as *mut u8,
                        size,
                    );
                }
            }
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
        if index != 0 {
            return Err("unknown GM20B dialect");
        }
        Ok(GpuBackendDialectInfo::new(0, DIALECT_TOKEN, DIALECT))
    }
    fn create_context(
        &self,
        dialect: GpuBackendDialectDescriptor,
    ) -> Result<Arc<dyn GpuBackendContext>, &'static str> {
        if dialect.index != 0 || dialect.token != DIALECT_TOKEN {
            return Err("invalid GM20B dialect token");
        }
        if self.shared.state.lock().lost {
            return Err("GM20B device lost");
        }
        Ok(Arc::new(Context {
            shared: self.shared.clone(),
            attachments: Arc::new(Mutex::new(BTreeMap::new())),
        }))
    }
    fn plan_image(
        &self,
        create: GpuImageCreateInfo,
    ) -> Result<GpuBackendImageLayout, &'static str> {
        if create.format != GPU_IMAGE_FORMAT_BGRA8_UNORM
            || create.usage & GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT != 0
            || create.width > 16384
            || create.height > 16384
        {
            return Err("GM20B requires BGRA8 color images");
        }
        if create.width == 1280
            && create.height == 720
            && create.usage & (GPU_IMAGE_USAGE_PRESENTABLE | GPU_IMAGE_USAGE_RENDER_TARGET)
                == GPU_IMAGE_USAGE_PRESENTABLE | GPU_IMAGE_USAGE_RENDER_TARGET
        {
            const PITCH: u32 = 1280 * 4;
            const SIZE: u32 = PITCH * 768; // six 128-row GOB blocks
            let mut planes = [GpuBackendImagePlaneLayout::EMPTY; GPU_IMAGE_MAX_PLANES];
            planes[0] = GpuBackendImagePlaneLayout {
                offset: 0,
                size: SIZE as u64,
                row_pitch: PITCH,
                array_pitch: SIZE,
                block_width: 1,
                block_height: 1,
                bytes_per_block: 4,
            };
            return Ok(GpuBackendImageLayout {
                modifier: GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4,
                total_size: SIZE as u64,
                alignment: 4096,
                plane_count: 1,
                planes,
            });
        }
        let pitch = create
            .width
            .checked_mul(4)
            .and_then(|v| v.checked_add(255))
            .ok_or("image pitch overflow")?
            & !255;
        GpuBackendImageLayout::linear_32bpp(create, pitch, 4096)
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
) -> Result<Vec<[u32; 64]>, &'static str> {
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
    for base in (0..decoded.commands_len()).step_by(64) {
        let mut w = [0; 64];
        for (i, word) in w.iter_mut().enumerate() {
            *word = decoded.commands_word(base + i).unwrap();
        }
        if w[1] != 0 || w[54..].iter().any(|&v| v != 0) || (w[0] == 1 && w[53] != 0) {
            return Err("canonical reserved words nonzero");
        }
        let mut roles = [None, None, None, None];
        let target_access = if w[0] == 2 {
            wire::ACCESS_READ | wire::ACCESS_WRITE
        } else {
            wire::ACCESS_WRITE
        };
        let fields: &[(usize, u32)] = match w[0] {
            1 => &[(2, target_access)],
            2 => &[
                (2, target_access),
                (4, wire::ACCESS_READ),
                (6, wire::ACCESS_READ),
                (8, wire::ACCESS_READ),
            ],
            3 => &[(2, wire::ACCESS_WRITE), (4, wire::ACCESS_READ)],
            _ => return Err("canonical opcode unsupported"),
        };
        for &(field, access) in fields {
            let present =
                field == 2 || field == 4 || field == 6 && w[29] != 0 || field == 8 && w[26] != 0;
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
            roles[(field - 2) / 2] = Some((&a.memory, offset, r.required_size));
            relocation += 1;
        }
        let target = roles[0].ok_or("missing target")?;
        if w[0] != 4 {
            surface(
                target,
                w[10],
                w[11],
                w[12],
                w[52],
                if w[0] == 3 {
                    GPU_IMAGE_USAGE_TRANSFER_DST
                } else {
                    GPU_IMAGE_USAGE_RENDER_TARGET
                },
            )?;
            rectangle(&w[13..17], w[10], w[11])?;
        }
        match w[0] {
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
                    || w[22] & !0x3f != 0
                    || (w[22] >> 2) & 3 == 3
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
                    surface(t, w[29], w[30], w[31], w[53], GPU_IMAGE_USAGE_SAMPLED)?;
                    if t.0.va == target.0.va {
                        return Err("sampled/render target alias forbidden");
                    }
                } else if w[29..32].iter().any(|&v| v != 0) || w[53] != 0 || w[22] & 0x22 != 0 {
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
                    let p = index.0.pages.as_vaddr() + index.1 as usize;
                    for i in w[24]..end {
                        let value = if element == 2 {
                            unsafe {
                                core::ptr::read_unaligned((p + i as usize * 2) as *const u16) as u32
                            }
                        } else {
                            unsafe { core::ptr::read_unaligned((p + i as usize * 4) as *const u32) }
                        };
                        if u64::from(value)
                            .checked_add(u64::from(w[27]))
                            .and_then(|n| n.checked_add(1))
                            .and_then(|n| n.checked_mul(u64::from(w[23])))
                            .is_none_or(|bytes| bytes > vertex.2)
                        {
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
    if relocation != decoded.relocation_len() {
        return Err("extra object references rejected");
    }
    Ok(operations)
}
fn surface(
    binding: (&Arc<Memory>, u64, u64),
    width: u32,
    height: u32,
    pitch: u32,
    tile_mode: u32,
    usage: u32,
) -> Result<(), &'static str> {
    let Kind::Image { create, layout, .. } = &binding.0.kind else {
        return Err("surface requires image");
    };
    if create.width != width
        || create.height != height
        || layout.planes[0].row_pitch != pitch
        || tile_mode
            != if layout.modifier == GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4 {
                0x40
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

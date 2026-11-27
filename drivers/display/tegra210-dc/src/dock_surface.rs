// SPDX-License-Identifier: GPL-2.0-only
//! The secondary display ABI maps one stable linear surface per mode.
use crate::{external, head::Timing};
use alloc::sync::Arc;
use core::any::Any;
use scarlet::{
    arch,
    device::{
        Device, DeviceType,
        gpu::GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4,
        graphics::{
            FramebufferConfig, GpuDisplayResource, GpuPresentOptions, GraphicsDevice, PixelFormat,
            output::DisplayRegion,
        },
    },
    mem::page::ContiguousPages,
    object::capability::{
        ControlOps, MemoryMappingInfo, MemoryMappingOps, Selectable,
        selectable::{ReadyInterest, SelectWaitOutcome},
    },
    vm::vmem::MemoryAttribute,
};

pub(crate) struct Surface {
    pub mode: Timing,
    config: FramebufferConfig,
    staging: ContiguousPages,
}

pub(crate) struct Plane {
    pub address: u64,
    pub pitch: u32,
    pub kind: u32,
}

impl Surface {
    pub fn new(mode: Timing) -> Result<Arc<Self>, &'static str> {
        let config = FramebufferConfig::new(mode.width, mode.height, PixelFormat::BGRA8888);
        let staging = ContiguousPages::new(config.size().div_ceil(4096))
            .ok_or("DC1 staging allocation failed")?;
        Ok(Arc::new(Self {
            mode,
            config,
            staging,
        }))
    }

    /// DC1 owns the private pages until scanout stops. A stale userspace
    /// mapping pins only its linear staging allocation through a modeset.
    pub fn cpu_plane(
        &self,
        slot: &mut Option<ContiguousPages>,
        upload: bool,
    ) -> Result<Plane, &'static str> {
        let size = self.config.size();
        if slot.is_none() {
            let pages =
                ContiguousPages::new(size.div_ceil(4096)).ok_or("DC1 scanout allocation failed")?;
            if pages
                .as_paddr()
                .checked_add(size as u64)
                .is_none_or(|end| end > 1 << 34)
            {
                return Err("DC1 framebuffer outside DMA aperture");
            }
            *slot = Some(pages);
        }
        let dst = slot.as_ref().unwrap();
        if upload {
            // The public mapping stays at the same address across all
            // presents. Copy into the retired private buffer before flipping.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.staging.as_vaddr() as *const u8,
                    dst.as_vaddr() as *mut u8,
                    size,
                );
            }
        }
        arch::clean_dcache_to_poc_range(dst.as_vaddr(), size);
        Ok(Plane {
            address: dst.as_paddr(),
            pitch: self.config.stride,
            kind: 0,
        })
    }

    fn validate_region(&self, r: DisplayRegion) -> Result<(), &'static str> {
        if r.width == 0
            || r.height == 0
            || r.x
                .checked_add(r.width)
                .is_none_or(|end| end > self.mode.width)
            || r.y
                .checked_add(r.height)
                .is_none_or(|end| end > self.mode.height)
        {
            return Err("invalid DC1 presentation region");
        }
        Ok(())
    }

    fn gpu_plane(&self, image: &GpuDisplayResource) -> Result<Plane, &'static str> {
        let (address, pitch, format, size, rows, kind) = if let Some(b) = image.modified_backing() {
            if b.modifier() != GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4 {
                return Err("unsupported DC1 GPU modifier");
            }
            (
                b.physical_addr(),
                b.stride(),
                b.format(),
                b.allocation_size(),
                self.mode.height.next_multiple_of(128),
                0x42,
            )
        } else {
            let b = image.linear_backing().ok_or("missing DC1 GPU backing")?;
            if b.physical_segments().len() != 1 {
                return Err("DC1 requires contiguous scanout");
            }
            (
                b.physical_addr(),
                b.stride(),
                b.format(),
                b.allocation_size(),
                self.mode.height,
                0,
            )
        };
        let extent = u64::from(rows) * u64::from(pitch);
        if image.width() != self.mode.width
            || image.height() != self.mode.height
            || !matches!(format, PixelFormat::BGRA8888 | PixelFormat::XRGB8888)
            || pitch < self.config.stride
            || pitch > 0xffff
            || pitch & 63 != 0
            || address & (if kind == 0 { 4095 } else { 8191 }) != 0
            || size < extent
            || address.checked_add(extent).is_none_or(|end| end > 1 << 34)
        {
            return Err("unsupported DC1 GPU layout");
        }
        Ok(Plane {
            address,
            pitch,
            kind,
        })
    }
}

impl Device for Surface {
    fn device_type(&self) -> DeviceType {
        DeviceType::Graphics
    }
    fn name(&self) -> &'static str {
        "tegra210-dock"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn as_graphics_device(&self) -> Option<&dyn GraphicsDevice> {
        Some(self)
    }
}
impl ControlOps for Surface {
    fn control(&self, _: u32, _: usize) -> Result<i32, &'static str> {
        Err("use the display endpoint")
    }
}
impl MemoryMappingOps for Surface {
    fn get_mapping_info(&self, _: usize, _: usize) -> Result<MemoryMappingInfo, &'static str> {
        Err("use the display endpoint")
    }
    fn on_mapped(&self, _: usize, _: u64, _: usize, _: usize) {}
    fn on_unmapped(&self, _: usize, _: usize) {}
    fn supports_mmap(&self) -> bool {
        false
    }
}
impl Selectable for Surface {
    fn wait_until_ready(
        &self,
        _: ReadyInterest,
        _: &mut arch::Trapframe,
        _: Option<u64>,
        _: u64,
    ) -> SelectWaitOutcome {
        SelectWaitOutcome::Ready
    }
}
impl GraphicsDevice for Surface {
    fn get_display_name(&self) -> &'static str {
        "Tegra210 dock DP"
    }
    fn get_framebuffer_config(&self) -> Result<FramebufferConfig, &'static str> {
        Ok(self.config.clone())
    }
    fn get_framebuffer_address(&self) -> Result<u64, &'static str> {
        Ok(self.staging.as_paddr())
    }
    fn framebuffer_memory_attribute(&self) -> MemoryAttribute {
        MemoryAttribute::Normal
    }
    fn present_framebuffer_region(
        &self,
        config: &FramebufferConfig,
        address: u64,
        region: DisplayRegion,
    ) -> Result<(), &'static str> {
        self.validate_region(region)?;
        if config.width != self.config.width
            || config.height != self.config.height
            || config.stride != self.config.stride
            || config.format != self.config.format
        {
            return Err("DC1 framebuffer geometry changed");
        }
        if address != self.staging.as_paddr() {
            return Err("unknown DC1 staging framebuffer");
        }
        external::present_cpu(self)
    }
    fn present_gpu_resource_region_with_options(
        &self,
        image: GpuDisplayResource,
        region: DisplayRegion,
        options: GpuPresentOptions,
    ) -> Result<(), &'static str> {
        self.validate_region(region)?;
        if !options.is_swapchain_buffer() {
            return Err("DC1 requires a swapchain image");
        }
        let plane = self.gpu_plane(&image)?;
        external::present_gpu(self, plane, image)
    }
    fn init_graphics(&self) -> Result<(), &'static str> {
        Ok(())
    }
}

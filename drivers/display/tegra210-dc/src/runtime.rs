// SPDX-License-Identifier: GPL-2.0-only
//! Register programming follows Linux v6.12 tegra/dc.{c,h}, Switchroot
//! NVIDIA 76e6d48970b451c242c20f298b8d63027836bb0b dc/window.c, and Hekate
//! v6.5.3 bdk/display/di.{h,inl}. DC register numbers are word offsets.

use alloc::{boxed::Box, sync::Arc, vec};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use scarlet::{
    arch,
    device::{
        Device, DeviceType,
        graphics::{
            FramebufferConfig, GpuDisplayResource, GpuPresentOptions, GraphicsDevice, PixelFormat,
            manager::GraphicsManager, output::DisplayRegion,
        },
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
    },
    earlyfb::{self, EarlyFramebufferSurface},
    mem::page::ContiguousPages,
    object::capability::{ControlOps, MemoryMappingOps, Selectable},
    sync::Mutex,
    time,
    vm::{self, vmem::MemoryAttribute},
};
use scarlet_driver_tegra210::{cell, delay_us};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const STRIDE: u32 = WIDTH * 4;
const STATE_ACCESS: usize = 0x40;
const STATE_CONTROL: usize = 0x41;
const HEADER: usize = 0x42;
const OPTIONS: usize = 0x700;
const START: usize = 0x800;
const START_HI: usize = 0x80d;
const INT_STATUS: usize = 0x37;
const INT_MASK: usize = 0x38;
const INT_ENABLE: usize = 0x39;
const MEMFETCH_CONTROL: usize = 0x82b;
const WIN_ENABLE: u32 = 1 << 30;
const ACT_REQ: u32 = 3; // GENERAL and window A.
const VBLANK: u32 = 1 << 2;
const REGISTERS: [usize; 14] = [
    OPTIONS, 0x702, 0x703, 0x704, 0x705, 0x706, 0x707, 0x708, 0x709, 0x70a, START, START_HI, 0x806,
    0x808,
];
static REGISTERED: AtomicBool = AtomicBool::new(false);

struct State {
    initialized: bool,
    changed: bool,
    lost: bool,
    front: usize,
    gpu_front: Option<GpuDisplayResource>,
    pending_gpu: Option<GpuDisplayResource>,
    early_surface: Option<EarlyFramebufferSurface>,
}

struct Display {
    base: usize,
    config: FramebufferConfig,
    buffers: Option<[ContiguousPages; 2]>,
    original: [u32; 14],
    original_kind: u32,
    original_vblank_enable: u32,
    original_vblank_mask: u32,
    state: Mutex<State>,
    front: AtomicUsize,
    gpu_active: AtomicBool,
}

impl Display {
    fn read(&self, register: usize) -> u32 {
        unsafe { arch::mmio::read32(self.base + register * 4) }
    }

    fn write(&self, register: usize, value: u32) {
        unsafe { arch::mmio::write32(self.base + register * 4, value) };
        arch::io_mb();
    }

    fn wait(&self, register: usize, ready: impl Fn(u32) -> bool) -> Result<(), &'static str> {
        let deadline = time::current_time_ns().saturating_add(100_000_000);
        loop {
            let value = self.read(register);
            if ready(value) {
                return Ok(());
            }
            if time::current_time_ns() >= deadline {
                scarlet::println!(
                    "tegra-dc: timeout reg={:#x} value={:#010x}",
                    register,
                    value
                );
                return Err("Tegra DC activation/vblank timeout");
            }
            delay_us(10);
        }
    }

    fn activate(&self) -> Result<(), &'static str> {
        // Hekate starts with event generation disabled. Enable VBlank status
        // while leaving its CPU interrupt masked; no GIC IRQ is claimed.
        self.write(INT_MASK, self.read(INT_MASK) & !VBLANK);
        self.write(INT_ENABLE, self.read(INT_ENABLE) | VBLANK);
        // T210 window.c resets the fetch FIFO before updating a window.
        self.write(MEMFETCH_CONTROL, 1);
        self.write(STATE_CONTROL, ACT_REQ << 8);
        let _ = self.read(STATE_CONTROL);
        self.write(STATE_CONTROL, ACT_REQ);
        self.wait(STATE_CONTROL, |value| value & ACT_REQ == 0)?;
        // A completed latch alone does not retire outstanding old scanout
        // fetches. Keep both allocations through another full frame boundary.
        self.write(INT_STATUS, VBLANK);
        self.wait(INT_STATUS, |value| value & VBLANK != 0)?;
        Ok(())
    }

    fn flip(&self, paddr: u64, stride: u32, format: PixelFormat) -> Result<(), &'static str> {
        let access = self.read(STATE_ACCESS);
        let header = self.read(HEADER);
        self.write(STATE_ACCESS, 0); // Read and write assembly state.
        self.write(HEADER, 1 << 4);
        let result = (|| {
            self.write(0x702, 0); // Host buffer, pitch-linear.
            self.write(
                0x703,
                match format {
                    PixelFormat::BGRA8888 | PixelFormat::XRGB8888 => 12,
                    PixelFormat::RGBA8888 | PixelFormat::XBGR8888 => 13,
                    _ => return Err("Tegra DC requires a 32-bit RGB image"),
                },
            );
            self.write(0x704, 0);
            self.write(0x705, WIDTH << 16 | HEIGHT); // Physical portrait mode.
            self.write(0x706, WIDTH << 16 | HEIGHT * 4);
            self.write(0x707, 0);
            self.write(0x708, 0);
            self.write(0x709, 0x10001000); // No scaling.
            self.write(0x70a, stride);
            self.write(0x80b, 0);
            self.write(START, paddr as u32);
            self.write(START_HI, (paddr >> 32) as u32);
            // Switchroot invert-H uses the last byte of the source row.
            // With SCAN_COLUMN this fetches (1279-panel_y, panel_x), the
            // same orientation as the former CPU rotation, without a copy.
            self.write(0x806, WIDTH * 4 - 1);
            self.write(0x808, 0);
            self.write(OPTIONS, WIN_ENABLE | (1 << 4) | 1);
            self.activate()?;
            self.write(STATE_ACCESS, 1);
            if self.read(START) != paddr as u32
                || self.read(START_HI) != (paddr >> 32) as u32
                || self.read(OPTIONS) != WIN_ENABLE | (1 << 4) | 1
            {
                return Err("Tegra DC active scanout readback mismatch");
            }
            Ok(())
        })();
        self.write(HEADER, header);
        self.write(STATE_ACCESS, access);
        if result.is_ok() {
            self.restore_vblank();
        }
        result
    }

    fn restore(&self) -> Result<(), &'static str> {
        let access = self.read(STATE_ACCESS);
        let header = self.read(HEADER);
        self.write(STATE_ACCESS, 0);
        self.write(HEADER, 1 << 4);
        for (register, value) in REGISTERS.iter().zip(self.original) {
            self.write(*register, value);
        }
        self.write(0x80b, self.original_kind);
        let result = self.activate().and_then(|_| {
            self.write(STATE_ACCESS, 1);
            if REGISTERS
                .iter()
                .zip(self.original)
                .any(|(register, value)| self.read(*register) != value)
                || self.read(0x80b) != self.original_kind
            {
                return Err("Tegra DC rollback active state mismatch");
            }
            Ok(())
        });
        self.write(HEADER, header);
        self.write(STATE_ACCESS, access);
        if result.is_ok() {
            self.restore_vblank();
        }
        result
    }

    fn restore_vblank(&self) {
        self.write(INT_STATUS, VBLANK);
        self.write(
            INT_ENABLE,
            (self.read(INT_ENABLE) & !VBLANK) | self.original_vblank_enable,
        );
        self.write(
            INT_MASK,
            (self.read(INT_MASK) & !VBLANK) | self.original_vblank_mask,
        );
    }

    fn validate_config(&self, config: &FramebufferConfig) -> Result<(), &'static str> {
        if config.width != WIDTH
            || config.height != HEIGHT
            || config.stride != STRIDE
            || config.format != PixelFormat::BGRA8888
        {
            return Err("Tegra DC framebuffer layout mismatch");
        }
        Ok(())
    }

    fn validate_region(&self, region: DisplayRegion) -> Result<(), &'static str> {
        if region.width == 0
            || region.height == 0
            || region
                .x
                .checked_add(region.width)
                .is_none_or(|end| end > WIDTH)
            || region
                .y
                .checked_add(region.height)
                .is_none_or(|end| end > HEIGHT)
        {
            return Err("Tegra DC damage region exceeds image");
        }
        Ok(())
    }

    fn present_buffer(&self, state: &mut State, index: usize) -> Result<(), &'static str> {
        if state.lost {
            return Err("Tegra DC display is lost");
        }
        let buffers = self.buffers.as_ref().unwrap();
        let memory = buffers
            .get(index)
            .ok_or("invalid Tegra DC scanout buffer")?;
        state.changed = true;
        if let Err(error) = self.flip(memory.as_paddr(), STRIDE, PixelFormat::BGRA8888) {
            state.lost = true;
            return Err(error);
        }
        state.gpu_front = None;
        state.pending_gpu = None;
        state.front = index;
        self.front.store(index, Ordering::Release);
        self.gpu_active.store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        if self.state.get_mut().changed && self.restore().is_err() {
            // A failed flip may still become active. Retain every allocation
            // the controller could fetch, including the attempted GPU image.
            core::mem::forget(self.buffers.take());
            core::mem::forget(self.state.get_mut().gpu_front.take());
            core::mem::forget(self.state.get_mut().pending_gpu.take());
            return;
        }
        if let Some(surface) = self.state.get_mut().early_surface.take() {
            // The original firmware reservation remains live after rollback.
            unsafe { earlyfb::restore_surface(surface) };
        }
    }
}

impl Device for Display {
    fn device_type(&self) -> DeviceType {
        DeviceType::Graphics
    }
    fn name(&self) -> &'static str {
        "tegra210-dc"
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
impl ControlOps for Display {
    fn control(&self, _: u32, _: usize) -> Result<i32, &'static str> {
        Err("use the ordinary display endpoint")
    }
}
impl MemoryMappingOps for Display {
    fn get_mapping_info(
        &self,
        _: usize,
        _: usize,
    ) -> Result<scarlet::object::capability::MemoryMappingInfo, &'static str> {
        Err("use the ordinary display endpoint")
    }
    fn on_mapped(&self, _: usize, _: u64, _: usize, _: usize) {}
    fn on_unmapped(&self, _: usize, _: usize) {}
    fn supports_mmap(&self) -> bool {
        false
    }
}
impl Selectable for Display {
    fn wait_until_ready(
        &self,
        _: scarlet::object::capability::selectable::ReadyInterest,
        _: &mut scarlet::arch::Trapframe,
        _: Option<u64>,
        _: u64,
    ) -> scarlet::object::capability::selectable::SelectWaitOutcome {
        scarlet::object::capability::selectable::SelectWaitOutcome::Ready
    }
}

impl GraphicsDevice for Display {
    fn get_display_name(&self) -> &'static str {
        "Tegra210 DSI"
    }
    fn get_framebuffer_config(&self) -> Result<FramebufferConfig, &'static str> {
        Ok(self.config.clone())
    }
    fn get_framebuffer_address(&self) -> Result<u64, &'static str> {
        Ok(self.buffers.as_ref().unwrap()[self.front.load(Ordering::Acquire) ^ 1].as_paddr())
    }
    fn framebuffer_memory_attribute(&self) -> MemoryAttribute {
        MemoryAttribute::DeviceBurstable
    }
    fn scanout_buffer_count(&self) -> usize {
        2
    }
    fn front_scanout_buffer(&self) -> Option<usize> {
        (!self.gpu_active.load(Ordering::Acquire)).then(|| self.front.load(Ordering::Acquire))
    }
    fn get_scanout_buffer_info(
        &self,
        index: usize,
    ) -> Result<(FramebufferConfig, u64), &'static str> {
        let memory = self
            .buffers
            .as_ref()
            .unwrap()
            .get(index)
            .ok_or("invalid Tegra DC scanout buffer")?;
        Ok((self.config.clone(), memory.as_paddr()))
    }
    fn present_framebuffer_region(
        &self,
        config: &FramebufferConfig,
        paddr: u64,
        region: DisplayRegion,
    ) -> Result<(), &'static str> {
        self.validate_config(config)?;
        self.validate_region(region)?;
        let mut state = self.state.lock();
        let back = state.front ^ 1;
        if paddr != self.buffers.as_ref().unwrap()[back].as_paddr() {
            return Err("Tegra DC backing is not the current back buffer");
        }
        self.present_buffer(&mut state, back)?;
        earlyfb::deactivate();
        Ok(())
    }
    fn present_scanout_buffer(&self, index: usize) -> Result<(), &'static str> {
        self.present_scanout_buffer_regions(index, &[])
    }
    fn present_scanout_buffer_regions(
        &self,
        index: usize,
        regions: &[DisplayRegion],
    ) -> Result<(), &'static str> {
        for region in regions {
            self.validate_region(*region)?;
        }
        let mut state = self.state.lock();
        if state.gpu_front.is_none() && index == state.front {
            return Err("Tegra DC buffer is currently scanned out");
        }
        self.present_buffer(&mut state, index)?;
        earlyfb::deactivate();
        Ok(())
    }
    fn present_gpu_resource_region_with_options(
        &self,
        resource: GpuDisplayResource,
        region: DisplayRegion,
        options: GpuPresentOptions,
    ) -> Result<(), &'static str> {
        self.validate_region(region)?;
        if !options.is_swapchain_buffer() {
            return Err("Tegra DC direct GPU scanout requires a swapchain image");
        }
        let backing = resource
            .linear_backing()
            .ok_or("Tegra DC requires linear GPU backing")?;
        let required = u64::from(backing.stride()) * u64::from(HEIGHT);
        if resource.width() != WIDTH
            || resource.height() != HEIGHT
            || backing.physical_segments().len() != 1
            || backing.stride() < STRIDE
            || backing.stride() > 0xffff
            || backing.stride() & 63 != 0
            || backing.physical_addr() & 63 != 0
            || backing.allocation_size() < required
            || backing
                .physical_addr()
                .checked_add(required)
                .is_none_or(|end| end > 1 << 34)
            || !matches!(
                backing.format(),
                PixelFormat::BGRA8888
                    | PixelFormat::XRGB8888
                    | PixelFormat::RGBA8888
                    | PixelFormat::XBGR8888
            )
        {
            return Err("unsupported Tegra DC GPU scanout layout");
        }
        let paddr = backing.physical_addr();
        let stride = backing.stride();
        let format = backing.format();
        let mut state = self.state.lock();
        if state.lost {
            return Err("Tegra DC display is lost");
        }
        // The producer must retire rendering and publish its writes before
        // presentation. Cleaning a stale CPU alias here could overwrite a
        // GPU-rendered frame. This controller only consumes the ready image.
        arch::io_mb();
        state.pending_gpu = Some(resource);
        state.changed = true;
        if let Err(error) = self.flip(paddr, stride, format) {
            state.lost = true;
            return Err(error);
        }
        state.gpu_front = state.pending_gpu.take();
        self.gpu_active.store(true, Ordering::Release);
        earlyfb::deactivate();
        Ok(())
    }
    fn init_graphics(&self) -> Result<(), &'static str> {
        let mut state = self
            .state
            .try_lock()
            .ok_or("Tegra DC initialization is busy")?;
        if state.initialized {
            return Ok(());
        }
        self.present_buffer(&mut state, 0)?;
        // Keep the ordinary boot/emergency console on the adopted surface
        // until the first userspace present. No console-mode policy lives here.
        state.early_surface = unsafe {
            earlyfb::replace_surface(
                self.buffers.as_ref().unwrap()[0].as_vaddr(),
                WIDTH as usize,
                HEIGHT as usize,
                STRIDE as usize,
                false,
                false,
            )
        }?;
        state.initialized = true;
        scarlet::println!(
            "tegra-dc: native scanout active; 1280x720, two buffers, hardware rotation"
        );
        Ok(())
    }
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if REGISTERED.load(Ordering::Acquire) {
        return Err("Tegra DC is already registered");
    }
    let resource = device
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM && r.start == 0x54200000)
        .ok_or("only the inherited DC0 DSI scanout is supported")?;
    if resource.size()? < 0x40000 || cell(device, "scarlet,boot-scanout", 0) != Some(1) {
        return Err("missing Tegra DC firmware scanout contract");
    }
    let car = vm::ioremap(0x60006000, 0x20)?;
    let mc = vm::ioremap(0x70019000, 0x248)?;
    let read = |base, offset| unsafe { arch::mmio::read32(base + offset) };
    if read(car, 0x10) & (1 << 27) == 0 || read(car, 4) & (1 << 27) != 0 {
        return Err("inherited Tegra DC clock/reset is not active");
    }
    if read(mc, 0x10) & 1 != 0 && read(mc, 0x240) & (1 << 31) != 0 {
        return Err("DC0 is attached to an inherited SMMU domain");
    }
    let base = vm::ioremap(resource.start, 0x2100)?;
    let dc_read = |r| read(base, r * 4);
    let dc_write = |r, v| unsafe {
        arch::mmio::write32(base + r * 4, v);
        arch::io_mb();
    };
    let access = dc_read(STATE_ACCESS);
    let header = dc_read(HEADER);
    if dc_read(STATE_CONTROL) & ACT_REQ != 0 {
        return Err("inherited Tegra DC has a pending update");
    }
    dc_write(STATE_ACCESS, 1);
    dc_write(HEADER, 1 << 4);
    let original = REGISTERS.map(dc_read);
    let kind = dc_read(0x80b);
    let display_command = dc_read(0x32);
    let active = dc_read(0x409);
    dc_write(HEADER, header);
    dc_write(STATE_ACCESS, access);
    scarlet::println!(
        "tegra-dc: inherited addr={:#x}/{:#x} options={:#x} kind={:#x} mode={:#x} active={:#x}",
        original[11],
        original[10],
        original[0],
        kind,
        display_command,
        active
    );
    if original[0] != WIN_ENABLE
        || original[2] != 12
        || original[9] & 0xffff != 720 * 4
        || original[10] != 0xf5a00000
        || original[11] != 0
        || kind != 0
        || display_command & (3 << 5) != 1 << 5
        || active != (1280 << 16 | 720)
    {
        return Err("inherited Tegra DC scanout differs from the inspected Hekate mode");
    }
    let mut buffers = [
        ContiguousPages::new((STRIDE as usize * HEIGHT as usize).div_ceil(4096))
            .ok_or("Tegra DC scanout allocation failed")?,
        ContiguousPages::new((STRIDE as usize * HEIGHT as usize).div_ceil(4096))
            .ok_or("Tegra DC scanout allocation failed")?,
    ];
    for memory in &mut buffers {
        if memory
            .as_paddr()
            .checked_add(u64::from(STRIDE) * u64::from(HEIGHT))
            .is_none_or(|end| end > 1 << 34)
        {
            return Err("Tegra DC scanout exceeds 34-bit DMA address range");
        }
        memory.retag_memory_attribute(MemoryAttribute::DeviceBurstable)?;
    }
    // Preserve the last boot frame once. Subsequent frames are scanned out
    // directly in landscape layout; there is no per-present CPU rotation.
    let boot = vm::addr::phys_to_virt(u64::from(original[10]));
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let source = boot + (((WIDTH - 1 - x) * HEIGHT + y) * 4) as usize;
            let pixel = unsafe { core::ptr::read_volatile(source as *const u32) };
            unsafe {
                core::ptr::write_volatile(
                    (buffers[0].as_vaddr() + (y * STRIDE + x * 4) as usize) as *mut u32,
                    pixel,
                )
            };
        }
    }
    let display = Arc::new(Display {
        base,
        config: FramebufferConfig::new(WIDTH, HEIGHT, PixelFormat::BGRA8888),
        buffers: Some(buffers),
        original,
        original_kind: kind,
        original_vblank_enable: dc_read(INT_ENABLE) & VBLANK,
        original_vblank_mask: dc_read(INT_MASK) & VBLANK,
        front: AtomicUsize::new(0),
        gpu_active: AtomicBool::new(false),
        state: Mutex::new(State {
            initialized: false,
            changed: false,
            lost: false,
            front: 0,
            gpu_front: None,
            pending_gpu: None,
            early_surface: None,
        }),
    });
    let id = DeviceManager::get_manager().register_device(display.clone());
    if let Err(error) =
        GraphicsManager::get_manager().register_native_framebuffer_from_device(id, display.clone())
    {
        DeviceManager::get_manager().unregister_device(id);
        return Err(error);
    }
    REGISTERED.store(true, Ordering::Release);
    Ok(())
}

fn register() {
    let driver = PlatformDeviceDriver::new(
        "tegra210-dc",
        probe,
        |_| Err("Tegra DC scanout is registered"),
        vec!["nvidia,tegra210-dc"],
    )
    .with_probe_options(PlatformProbeOptions {
        deassert_resets: false,
        resolve_iommu: false,
        resolve_dma: false,
    });
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Late);
}
scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}

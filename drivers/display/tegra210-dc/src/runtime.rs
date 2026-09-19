// SPDX-License-Identifier: GPL-2.0-only
//! Register programming follows Linux v6.12 tegra/dc.{c,h}, Switchroot
//! NVIDIA 7d95822acda1f6dab3f3d1d099b43ff9e0d0e626 dc/{window,ext/dev}.c, and Hekate
//! v6.5.3 bdk/display/di.{h,inl}. DC register numbers are word offsets.

use alloc::{boxed::Box, sync::Arc, vec};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
};
use scarlet::{
    arch,
    device::{
        Device, DeviceType,
        events::InterruptCapableDevice,
        gpu::GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4,
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
    earlyfb,
    interrupt::{
        InterruptClaim, InterruptId, InterruptResult, register_and_enable_platform_irq_device,
        resolve_platform_irq,
    },
    mem::page::ContiguousPages,
    object::capability::{ControlOps, MemoryMappingOps, Selectable},
    sync::{IrqSpinLock, Mutex, Waker},
    time,
    vm::{self, vmem::MemoryAttribute},
};
use scarlet_driver_tegra210::{cell, delay_us};

use crate::block_linear;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const STRIDE: u32 = WIDTH * 4;
const STATE_ACCESS: usize = 0x40;
const STATE_CONTROL: usize = 0x41;
const HEADER: usize = 0x42;
const ACT_CONTROL: usize = 0x43;
const CDE_CONTROL: usize = 0x82f;
const CDE_CG_SW_OVERRIDE: usize = 0x837;
const OPTIONS: usize = 0x700;
const START: usize = 0x800;
const START_HI: usize = 0x80d;
const INT_STATUS: usize = 0x37;
const INT_MASK: usize = 0x38;
const INT_ENABLE: usize = 0x39;
const MEMFETCH_CONTROL: usize = 0x82b;
const WIN_ENABLE: u32 = 1 << 30;
const SCAN_COLUMN: u32 = 1 << 4;
const H_DIRECTION: u32 = 1;
const BUFFER_STRIDE: usize = 0x70b;
const UV_BUFFER_STRIDE: usize = 0x70c;
const MEM_HIGH_PRIORITY: usize = 0x403;
const MEM_HIGH_PRIORITY_TIMER: usize = 0x404;
const ACT_REQ: u32 = 3; // GENERAL and window A.
const FRAME_END: u32 = 1 << 1;
const VBLANK: u32 = 1 << 2;
const WINDOW_A_FETCH_EVENTS: u32 = (1 << 8) | (1 << 14);
const WINDOW_B_FETCH_EVENTS: u32 = (1 << 9) | (1 << 15);
// T210 uses the gen2 blender at 0x716..0x719. NVIDIA window.c programs
// DC_WIN_GLOBAL_ALPHA (0x715) only for gen1; do not manage or verify it here.
const REGISTERS: [usize; 24] = [
    OPTIONS,
    0x702,
    0x703,
    0x704,
    0x705,
    0x706,
    0x707,
    0x708,
    0x709,
    0x70a,
    START,
    START_HI,
    0x806,
    0x808,
    0x701,
    0x70d,
    0x716,
    0x717,
    0x718,
    0x719,
    BUFFER_STRIDE,
    UV_BUFFER_STRIDE,
    CDE_CONTROL,
    CDE_CG_SW_OVERRIDE,
];
static REGISTERED: AtomicBool = AtomicBool::new(false);

struct State {
    initialized: bool,
    changed: bool,
    lost: bool,
    front: usize,
    scanout_front: usize,
    gpu_front: Option<GpuDisplayResource>,
    pending_gpu: Option<GpuDisplayResource>,
}

struct Display {
    base: usize,
    mc: usize,
    config: FramebufferConfig,
    // Ordinary linear mmap buffers; private block-linear front/back are never
    // exported as a linear framebuffer or written while DC fetches them.
    buffers: Option<[ContiguousPages; 2]>,
    scanout: Option<[ContiguousPages; 2]>,
    original: [u32; REGISTERS.len()],
    original_console_window: [u32; REGISTERS.len()],
    original_console_kind: u32,
    original_kind: u32,
    event_mask: u32,
    original_event_enable: u32,
    original_event_mask: u32,
    original_priority: [u32; 2],
    original_activation: u32,
    state: Mutex<State>,
    front: AtomicUsize,
    gpu_active: AtomicBool,
    keep_console: bool,
    diagnostic_frames: AtomicUsize,
    last_underflow_a: AtomicU32,
    last_underflow_b: AtomicU32,
    interrupt_id: InterruptId,
    irq_ready: AtomicBool,
    irq_lock: IrqSpinLock<()>,
    flip_pending: AtomicBool,
    flip_complete: AtomicBool,
    flip_waker: Waker,
    delayed_flip_irqs: AtomicUsize,
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
        // Hekate starts with event generation disabled. Enable frame and
        // fetch-error status. Only FRAME_END is unmasked during an armed flip.
        self.write(INT_MASK, self.read(INT_MASK) & !self.event_mask);
        self.write(INT_ENABLE, self.read(INT_ENABLE) | self.event_mask);
        // T210 window.c resets the fetch FIFO before updating a window.
        self.write(MEMFETCH_CONTROL, 1);
        let header = self.read(HEADER);
        self.write(HEADER, 1 << 5);
        self.write(MEMFETCH_CONTROL, 1);
        self.write(HEADER, header);
        // B preserves the firmware console through native initialization;
        // the first normal userspace present retires it with A's page flip.
        let request = ACT_REQ | (1 << 2);
        self.write(STATE_CONTROL, request << 8);
        let _ = self.read(STATE_CONTROL);
        // Hekate leaves all window activations on H-counter boundaries.
        // NVIDIA window.c selects V-counter activation for a vsynced flip.
        // Waiting for a later VBlank cannot make an H-counter latch atomic.
        let activation_mask = self.activation_mask();
        self.write(ACT_CONTROL, self.read(ACT_CONTROL) & !activation_mask);
        if self.read(ACT_CONTROL) & activation_mask != 0 {
            return Err("Tegra DC V-counter activation did not latch");
        }
        // Linux's continuous-mode IRQ retires windows at FRAME_END once
        // ACT_REQ has cleared, then ext/dev.c releases the old front buffers.
        // Clear the sticky event BEFORE arming this flip, so its own boundary
        // can satisfy both conditions. Clearing it after promotion would
        // impose a second frame wait and cap 60-Hz scanout at 30 presents/s.
        let use_irq = self.irq_ready.load(Ordering::Acquire) && scarlet::task::mytask().is_some();
        {
            let _irq = self.irq_lock.lock();
            self.flip_complete.store(false, Ordering::Release);
            self.write(INT_STATUS, FRAME_END);
            self.flip_pending.store(use_irq, Ordering::Release);
            self.write(STATE_CONTROL, request);
            if use_irq {
                self.write(INT_MASK, self.read(INT_MASK) | FRAME_END);
            }
        }
        if use_irq {
            let deadline = time::current_time_ns().saturating_add(100_000_000);
            let task = scarlet::task::mytask().ok_or("DC flip lost its waiting task")?;
            while !self.flip_complete.load(Ordering::Acquire) {
                let remaining = deadline.saturating_sub(time::current_time_ns());
                if remaining == 0 {
                    // A delayed IRQ is not a failed flip. Recheck the same
                    // hardware retirement proof as the ISR while excluding
                    // it, before revoking pending ownership or losing DC.
                    let (complete, status, activation, mask, enable) = {
                        let _irq = self.irq_lock.lock();
                        let status = self.read(INT_STATUS);
                        self.retire_flip_locked(status);
                        let complete = self.flip_complete.load(Ordering::Acquire);
                        let snapshot = (
                            complete,
                            status,
                            self.read(STATE_CONTROL),
                            self.read(INT_MASK),
                            self.read(INT_ENABLE),
                        );
                        self.write(INT_MASK, self.read(INT_MASK) & !FRAME_END);
                        self.flip_pending.store(false, Ordering::Release);
                        snapshot
                    };
                    if !complete {
                        scarlet::println!(
                            "tegra-dc: flip timeout status={:#x} act={:#x} mask={:#x} enable={:#x}",
                            status,
                            activation,
                            mask,
                            enable
                        );
                        return Err("Tegra DC FRAME_END interrupt timeout");
                    }
                    if self.delayed_flip_irqs.fetch_add(1, Ordering::Relaxed) < 4 {
                        scarlet::println!(
                            "tegra-dc: retired completed flip after delayed FRAME_END IRQ"
                        );
                    }
                    break;
                }
                self.flip_waker.wait_with_condition(
                    task.get_id(),
                    task.get_trapframe(),
                    Some(remaining),
                    0,
                    || self.flip_complete.load(Ordering::Acquire),
                );
            }
        } else {
            // Native adoption precedes scheduler/IRQ initialization.
            self.wait(STATE_CONTROL, |value| value & request == 0)?;
            self.wait(INT_STATUS, |value| value & FRAME_END != 0)?;
        }
        Ok(())
    }

    /// Caller holds irq_lock; both ISR and timeout observation use the same
    /// post-arm FRAME_END plus cleared activation proof before releasing DMA.
    fn retire_flip_locked(&self, status: u32) -> bool {
        let completed = status & FRAME_END != 0
            && self.flip_pending.load(Ordering::Acquire)
            && self.read(STATE_CONTROL) & (ACT_REQ | (1 << 2)) == 0;
        if status & FRAME_END != 0 {
            self.write(INT_STATUS, FRAME_END);
        }
        if completed {
            self.write(INT_MASK, self.read(INT_MASK) & !FRAME_END);
            self.flip_pending.store(false, Ordering::Release);
            self.flip_complete.store(true, Ordering::Release);
        }
        completed
    }

    fn activation_mask(&self) -> u32 {
        // WIN_ACT_CNTR_SEL_HCOUNTER(index) = 1 << (index * 2 + 2).
        (1 << 2) | (1 << 4)
    }

    fn priority_fields(&self) -> (u32, u32, u32, u32) {
        // Linux dc.h places A in bits 22:16, B in 14:8. Timer fields
        // are six bits each. Preserve cursor and unclaimed C fields.
        let windows = (1 << 16) | (1 << 8);
        (0x7f * windows, 0x3f * windows, 0x20 * windows, windows)
    }

    fn flip(
        &self,
        paddr: u64,
        format: PixelFormat,
        show_console: bool,
    ) -> Result<(), &'static str> {
        let stride = STRIDE;
        let access = self.read(STATE_ACCESS);
        let header = self.read(HEADER);
        self.write(STATE_ACCESS, 0); // Read and write assembly state.
        self.write(HEADER, 1 << 4);
        let result = (|| {
            // Hekate initializes both priority controls to zero. Adopt
            // Linux/NVIDIA's native DC fetch threshold and timer instead
            // of retaining bootloader FIFO policy for SCAN_COLUMN fetches.
            let (priority_mask, timer_mask, priority, timer) = self.priority_fields();
            self.write(
                MEM_HIGH_PRIORITY,
                (self.read(MEM_HIGH_PRIORITY) & !priority_mask) | priority,
            );
            self.write(
                MEM_HIGH_PRIORITY_TIMER,
                (self.read(MEM_HIGH_PRIORITY_TIMER) & !timer_mask) | timer,
            );
            // Both CPU and admitted GPU images are uncompressed. Explicitly
            // disable inherited CDE state, as NVIDIA's uncompressed branch does.
            self.write(CDE_CONTROL, 0);
            self.write(CDE_CG_SW_OVERRIDE, 1);
            self.write(0x701, 0); // No byte swap.
            self.write(0x702, 0); // Host buffer; DC surface kind selects block-linear.
            let color_depth = match format {
                PixelFormat::BGRA8888 | PixelFormat::XRGB8888 => 12,
                PixelFormat::RGBA8888 | PixelFormat::XBGR8888 => 13,
                _ => return Err("Tegra DC requires a 32-bit RGB image"),
            };
            self.write(0x703, color_depth);
            self.write(0x704, 0);
            self.write(0x705, WIDTH << 16 | HEIGHT); // Physical portrait mode.
            self.write(0x706, WIDTH << 16 | HEIGHT * 4);
            self.write(0x707, 0);
            self.write(0x708, 0);
            self.write(0x709, 0x10001000); // No scaling.
            self.write(0x70a, stride);
            // Linux tegra_dc_setup_window clears these even for pitch
            // surfaces. Do not inherit a firmware pixel/tile stride.
            self.write(BUFFER_STRIDE, 0);
            self.write(UV_BUFFER_STRIDE, 0);
            self.write(0x70d, 0); // Clear inherited address mode; kind is programmed below.
            // SWS supplies the complete composited RGB frame. Linux's gen2
            // blender bypass avoids inherited per-pixel alpha/key state.
            self.write(0x716, (1 << 24) | 255);
            self.write(0x80b, block_linear::SURFACE_KIND);
            self.write(START, paddr as u32);
            self.write(START_HI, (paddr >> 32) as u32);
            // Switchroot ext/dev.c maps its 90-degree output rotation to
            // SCAN_COLUMN | INVERT_H. NVIDIA window.c starts INVERT_H at
            // (source_x + source_width) * Bpp - 1, the last byte (not pixel).
            // Geometry remains landscape storage -> inherited portrait output.
            let h_offset = STRIDE - 1;
            let options = WIN_ENABLE | SCAN_COLUMN | H_DIRECTION;
            self.write(0x806, h_offset);
            self.write(0x808, 0);
            self.write(OPTIONS, options);
            self.write(HEADER, 1 << 5);
            if show_console {
                // Keep the original portrait boot console in window B. It
                // uses the inherited linear fetch, independently of window
                // A's SCAN_COLUMN landscape fetch. It remains through native
                // initialization; keep_bootcon also retains it in userspace.
                for (register, value) in REGISTERS.iter().zip(self.original) {
                    self.write(*register, value);
                }
                self.write(0x80b, self.original_kind);
                self.write(CDE_CONTROL, 0);
                self.write(CDE_CG_SW_OVERRIDE, 1);
                self.write(0x716, 1 << 24); // Opaque foreground, depth zero.
                self.write(OPTIONS, WIN_ENABLE);
            } else {
                self.write(OPTIONS, 0);
            }
            self.write(HEADER, 1 << 4);
            if let Err(error) = self.activate() {
                self.trace_scanout(paddr, stride, options);
                return Err(error);
            }
            self.write(STATE_ACCESS, 1);
            let expected = [
                (START, paddr as u32),
                (START_HI, (paddr >> 32) as u32),
                (OPTIONS, options),
                (0x701, 0),
                (0x702, 0),
                (0x703, color_depth),
                (0x705, WIDTH << 16 | HEIGHT),
                (0x706, WIDTH << 16 | HEIGHT * 4),
                (0x709, 0x10001000),
                (0x70a, stride),
                (BUFFER_STRIDE, 0),
                (UV_BUFFER_STRIDE, 0),
                (0x70d, 0),
                (0x716, (1 << 24) | 255),
                (0x80b, block_linear::SURFACE_KIND),
                (0x806, h_offset),
                (0x808, 0),
                (CDE_CONTROL, 0),
                (CDE_CG_SW_OVERRIDE, 1),
            ];
            if let Some(&(register, value)) = expected
                .iter()
                .find(|&&(register, value)| self.read(register) != value)
            {
                scarlet::println!(
                    "tegra-dc: active reg={:#x} expected={:#010x} actual={:#010x}",
                    register,
                    value,
                    self.read(register)
                );
                return Err("Tegra DC active scanout readback mismatch");
            }
            for (register, mask, value) in [
                (MEM_HIGH_PRIORITY, priority_mask, priority),
                (MEM_HIGH_PRIORITY_TIMER, timer_mask, timer),
                (ACT_CONTROL, self.activation_mask(), 0),
            ] {
                if self.read(register) & mask != value {
                    scarlet::println!(
                        "tegra-dc: active fetch policy reg={:#x} mask={:#010x} expected={:#010x} actual={:#010x}",
                        register,
                        mask,
                        value,
                        self.read(register)
                    );
                    return Err("Tegra DC active fetch policy mismatch");
                }
            }
            self.write(HEADER, 1 << 5);
            let console_valid = if show_console {
                REGISTERS
                    .iter()
                    .zip(self.original)
                    .all(|(register, value)| {
                        let expected = match *register {
                            OPTIONS => WIN_ENABLE,
                            0x716 => 1 << 24,
                            CDE_CONTROL => 0,
                            CDE_CG_SW_OVERRIDE => 1,
                            _ => value,
                        };
                        self.read(*register) == expected
                    })
                    && self.read(0x80b) == self.original_kind
            } else {
                self.read(OPTIONS) == 0
            };
            self.write(HEADER, 1 << 4);
            if !console_valid {
                return Err("Tegra DC boot console state mismatch");
            }
            self.trace_scanout(paddr, stride, options);
            Ok(())
        })();
        self.write(HEADER, header);
        self.write(STATE_ACCESS, access);
        if result.is_ok() {
            self.restore_events();
        }
        result
    }

    fn restore(&self) -> Result<(), &'static str> {
        let access = self.read(STATE_ACCESS);
        let header = self.read(HEADER);
        self.write(STATE_ACCESS, 0);
        self.write(HEADER, 1 << 4);
        let (priority_mask, timer_mask, _, _) = self.priority_fields();
        for (register, mask, original) in [
            (MEM_HIGH_PRIORITY, priority_mask, self.original_priority[0]),
            (
                MEM_HIGH_PRIORITY_TIMER,
                timer_mask,
                self.original_priority[1],
            ),
        ] {
            self.write(register, (self.read(register) & !mask) | (original & mask));
        }
        for (register, value) in REGISTERS.iter().zip(self.original) {
            self.write(*register, value);
        }
        self.write(0x80b, self.original_kind);
        self.write(HEADER, 1 << 5);
        for (register, value) in REGISTERS.iter().zip(self.original_console_window) {
            self.write(*register, value);
        }
        self.write(0x80b, self.original_console_kind);
        self.write(HEADER, 1 << 4);
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
            if self.read(MEM_HIGH_PRIORITY) & priority_mask
                != self.original_priority[0] & priority_mask
                || self.read(MEM_HIGH_PRIORITY_TIMER) & timer_mask
                    != self.original_priority[1] & timer_mask
            {
                return Err("Tegra DC rollback fetch priority mismatch");
            }
            self.write(HEADER, 1 << 5);
            let valid = REGISTERS
                .iter()
                .zip(self.original_console_window)
                .all(|(register, value)| self.read(*register) == value)
                && self.read(0x80b) == self.original_console_kind;
            self.write(HEADER, 1 << 4);
            if !valid {
                return Err("Tegra DC boot console rollback mismatch");
            }
            let mask = self.activation_mask();
            self.write(
                ACT_CONTROL,
                (self.read(ACT_CONTROL) & !mask) | (self.original_activation & mask),
            );
            if self.read(ACT_CONTROL) & mask != self.original_activation & mask {
                return Err("Tegra DC rollback activation policy mismatch");
            }
            Ok(())
        });
        self.write(HEADER, header);
        self.write(STATE_ACCESS, access);
        if result.is_ok() {
            self.restore_events();
        }
        result
    }

    fn restore_events(&self) {
        self.write(INT_STATUS, self.event_mask);
        self.write(
            INT_ENABLE,
            (self.read(INT_ENABLE) & !self.event_mask) | self.original_event_enable,
        );
        self.write(
            INT_MASK,
            (self.read(INT_MASK) & !self.event_mask) | self.original_event_mask,
        );
    }

    fn validate_initial_scanout(&self) -> Result<(), &'static str> {
        // Register readback does not prove a usable fetch path. Earlier pitch
        // column candidates latched correctly while underflowing every frame.
        // Observe two further frame boundaries before replacing simplefb;
        // this is an adoption check, not a wait added to ordinary presents.
        let before = [self.read(0xbca), self.read(0xdca)];
        self.write(INT_MASK, self.read(INT_MASK) & !VBLANK);
        self.write(INT_ENABLE, self.read(INT_ENABLE) | VBLANK);
        let result = (|| {
            for _ in 0..2 {
                self.write(INT_STATUS, VBLANK);
                self.wait(INT_STATUS, |value| value & VBLANK != 0)?;
            }
            let after = [self.read(0xbca), self.read(0xdca)];
            let delta = [
                after[0].wrapping_sub(before[0]),
                after[1].wrapping_sub(before[1]),
            ];
            scarlet::println!(
                "tegra-dc: adoption fetch A={:#x}->{:#x} B={:#x}->{:#x} delta={}/{}",
                before[0],
                after[0],
                before[1],
                after[1],
                delta[0],
                delta[1]
            );
            if delta[0] != 0 || delta[1] != 0 {
                return Err("DC column scanout underflows; keeping firmware framebuffer");
            }
            Ok(())
        })();
        self.restore_events();
        result
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
        arch::io_mb();
        self.diagnostic_frames.fetch_add(1, Ordering::Relaxed);
        let scanout = state.scanout_front ^ 1;
        let paddr = self.scanout.as_ref().unwrap()[scanout].as_paddr();
        self.upload_frame(memory.as_paddr(), STRIDE, paddr, false)?;
        state.changed = true;
        if let Err(error) = self.flip(
            paddr,
            PixelFormat::BGRA8888,
            !state.initialized || self.keep_console,
        ) {
            state.lost = true;
            return Err(error);
        }
        state.gpu_front = None;
        state.pending_gpu = None;
        state.front = index;
        state.scanout_front = scanout;
        self.front.store(index, Ordering::Release);
        self.gpu_active.store(false, Ordering::Release);
        Ok(())
    }

    fn upload_frame(
        &self,
        source: u64,
        stride: u32,
        destination: u64,
        gpu: bool,
    ) -> Result<(), &'static str> {
        let source_address = vm::addr::phys_to_virt(source);
        let source_size = stride as usize * HEIGHT as usize;
        if gpu {
            // The producer retired before entry. Invalidate for CPU reads;
            // never clean a stale CPU alias over the GPU's completed stores.
            arch::invalidate_dcache_to_poc_range(source_address, source_size);
        }
        let output_address = vm::addr::phys_to_virt(destination);
        let input =
            unsafe { core::slice::from_raw_parts(source_address as *const u8, source_size) };
        let output = unsafe {
            core::slice::from_raw_parts_mut(output_address as *mut u8, block_linear::SIZE)
        };
        let start = time::current_time_ns();
        block_linear::upload(input, stride as usize, output)?;
        arch::io_mb();
        let sequence = self.diagnostic_frames.load(Ordering::Relaxed);
        if sequence <= 4 {
            scarlet::println!(
                "tegra-dc: conversion={} gpu={} elapsed_us={}",
                sequence,
                gpu,
                time::current_time_ns().saturating_sub(start) / 1000
            );
        }
        if sequence == 1 {
            let elapsed = time::current_time_ns().saturating_sub(start);
            let mut matched = 0;
            for gy in 0..18 {
                for gx in 0..32 {
                    let x = gx * (WIDTH as usize - 1) / 31;
                    let y = gy * (HEIGHT as usize - 1) / 17;
                    let offset = y * stride as usize + x * 4;
                    let tiled_offset = block_linear::pixel_offset(x, y);
                    matched += u32::from(
                        input[offset..offset + 4] == output[tiled_offset..tiled_offset + 4],
                    );
                }
            }
            scarlet::println!(
                "tegra-dc: upload={} src={:#x} dst={:#x} kind={:#x} block-height={} padded={} elapsed={}us matched={}/576",
                sequence,
                source,
                destination,
                block_linear::SURFACE_KIND,
                1 << block_linear::BLOCK_HEIGHT_LOG2,
                block_linear::SIZE,
                elapsed / 1000,
                matched
            );
            if matched != 576 {
                return Err("Tegra DC block-linear storage differs from ready source");
            }
        }
        Ok(())
    }

    fn trace_scanout(&self, paddr: u64, stride: u32, options: u32) {
        let sequence = self.diagnostic_frames.load(Ordering::Relaxed);
        if sequence > 4 {
            return;
        }
        let mc_read = |offset| unsafe { arch::mmio::read32(self.mc + offset) };
        let underflow_a = self.read(0xbca);
        let underflow_b = self.read(0xdca);
        let previous_a = self.last_underflow_a.swap(underflow_a, Ordering::Relaxed);
        let previous_b = self.last_underflow_b.swap(underflow_b, Ordering::Relaxed);
        // These are fetch counters/status, not proof of correct physical
        // pixels. Read the shared MC error latch without acknowledging it.
        scarlet::println!(
            "tegra-dc: scanout={} addr={:#x} pitch={} options={:#010x} offsets={}/{} buf-stride={}/{}",
            sequence,
            paddr,
            stride,
            options,
            self.read(0x806),
            self.read(0x808),
            self.read(BUFFER_STRIDE),
            self.read(UV_BUFFER_STRIDE)
        );
        scarlet::println!(
            "tegra-dc: layout kind={:#x} cde={:#x}/{:#x} activation={:#010x} vcounter-mask={:#x}",
            self.read(0x80b),
            self.read(CDE_CONTROL),
            self.read(CDE_CG_SW_OVERRIDE),
            self.read(ACT_CONTROL),
            self.activation_mask()
        );
        scarlet::println!(
            "tegra-dc: fetch uf={:#010x}/{:#010x} delta={}/{} intr={:#010x} priority={:#010x}/{:#010x}",
            underflow_a,
            underflow_b,
            underflow_a.wrapping_sub(previous_a),
            underflow_b.wrapping_sub(previous_b),
            self.read(INT_STATUS),
            self.read(MEM_HIGH_PRIORITY),
            self.read(MEM_HIGH_PRIORITY_TIMER)
        );
        scarlet::println!(
            "tegra-dc: sticky-mc={:#010x} err={:#010x}/{:#010x} la-ab={:#010x} scaled-la={:#010x}/{:#010x}",
            mc_read(0),
            mc_read(8),
            mc_read(0xc),
            mc_read(0x2e8),
            mc_read(0x690),
            mc_read(0x698)
        );
    }
}

impl InterruptCapableDevice for Display {
    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.interrupt_id)
    }
    fn handle_interrupt(&self) -> InterruptResult<()> {
        self.claim_interrupt().map(|_| ())
    }
    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        let completed = {
            let _irq = self.irq_lock.lock();
            self.retire_flip_locked(self.read(INT_STATUS))
        };
        if completed {
            self.flip_waker.wake_one();
        }
        // Dedicated DC0 line, including a controller delivery after masking.
        Ok(InterruptClaim::Handled)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        if self.state.get_mut().changed && self.restore().is_err() {
            // A failed flip may still become active. Retain every allocation
            // the controller could fetch, including the attempted GPU image.
            core::mem::forget(self.buffers.take());
            core::mem::forget(self.scanout.take());
            core::mem::forget(self.state.get_mut().gpu_front.take());
            core::mem::forget(self.state.get_mut().pending_gpu.take());
            return;
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
        // These are CPU-only linear render/upload sources. DC fetches the
        // private Normal-NC allocations, never these cacheable mmap buffers.
        // Keep the HHDM and application aliases on the same Normal attribute.
        MemoryAttribute::Normal
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
            return Err("Tegra DC buffer is the current presentation source");
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
            return Err("Tegra DC presentation requires a GPU swapchain image");
        }
        let (paddr, stride, format, direct) = if let Some(backing) = resource.modified_backing() {
            if backing.modifier() != GPU_IMAGE_MODIFIER_NVIDIA_BLOCK_LINEAR_16BX2_H4
                || backing.stride() != STRIDE
                || backing.allocation_size() < block_linear::SIZE as u64
            {
                return Err("unsupported Tegra DC modified GPU scanout layout");
            }
            (
                backing.physical_addr(),
                backing.stride(),
                backing.format(),
                true,
            )
        } else {
            let backing = resource
                .linear_backing()
                .ok_or("Tegra DC requires GPU backing")?;
            if backing.physical_segments().len() != 1
                || backing.stride() < STRIDE
                || backing.stride() > 0xffff
                || backing.stride() & 63 != 0
                || backing.allocation_size() < u64::from(backing.stride()) * u64::from(HEIGHT)
            {
                return Err("unsupported Tegra DC linear GPU scanout layout");
            }
            (
                backing.physical_addr(),
                backing.stride(),
                backing.format(),
                false,
            )
        };
        let required = if direct {
            block_linear::SIZE as u64
        } else {
            u64::from(stride) * u64::from(HEIGHT)
        };
        if resource.width() != WIDTH
            || resource.height() != HEIGHT
            || paddr & 0xfff != 0
            || paddr.checked_add(required).is_none_or(|end| end > 1 << 34)
            || !matches!(format, PixelFormat::BGRA8888 | PixelFormat::XRGB8888)
        {
            return Err("unsupported Tegra DC GPU scanout layout");
        }
        let mut state = self.state.lock();
        if state.lost {
            return Err("Tegra DC display is lost");
        }
        // The producer must retire rendering and publish its writes before
        // presentation. Cleaning a stale CPU alias here could overwrite a
        // GPU-rendered frame. This controller only consumes the ready image.
        arch::io_mb();
        state.pending_gpu = Some(resource);
        self.diagnostic_frames.fetch_add(1, Ordering::Relaxed);
        let scanout = state.scanout_front ^ 1;
        let scanout_address = if direct {
            if self.diagnostic_frames.load(Ordering::Relaxed) <= 4 {
                scarlet::println!(
                    "tegra-dc: direct GPU block-linear scanout paddr={:#x}",
                    paddr
                );
            }
            paddr
        } else {
            let address = self.scanout.as_ref().unwrap()[scanout].as_paddr();
            self.upload_frame(paddr, stride, address, true)?;
            address
        };
        state.changed = true;
        if let Err(error) = self.flip(
            scanout_address,
            format,
            !state.initialized || self.keep_console,
        ) {
            state.lost = true;
            scarlet::println!("tegra-dc: GPU page flip failed: {}", error);
            return Err(error);
        }
        if !direct {
            state.scanout_front = scanout;
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
        scarlet::println!("tegra-dc: activating native scanout");
        self.present_buffer(&mut state, 0)?;
        self.validate_initial_scanout()?;
        // The firmware's linear surface stays in B through initialization.
        // A block-linear surface cannot be adopted as an earlyfb linear map.
        // The first ordinary userspace present retires B unless keep_bootcon.
        state.initialized = true;
        scarlet::println!(
            "tegra-dc: native scanout active; DC90, block-linear kind=0x42, render/scanout=2/2, V-counter, render=Normal scanout=Normal-NC"
        );
        if self.keep_console {
            scarlet::println!("tegra-dc: keep_bootcon; boot logs remain visible in window B");
        }
        Ok(())
    }
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if REGISTERED.load(Ordering::Acquire) {
        return Err("Tegra DC is already registered");
    }
    let irq = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::IRQ)
        .ok_or("Tegra DC IRQ missing")?;
    let interrupt_id = resolve_platform_irq(irq).map_err(|_| "Tegra DC IRQ resolution failed")?;
    let resource = device
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM && r.start == 0x54200000)
        .ok_or("only the inherited DC0 DSI scanout is supported")?;
    if resource.size()? < 0x40000 || cell(device, "scarlet,boot-scanout", 0) != Some(1) {
        return Err("missing Tegra DC firmware scanout contract");
    }
    let car = vm::ioremap(0x60006000, 0x20)?;
    let mc = vm::ioremap(0x70019000, 0x1000)?;
    let read = |base, offset| unsafe { arch::mmio::read32(base + offset) };
    if read(car, 0x10) & (1 << 27) == 0 || read(car, 4) & (1 << 27) != 0 {
        return Err("inherited Tegra DC clock/reset is not active");
    }
    // Noble DC0 names both DC and DC1 SWGROUPs. The global enable register
    // belongs to TrustZone; conservatively require both per-group enables
    // clear before publishing physical scanout. Never change either domain.
    let dc_asid = read(mc, 0x240);
    let dc1_asid = read(mc, 0xa88);
    scarlet::println!(
        "tegra-dc: MC dc_asid={:#010x} dc1_asid={:#010x}",
        dc_asid,
        dc1_asid
    );
    if dc_asid == u32::MAX || dc1_asid == u32::MAX {
        return Err("DC SMMU domain registers are unreadable");
    }
    if (dc_asid | dc1_asid) & (1 << 31) != 0 {
        return Err("DC0 is attached to an inherited SMMU domain");
    }
    // Direct A/B underflow counters live beyond the window-selected bank.
    let base = vm::ioremap(resource.start, 0x4000)?;
    let dc_read = |r| read(base, r * 4);
    let dc_write = |r, v| unsafe {
        arch::mmio::write32(base + r * 4, v);
        arch::io_mb();
    };
    let access = dc_read(STATE_ACCESS);
    let header = dc_read(HEADER);
    let request = ACT_REQ | (1 << 2);
    if dc_read(STATE_CONTROL) & request != 0 {
        return Err("inherited Tegra DC has a pending update");
    }
    dc_write(STATE_ACCESS, 1);
    dc_write(HEADER, 1 << 4);
    let original = REGISTERS.map(dc_read);
    let kind = dc_read(0x80b);
    let original_priority = [dc_read(MEM_HIGH_PRIORITY), dc_read(MEM_HIGH_PRIORITY_TIMER)];
    let original_activation = dc_read(ACT_CONTROL);
    let underflow_a = dc_read(0xbca);
    let underflow_b = dc_read(0xdca);
    dc_write(HEADER, 1 << 5);
    let original_console_window = REGISTERS.map(dc_read);
    let original_console_kind = dc_read(0x80b);
    dc_write(HEADER, 1 << 4);
    let display_command = dc_read(0x32);
    let active = dc_read(0x409);
    dc_write(HEADER, header);
    dc_write(STATE_ACCESS, access);
    if original_console_window[0] & WIN_ENABLE != 0 {
        return Err("boot console handoff requires an unused Tegra DC window B");
    }
    scarlet::println!(
        "tegra-dc: inherited addr={:#x}/{:#x} options={:#x} kind={:#x} mode={:#x} active={:#x}",
        original[11],
        original[10],
        original[0],
        kind,
        display_command,
        active
    );
    scarlet::println!(
        "tegra-dc: inherited color={} size={:#x} prescale={:#x} dda={:#x} stride={} offsets={}/{}",
        original[2],
        original[4],
        original[5],
        original[8],
        original[9],
        original[12],
        original[13]
    );
    scarlet::println!(
        "tegra-dc: inherited activation={:#010x} cde={:#x}/{:#x}",
        original_activation,
        original[22],
        original[23]
    );
    scarlet::println!(
        "tegra-dc: inherited fetch uf={:#010x}/{:#010x} priority={:#010x}/{:#010x} sticky-mc={:#010x} err={:#010x}/{:#010x}",
        underflow_a,
        underflow_b,
        original_priority[0],
        original_priority[1],
        read(mc, 0),
        read(mc, 8),
        read(mc, 0xc)
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
    let mut scanout = [
        ContiguousPages::new_aligned(block_linear::SIZE.div_ceil(4096), 128 * 1024)
            .ok_or("Tegra DC block-linear allocation failed")?,
        ContiguousPages::new_aligned(block_linear::SIZE.div_ceil(4096), 128 * 1024)
            .ok_or("Tegra DC block-linear allocation failed")?,
    ];
    for (index, memory) in buffers.iter_mut().chain(scanout.iter_mut()).enumerate() {
        if memory
            .as_paddr()
            .checked_add(memory.len() as u64 * 4096)
            .is_none_or(|end| end > 1 << 34)
        {
            return Err("Tegra DC scanout exceeds 34-bit DMA address range");
        }
        scarlet::println!(
            "tegra-dc: preparing {} buffer {} paddr={:#x} bytes={} attribute={:?}",
            if index < 2 { "render" } else { "block-linear" },
            index % 2,
            memory.as_paddr(),
            memory.len() * 4096,
            if index < 2 {
                MemoryAttribute::Normal
            } else {
                MemoryAttribute::NonCacheable
            }
        );
        if index >= 2 {
            // Only DC's private front/back require uncached CPU writes.
            // Allocation zeroes the complete padded extent; retagging cleans
            // those stores before replacing the HHDM alias with Normal-NC.
            memory.retag_memory_attribute(MemoryAttribute::NonCacheable)?;
        }
    }
    scarlet::println!("tegra-dc: scanout buffers ready; preserving boot frame");
    // Convert the inherited portrait boot frame once during adoption.
    // Subsequent images keep these coordinates in block-linear storage;
    // DC performs the display rotation.
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
    let event_mask = FRAME_END | VBLANK | WINDOW_A_FETCH_EVENTS | WINDOW_B_FETCH_EVENTS;
    let display = Arc::new(Display {
        base,
        mc,
        config: FramebufferConfig::new(WIDTH, HEIGHT, PixelFormat::BGRA8888),
        buffers: Some(buffers),
        scanout: Some(scanout),
        original,
        original_console_window,
        original_console_kind,
        original_kind: kind,
        event_mask,
        original_event_enable: dc_read(INT_ENABLE) & event_mask,
        original_event_mask: dc_read(INT_MASK) & event_mask,
        original_priority,
        original_activation,
        front: AtomicUsize::new(0),
        gpu_active: AtomicBool::new(false),
        keep_console: earlyfb::keep_boot_console(),
        diagnostic_frames: AtomicUsize::new(0),
        last_underflow_a: AtomicU32::new(underflow_a),
        last_underflow_b: AtomicU32::new(underflow_b),
        interrupt_id,
        irq_ready: AtomicBool::new(false),
        irq_lock: IrqSpinLock::new(()),
        flip_pending: AtomicBool::new(false),
        flip_complete: AtomicBool::new(false),
        flip_waker: Waker::new_uninterruptible("tegra-dc-flip"),
        delayed_flip_irqs: AtomicUsize::new(0),
        state: Mutex::new(State {
            initialized: false,
            changed: false,
            lost: false,
            front: 0,
            scanout_front: 1,
            gpu_front: None,
            pending_gpu: None,
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
    match register_and_enable_platform_irq_device(
        irq,
        display.clone(),
        arch::get_cpu().get_cpuid() as u32,
    ) {
        Ok(_) => {
            display.irq_ready.store(true, Ordering::Release);
            scarlet::println!("tegra-dc: FRAME_END interrupt page-flip retirement enabled");
        }
        Err(_) => {
            scarlet::println!("tegra-dc: IRQ registration failed; polled page-flip retirement")
        }
    }
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

// SPDX-License-Identifier: GPL-2.0-only
use crate::engine::{Engine, Transport, output_descriptor};
use alloc::{boxed::Box, sync::Arc, vec};
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};
use scarlet::{
    arch,
    device::{
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
    },
    random::{EntropySource, RandomManager},
    sync::SpinLock,
};
use scarlet_driver_tegra210::{Mmio, cell, delay_us, enable_se_clocks};

const OUTPUT_OFFSET: usize = 128;
#[repr(C, align(4096))]
struct DmaPage(UnsafeCell<[u8; 4096]>);
// Exactly one transport takes this page, and its Engine is serialized by a
// SpinLock. No reference into the page is exposed while DMA owns it.
unsafe impl Sync for DmaPage {}
static DMA: DmaPage = DmaPage(UnsafeCell::new([0; 4096]));
static CLAIMED: AtomicBool = AtomicBool::new(false);

struct Hardware {
    regs: Mmio,
    ahb: Mmio,
    dma_phys: u32,
}
impl Hardware {
    fn address(&self) -> usize {
        DMA.0.get() as usize
    }
}
impl Transport for Hardware {
    fn read(&self, offset: usize) -> u32 {
        self.regs.read(offset)
    }
    fn write(&mut self, offset: usize, value: u32) {
        self.regs.write(offset, value);
    }
    fn now_ns(&self) -> u64 {
        scarlet::time::current_time_ns()
    }
    fn delay_us(&self, us: u64) {
        delay_us(us);
    }
    fn ahb_pending(&self) -> u32 {
        self.ahb.read(0xfc)
    }
    fn prepare_output(&mut self, size: usize) -> Result<u32, &'static str> {
        let words = output_descriptor(u64::from(self.dma_phys) + OUTPUT_OFFSET as u64, size)?;
        let address = self.address();
        unsafe {
            for (i, word) in words.into_iter().enumerate() {
                core::ptr::write_volatile((address + i * 4) as *mut u32, word);
            }
            // A missing DMA write must not accidentally return an old block.
            core::ptr::write_bytes((address + OUTPUT_OFFSET) as *mut u8, 0, size);
        }
        arch::clean_dcache_to_poc_range(address, 12);
        arch::clean_invalidate_dcache_to_poc_range(address + OUTPUT_OFFSET, size);
        arch::io_mb();
        Ok(self.dma_phys)
    }
    fn copy_output(&mut self, output: &mut [u8]) {
        arch::io_mb();
        let address = self.address() + OUTPUT_OFFSET;
        arch::invalidate_dcache_to_poc_range(address, output.len());
        for (i, byte) in output.iter_mut().enumerate() {
            *byte = unsafe { core::ptr::read_volatile((address + i) as *const u8) };
        }
    }
}

struct Rng(SpinLock<Engine<Hardware>>);
impl EntropySource for Rng {
    fn name(&self) -> &'static str {
        "tegra210-se"
    }
    fn is_available(&self) -> bool {
        self.0.lock().is_available()
    }
    fn read_entropy(&self, output: &mut [u8]) -> usize {
        let mut engine = self.0.lock();
        if !engine.is_available() {
            return 0;
        }
        match engine.read_entropy(output) {
            Ok(count) => count,
            Err(error) => {
                scarlet::println!("tegra210-se-rng: disabled: {}", error);
                0
            }
        }
    }
}

fn probe(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = d
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM && r.start == 0x70012000)
        .ok_or("unexpected SE resource")?;
    if resource.size()? < 0x808 {
        return Err("truncated SE resource");
    }
    let provider = cell(d, "clocks", 0).ok_or("SE clock provider missing")?;
    if !matches!(cell(d, "clocks", 1), Some(127 | 405))
        || cell(d, "clocks", 2) != Some(provider)
        || cell(d, "clocks", 3) != Some(149)
    {
        return Err("unexpected SE/entropy clocks");
    }
    // Physical AHB descriptors require identity DMA. Do not reprogram an
    // inherited SMMU or disturb the other engines' memory mappings.
    let mc = Mmio::map(0x70019000, 0x1000)?;
    let smmu = mc.read(0x10);
    if smmu == u32::MAX || (smmu & 1 != 0 && (mc.read(0xabc) | mc.read(0xac8)) & (1 << 31) != 0) {
        return Err("SE inherited SMMU translation is unsupported");
    }
    enable_se_clocks(provider)?;
    let regs = Mmio::map(resource.start, resource.size()?)?;
    let ahb = Mmio::map(0x6000c000, 0x1000)?;
    // The page is part of the low-address L4T kernel image, so it stays within
    // SE's 32-bit DMA aperture even when the page allocator uses RAM above 4G.
    // It is never freed or reused after a failed/aborted DMA transaction.
    let physical = scarlet::vm::virt_to_phys(DMA.0.get() as usize);
    if physical & 4095 != 0
        || physical
            .checked_add(4096)
            .is_none_or(|end| end > 1u64 << 32)
    {
        return Err("SE DMA page is outside the 32-bit aperture");
    }
    CLAIMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "SE RNG already claimed")?;
    let engine = Engine::initialize(Hardware {
        regs,
        ahb,
        dma_phys: physical as u32,
    })?;
    RandomManager::register_entropy_source(Arc::new(Rng(SpinLock::new(engine))));
    scarlet::println!("tegra210-se-rng: hardware entropy ready");
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("SE RNG remains registered")
}
fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(
            PlatformDeviceDriver::new("tegra210-se-rng", probe, remove, vec!["nvidia,tegra210-se"])
                .with_probe_options(PlatformProbeOptions {
                    deassert_resets: false,
                    resolve_iommu: false,
                    resolve_dma: false,
                }),
        ),
        DriverPriority::Standard,
    );
}
scarlet::driver_initcall!(register);
#[used]
static LINK: fn() = register;
pub fn force_link() {
    let _ = LINK;
}

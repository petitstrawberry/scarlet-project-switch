// SPDX-License-Identifier: GPL-2.0-only
use crate::{codec::Codec, pcm::Backend};
use alloc::{boxed::Box, sync::Arc, vec};
use scarlet::{
    device::{
        audio::{AUDIO_DEVICE_KIND_SPEAKERS, AudioDeviceInfo, register_playback_device_with_info},
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions},
    },
    sync::{IrqSpinLock, SpinLock},
};
use scarlet_driver_tegra210::{
    audio_platform, cell, delay_us, gpio_for, pad, sleep_ms, spawn_worker,
};

static BACKEND: IrqSpinLock<Option<Arc<Backend>>> = IrqSpinLock::new(None);

fn codec_probe(d: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if cell(d, "reg", 0) != Some(0x1c)
        || cell(d, "realtek,eq-config-type", 0) != Some(0)
        || cell(d, "realtek,ldo1-en-gpios", 1) != Some(204)
    {
        return Err("unsupported RT5639 board wiring");
    }
    let bus = DeviceManager::get_manager()
        .get_i2c_bus(d.parent_phandle().ok_or("codec bus missing")?)
        .ok_or(PROBE_DEFER)?;
    if bus.bus_number() != 1 {
        return Err("RT5639 must be on I2C1");
    }
    let gpio = gpio_for(cell(d, "realtek,ldo1-en-gpios", 0).ok_or("codec GPIO provider missing")?)?;
    pad(0x28c, 5)?;
    gpio.output(204, true)?;
    delay_us(400_000);
    let codec = Arc::new(Codec {
        bus,
        lock: SpinLock::new(()),
    });
    codec.initialize()?;
    register_sound(codec)
}
fn completion_worker() {
    loop {
        let backend = BACKEND.lock().clone();
        let active = backend.is_some_and(|b| b.service());
        // ADMA clocks the stream independently. This sleeping worker observes
        // hardware period counters; it never busy-waits or drives the clock.
        sleep_ms(if active { 2 } else { 20 });
    }
}
fn register_sound(codec: Arc<Codec>) -> Result<(), &'static str> {
    if BACKEND.lock().is_some() {
        return Err("audio already registered");
    }
    let platform = audio_platform()?;
    let backend = Arc::new(Backend::new(platform, codec)?);
    *BACKEND.lock() = Some(backend.clone());
    let name = register_playback_device_with_info(
        backend,
        AudioDeviceInfo::new(
            AUDIO_DEVICE_KIND_SPEAKERS,
            "Switch speakers",
            "Tegra210 I2S1 / Realtek RT5639",
        ),
    );
    spawn_worker("tegra-audio", completion_worker);
    scarlet::println!("tegra210-audio: /dev/{} registered", name);
    Ok(())
}
fn register() {
    // This machine driver matches the Icosa codec wiring (I2C1, PZ4, EQ 0).
    // It owns APE/I2S1 clock sequencing, independent of Linux's sound-card
    // assigned-clock policy. The common PCM interface remains unchanged.
    let driver = PlatformDeviceDriver::new(
        "tegra210-rt5639",
        codec_probe,
        |_| Err("audio is registered"),
        vec!["realtek,rt5639"],
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

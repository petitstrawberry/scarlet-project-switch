// SPDX-License-Identifier: GPL-2.0-only
//! Lifecycle of the physical ODIN dock connector. The display service can
//! enumerate either output while the worker negotiates the external link.
use crate::{
    dock_surface::{Plane, Surface},
    dp_link::{Caps, Link},
    dpaux::DpAux,
    head::{self, Head, Timing},
    sor::{OutputOps, Sor},
};
use alloc::{sync::Arc, vec::Vec};
use scarlet::{
    arch,
    device::{
        graphics::{
            GpuDisplayResource,
            monitor::MonitorIdentity,
            output::{ConnectorInfo, ConnectorKind, DisplayMode},
        },
        platform::{PlatformDeviceInfo, resource::PlatformDeviceResourceType},
    },
    mem::page::ContiguousPages,
    sync::{IrqSpinLock, Mutex},
    time::current_time_ns,
    vm,
};
use scarlet_driver_tegra210::{
    ExternalDpPlatform, Mmio, cell,
    dock::{self, Route},
    external_dp_platform,
};

pub const DC1: u64 = 0x5424_0000;
const SOR1: u64 = 0x5458_0000;
const AUX1: u64 = 0x5404_0000;
const DISCONNECTED: &str = "Switch dock display is disconnected";
const RETRY_NS: u64 = 1_000_000_000;
const HPD_SAMPLE_NS: u64 = 250_000_000;

struct Output {
    epoch: u64,
    surface: Arc<Surface>,
    identity: Option<MonitorIdentity>,
    modes: Vec<Timing>,
    requested: IrqSpinLock<Option<Timing>>,
}
// The display ioctl snapshots its backing under an IRQ guard. This lock
// only transfers the published reference; all hardware work may sleep.
static OUTPUT: IrqSpinLock<Option<Arc<Output>>> = IrqSpinLock::new(None);
static HARDWARE: Mutex<Option<Hardware>> = Mutex::new(None);

fn published() -> Option<Arc<Output>> {
    let output = OUTPUT.lock().clone()?;
    let route = dock::snapshot();
    (route.permits_probe() && route.epoch == output.epoch).then_some(output)
}

fn withdraw() {
    let previous = OUTPUT.lock().take();
    drop(previous);
}

pub fn info() -> ConnectorInfo {
    let published = published();
    let out = published.as_ref();
    ConnectorInfo {
        index: 1,
        name: "Switch dock DP".into(),
        kind: ConnectorKind::External,
        connected: out.is_some(),
        width: out.map_or(0, |o| o.surface.mode.width),
        height: out.map_or(0, |o| o.surface.mode.height),
        refresh_mhz: out.map_or(0, |o| o.surface.mode.refresh_millihz()),
        physical_size_mm: out.and_then(|o| o.identity.as_ref()?.physical_size_mm),
    }
}

fn display_mode(t: Timing) -> DisplayMode {
    DisplayMode {
        width: t.width,
        height: t.height,
        refresh_millihz: t.refresh_millihz(),
    }
}
pub fn modes() -> Vec<DisplayMode> {
    published()
        .map(|o| o.modes.iter().copied().map(display_mode).collect())
        .unwrap_or_default()
}
pub fn identity() -> Option<MonitorIdentity> {
    published().and_then(|o| o.identity.clone())
}
pub fn display() -> Result<Arc<Surface>, &'static str> {
    published().map(|o| o.surface.clone()).ok_or(DISCONNECTED)
}
pub fn request_mode(mode: DisplayMode) -> Result<(), &'static str> {
    let out = published().ok_or(DISCONNECTED)?;
    let timing = *out
        .modes
        .iter()
        .find(|t| display_mode(**t) == mode)
        .ok_or("unavailable dock mode")?;
    *out.requested.lock() = (timing != out.surface.mode).then_some(timing);
    dock::wake_worker();
    Ok(())
}

struct Hardware {
    platform: ExternalDpPlatform,
    head: Head,
    aux: DpAux<Mmio>,
    sor: Sor<Mmio>,
    acquired: bool,
    head_started: bool,
    aux_epoch: Option<u64>,
    surface: Option<Arc<Surface>>,
    scanout: [Option<ContiguousPages>; 2],
    front_cpu: usize,
    front_gpu: Option<GpuDisplayResource>,
    pending_gpu: Option<GpuDisplayResource>,
    epoch: Option<u64>,
    failed: bool,
}
struct RouteOps<'a> {
    platform: &'a mut ExternalDpPlatform,
    head: &'a Head,
}
impl OutputOps for RouteOps<'_> {
    fn set_pad_power(&mut self, on: bool) -> Result<(), &'static str> {
        self.platform.set_pad_power(on)
    }
    fn select_pad_clock(&mut self, pad: bool) -> Result<(), &'static str> {
        self.platform.select_link_clock(pad)
    }
    fn route_dc(&mut self, on: bool) -> Result<(), &'static str> {
        self.head.route(on)
    }
}
impl Hardware {
    fn prepare_aux(&mut self, epoch: u64) -> Result<(), &'static str> {
        if self.aux_epoch == Some(epoch) {
            return Ok(());
        }
        self.acquired = true;
        self.platform.prepare_aux()?;
        self.aux.enable();
        self.aux_epoch = Some(epoch);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), &'static str> {
        withdraw();
        self.epoch = None;
        self.aux_epoch = None;
        if !self.acquired {
            return Ok(());
        }
        let mut ops = RouteOps {
            platform: &mut self.platform,
            head: &self.head,
        };
        if self.sor.needs_disable() {
            self.sor.disable(
                &mut self.aux,
                &mut ops,
                current_time_ns().saturating_add(500_000_000),
            )?;
        }
        if self.head_started {
            self.head.stop()?;
            self.head_started = false;
        }
        self.aux.disable();
        self.platform.restore()?;
        self.acquired = false;
        self.surface = None;
        self.scanout = [None, None];
        self.front_cpu = 0;
        self.front_gpu = None;
        self.pending_gpu = None;
        self.failed = false;
        Ok(())
    }

    fn read_monitor(&mut self) -> Result<(Vec<Timing>, Option<MonitorIdentity>), &'static str> {
        let end = current_time_ns().saturating_add(1_000_000_000);
        let mut base = [0; 128];
        self.aux.read_edid_block(0, &mut base, end)?;
        let identity = MonitorIdentity::from_edid(&base).ok_or("invalid dock EDID")?;
        let mut edid = base.to_vec();
        for number in 1..=base[126].min(8) {
            let mut extension = [0; 128];
            self.aux.read_edid_block(number, &mut extension, end)?;
            edid.extend_from_slice(&extension);
        }
        let caps = Caps::read(&mut self.aux, end)?;
        let max_link = Link {
            rate: caps.rate,
            lanes: caps.lanes,
            enhanced: caps.enhanced,
        };
        let mut modes = head::monitor_modes(&edid)?;
        modes.retain(|t| max_link.transfer_unit(t).is_some());
        if modes.is_empty() {
            return Err("no advertised timing fits the dock link");
        }
        Ok((modes, Some(identity)))
    }
    fn start(&mut self, t: Timing) -> Result<(), &'static str> {
        let surface = Surface::new(t)?;
        let first = surface.cpu_plane(&mut self.scanout[0], false)?;
        let divider = self.platform.prepare_mode(t.clock_khz * 1000)?;
        // Hold all backing from the first DC write. A partial latch can fetch
        // the new address even if start times out.
        self.surface = Some(surface);
        self.head_started = true;
        self.head.start(t, first.address, divider)?;
        let mut ops = RouteOps {
            platform: &mut self.platform,
            head: &self.head,
        };
        self.sor.enable(
            &mut self.aux,
            &t,
            &mut ops,
            current_time_ns().saturating_add(1_000_000_000),
        )
    }

    fn connect(&mut self, route: Route, preferred: Option<Timing>) -> Result<(), &'static str> {
        let (modes, identity) = self.read_monitor()?;
        let mut order = modes.clone();
        if let Some(i) = order.iter().position(|t| Some(*t) == preferred) {
            order.swap(0, i);
        }
        let mut failure = "no trainable dock mode";
        for (i, t) in order.into_iter().enumerate() {
            if dock::snapshot().epoch != route.epoch {
                return Err("dock changed during training");
            }
            if i != 0 {
                self.prepare_aux(route.epoch)?;
            }
            match self.start(t) {
                Ok(()) => {
                    if dock::snapshot().epoch != route.epoch {
                        return Err("dock changed during modeset");
                    }
                    self.epoch = Some(route.epoch);
                    let output = Arc::new(Output {
                        epoch: route.epoch,
                        surface: self.surface.as_ref().unwrap().clone(),
                        identity,
                        modes,
                        requested: IrqSpinLock::new(None),
                    });
                    *OUTPUT.lock() = Some(output);
                    scarlet::println!(
                        "tegra-dock: connected {}x{} at {} mHz",
                        t.width,
                        t.height,
                        t.refresh_millihz()
                    );
                    return Ok(());
                }
                Err(err) => {
                    failure = err;
                    self.stop()?;
                }
            }
        }
        Err(failure)
    }

    fn link_alive(&mut self) -> bool {
        let end = current_time_ns().saturating_add(40_000_000);
        let mut lanes = [0];
        let mut status = [0; 3];
        if !self.aux.hpd()
            || self.aux.read_dpcd(0x101, &mut lanes, end).is_err()
            || self.aux.read_dpcd(0x202, &mut status, end).is_err()
        {
            return false;
        }
        let lanes = lanes[0] & 31;
        // Per-lane clock recovery, channel EQ and symbol lock, plus interlane alignment.
        matches!(lanes, 1 | 2)
            && status[2] & 1 != 0
            && status[2] & 0x40 == 0
            && (0..lanes).all(|i| (status[0] >> (i * 4)) & 7 == 7)
    }

    fn update(&mut self, route: Route, event: bool) -> Result<(), &'static str> {
        let request = published()
            .filter(|o| o.epoch == route.epoch)
            .and_then(|o| o.requested.lock().take());
        if !route.permits_probe() {
            return self.stop();
        }
        if self.acquired
            && (self.aux_epoch != Some(route.epoch) || self.failed || request.is_some())
        {
            self.stop()?;
        }
        let result = (|| {
            self.prepare_aux(route.epoch)?;
            // A dock may stay connected to USB while its HDMI monitor is
            // unplugged. DPAUX1 samples physical HPD even when BM92T supplies
            // neither an Attention nor a reliable SIGNAL_ON transition.
            if !self.aux.hpd() {
                if self.head_started {
                    self.stop()?;
                }
                return Ok(());
            }
            if self.epoch == Some(route.epoch) {
                if !event || self.link_alive() {
                    return Ok(());
                }
                self.stop()?;
                self.prepare_aux(route.epoch)?;
            }
            self.connect(route, request)
        })();
        if result.is_err() {
            self.stop()?;
        }
        result
    }

    fn check_surface(&self, surface: &Surface) -> Result<(), &'static str> {
        let route = dock::snapshot();
        if self.failed
            || !route.permits_probe()
            || self.epoch != Some(route.epoch)
            || !self
                .surface
                .as_ref()
                .is_some_and(|s| core::ptr::eq(s.as_ref(), surface))
        {
            return Err(DISCONNECTED);
        }
        Ok(())
    }
    fn flip(&mut self, t: Timing, p: Plane) -> Result<(), &'static str> {
        arch::io_mb();
        if let Err(e) = self.head.flip(t, p.address, p.pitch, 12, p.kind) {
            self.failed = true;
            withdraw();
            dock::wake_worker();
            return Err(e);
        }
        self.front_gpu = self.pending_gpu.take();
        Ok(())
    }
}
pub fn present_cpu(s: &Surface) -> Result<(), &'static str> {
    let mut guard = HARDWARE.lock();
    let hw = guard.as_mut().ok_or(DISCONNECTED)?;
    hw.check_surface(s)?;
    let index = hw.front_cpu ^ 1;
    let plane = s.cpu_plane(&mut hw.scanout[index], true)?;
    hw.flip(s.mode, plane)?;
    hw.front_cpu = index;
    Ok(())
}
pub fn present_gpu(
    s: &Surface,
    plane: Plane,
    image: GpuDisplayResource,
) -> Result<(), &'static str> {
    let mut guard = HARDWARE.lock();
    let hw = guard.as_mut().ok_or(DISCONNECTED)?;
    hw.check_surface(s)?;
    hw.pending_gpu = Some(image);
    hw.flip(s.mode, plane)?;
    Ok(())
}
fn worker() {
    let mut last = None;
    let mut last_epoch = None;
    let mut reported = None;
    loop {
        let route = dock::snapshot();
        if last_epoch != Some(route.epoch) {
            scarlet::println!(
                "tegra-dock: Type-C lanes={} HPD={} source={}",
                route.lanes,
                route.hpd,
                if route.explicit_hpd {
                    "DP status"
                } else {
                    "BM92T signal"
                }
            );
            last_epoch = Some(route.epoch);
        }
        let result = HARDWARE
            .lock()
            .as_mut()
            .unwrap()
            .update(route, last != Some(route.sequence));
        let retry = match result {
            Err(err) => {
                if reported != Some((route.epoch, err)) {
                    scarlet::println!("tegra-dock: {}", err);
                    reported = Some((route.epoch, err));
                }
                Some(RETRY_NS)
            }
            Ok(()) => {
                reported = None;
                route.permits_probe().then_some(HPD_SAMPLE_NS)
            }
        };
        last = Some(route.sequence);
        dock::wait(route.sequence, retry);
    }
}

pub fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if cell(device, "scarlet,dock-display", 0) != Some(1)
        || cell(device, "iommus", 1) != Some(3)
        || !device.get_resources().iter().any(|r| {
            r.res_type == PlatformDeviceResourceType::MEM
                && r.start == DC1
                && r.size().unwrap_or(0) >= 0x40000
        })
    {
        return Err("missing ODIN DC1 dock wiring");
    }
    let mc = Mmio::map(0x70019000, 0x1000)?;
    if mc.read(0x244) & (1 << 31) != 0 {
        return Err("DCB SMMU is active or inaccessible");
    }
    let car = cell(device, "clocks", 0).ok_or("missing DISP2 clock provider")?;
    let mut guard = HARDWARE.lock();
    if guard.is_some() {
        return Err("DC1 already registered");
    }
    *guard = Some(Hardware {
        platform: external_dp_platform(car, DC1, SOR1, AUX1)?,
        head: Head(vm::ioremap(DC1, 0x4000)?),
        aux: DpAux::new(Mmio::map(AUX1, 0x1000)?),
        sor: Sor::new(Mmio::map(SOR1, 0x1000)?),
        acquired: false,
        head_started: false,
        aux_epoch: None,
        surface: None,
        scanout: [None, None],
        front_cpu: 0,
        front_gpu: None,
        pending_gpu: None,
        epoch: None,
        failed: false,
    });
    drop(guard);
    let task = scarlet::task::new_kernel_task("tegra210-dock".into(), 1, worker);
    task.init();
    scarlet::sched::scheduler::add_task(task, arch::get_cpu().get_cpuid());
    Ok(())
}

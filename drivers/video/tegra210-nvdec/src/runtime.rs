// SPDX-License-Identifier: GPL-2.0-only
//! One serialized, asynchronous NVDEC session. The video client polls dequeue;
//! no decode wait holds the state lock and no unrelated engine is reset.

use crate::{
    engine::Engine,
    h264::{Geometry, INPUT_BYTES, OUTPUT_BYTES, Pending, Session},
};
use alloc::{boxed::Box, format, string::String, sync::Arc, vec};
use core::sync::atomic::{AtomicBool, Ordering};
use scarlet::{
    arch,
    device::{
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, PlatformProbeOptions,
            resource::PlatformDeviceResourceType,
        },
        video::*,
    },
    sync::SpinLock,
    time,
};
use scarlet_driver_tegra210::{cell, nvdec_platform};

static REGISTERED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct Timing {
    width: usize,
    height: usize,
    submitted: u64,
    submit_ns: u64,
    completed: u64,
    ready_ns: u64,
    linear_ns: u64,
}

struct State {
    engine: Engine,
    session_id: Option<u32>,
    next_id: u32,
    session: Option<Session>,
    pending: Option<Pending>,
    frames: u64,
    last_status: [u32; 4],
    error: Option<&'static str>,
    timing: Timing,
}
impl State {
    fn fail(&mut self, error: &'static str) -> Result<(), &'static str> {
        self.error = Some(error);
        self.pending = None;
        // DMA backing is retained if reset/drain cannot be proven. That error
        // also prevents reopening the engine or recycling output references.
        self.engine.isolate()?;
        self.session = None;
        Ok(())
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if self.engine.isolate().is_err() {
            if let Some(session) = self.session.take() {
                core::mem::forget(session);
            }
        }
    }
}

struct Backend {
    state: SpinLock<State>,
}
impl VideoDecodeBackend for Backend {
    fn name(&self) -> &'static str {
        "tegra210-nvdec"
    }
    fn capabilities(&self) -> VideoBackendCapabilities {
        VideoBackendCapabilities {
            max_sessions: 1,
            max_inflight_decodes: 1,
            mapped_input_len: INPUT_BYTES as u32,
            mapped_output_len: OUTPUT_BYTES as u32,
            output_pixel_format: SCARLET_VIDEO_PIXEL_FORMAT_NV12,
            supports_stateless_h264: true,
            ..Default::default()
        }
    }
    fn supports_variable_mapped_buffers(&self) -> bool {
        true
    }
    fn debug_status(&self) -> Option<String> {
        let s = self.state.lock();
        Some(format!(
            " firmware={} frames={} pending={} status={:x?} last_error={} completion=syncpoint-poll size={}x{} samples={} avg_us(submit/ready/linear)={}/{}/{}",
            if s.engine.active() {
                "running"
            } else {
                "isolated"
            },
            s.frames,
            s.pending.is_some(),
            s.last_status,
            s.error.unwrap_or("none"),
            s.timing.width,
            s.timing.height,
            s.timing.completed,
            s.timing.submit_ns / s.timing.submitted.max(1) / 1000,
            s.timing.ready_ns / s.timing.completed.max(1) / 1000,
            s.timing.linear_ns / s.timing.completed.max(1) / 1000,
        ))
    }
    fn create_session(&self, coded_format: u32) -> Result<u32, &'static str> {
        if coded_format != SCARLET_VIDEO_FORMAT_H264 {
            return Err("NVDEC currently supports H.264 only");
        }
        let mut s = self.state.lock();
        if s.session_id.is_some() {
            return Err("NVDEC session already owned");
        }
        if s.error.is_some() {
            // An earlier failed drain leaves active=true; never free or reuse
            // those pages until a successful isolation handshake.
            s.engine.isolate()?;
            s.session = None;
            s.engine.boot()?;
        }
        let id = s.next_id;
        s.next_id = s
            .next_id
            .checked_add(1)
            .ok_or("NVDEC session IDs exhausted")?;
        s.session_id = Some(id);
        s.error = None;
        s.timing = Timing::default();
        Ok(id)
    }
    fn destroy_session(&self, id: u32) -> Result<(), &'static str> {
        let mut s = self.state.lock();
        if s.session_id != Some(id) {
            return Err("NVDEC session ID invalid");
        }
        if s.pending.is_some() || s.error.is_some() {
            s.engine.isolate()?;
            s.pending = None;
            s.session = None;
            s.error = Some("session reset");
        } else {
            s.session = None;
        }
        s.session_id = None;
        Ok(())
    }
    fn submit_decode(&self, _: &VideoBackendDecodeRequest) -> Result<(), &'static str> {
        Err("NVDEC needs stateless H.264 parameters")
    }
    fn submit_h264_stateless(
        &self,
        request: &VideoBackendH264StatelessRequest,
    ) -> Result<(), &'static str> {
        let started = time::current_time_ns();
        let geometry = Geometry::from_params(&request.h264)?;
        let mut s = self.state.lock();
        if s.session_id != Some(request.decode.stream_id) {
            return Err("NVDEC session ID invalid");
        }
        if s.pending.is_some() {
            return Err("NVDEC decode already in flight");
        }
        if s.error.is_some() {
            return Err("NVDEC session must be reopened after failure");
        }
        if s.session
            .as_ref()
            .is_none_or(|session| session.geometry != geometry)
        {
            if request.h264.decode_params.flags & SCARLET_VIDEO_H264_DECODE_PARAM_FLAG_IDR == 0 {
                return Err("NVDEC resolution changes require an IDR picture");
            }
            s.session = Some(Session::new(request.decode.stream_id, geometry)?);
        }
        let State {
            engine,
            session,
            pending,
            ..
        } = &mut *s;
        *pending = Some(session.as_mut().unwrap().submit(engine, request)?);
        s.timing.width = geometry.visible_width;
        s.timing.height = geometry.visible_height;
        s.timing.submitted += 1;
        s.timing.submit_ns += time::current_time_ns().saturating_sub(started);
        Ok(())
    }
    fn dequeue_frame(&self, id: u32) -> Result<Option<VideoBackendDecodedFrame>, &'static str> {
        let mut s = self.state.lock();
        if s.session_id != Some(id) {
            return Err("NVDEC session ID invalid");
        }
        if let Some(error) = s.error {
            return Err(error);
        }
        let Some(pending) = s.pending else {
            return Ok(None);
        };
        let status = s.session.as_ref().unwrap().status();
        s.last_status = status;
        if status[3] == u32::MAX || !s.engine.completed(pending.fence) {
            if time::current_time_ns().saturating_sub(pending.started) > 1_000_000_000 {
                let regs = s.engine.regs();
                scarlet::println!(
                    "nvdec: frame timeout status={:x?} idle={:#x} cpu={:#x} debug={:#x} irq={:#x} fence={}/{}",
                    status,
                    regs.read(0x104c),
                    regs.read(0x1100),
                    regs.read(0x1094),
                    regs.read(0x1008),
                    s.engine.platform.completion.value(),
                    pending.fence
                );
                s.fail("NVDEC frame timeout")?;
                return Err("NVDEC frame timeout");
            }
            return Ok(None);
        }
        arch::io_mb();
        let ready = time::current_time_ns();
        match s.session.as_ref().unwrap().finish(pending) {
            Ok(frame) => {
                s.timing.completed += 1;
                s.timing.ready_ns += ready.saturating_sub(pending.started);
                s.timing.linear_ns += time::current_time_ns().saturating_sub(ready);
                s.pending = None;
                s.frames += 1;
                // A sparse progress sample also survives abrupt application
                // termination, where userspace may not destroy its session.
                if s.timing.completed == 128 || s.timing.completed % 1024 == 0 {
                    scarlet::println!(
                        "nvdec: {}x{} samples={} avg_us(submit/ready/linear)={}/{}/{}",
                        s.timing.width,
                        s.timing.height,
                        s.timing.completed,
                        s.timing.submit_ns / s.timing.submitted.max(1) / 1000,
                        s.timing.ready_ns / s.timing.completed / 1000,
                        s.timing.linear_ns / s.timing.completed / 1000,
                    );
                }
                Ok(Some(frame))
            }
            Err(error) => {
                s.fail(error)?;
                Err(error)
            }
        }
    }
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if REGISTERED.load(Ordering::Acquire) {
        return Err("NVDEC already registered");
    }
    let aperture = device
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("NVDEC register resource missing")?;
    if aperture.start != 0x54480000 || aperture.size()? < 0x40000 {
        return Err("NVDEC register resource invalid");
    }
    let provider = cell(device, "clocks", 0).ok_or("NVDEC clock provider missing")?;
    if cell(device, "resets", 0) != Some(provider) || cell(device, "resets", 1) != Some(194) {
        return Err("NVDEC reset specifier invalid");
    }
    let engine = Engine::new(nvdec_platform(provider)?)?;
    let backend = Arc::new(Backend {
        state: SpinLock::new(State {
            engine,
            session_id: None,
            next_id: 1,
            session: None,
            pending: None,
            frames: 0,
            last_status: [0; 4],
            error: None,
            timing: Timing::default(),
        }),
    });
    let name = register_video_decode_device(backend);
    REGISTERED.store(true, Ordering::Release);
    scarlet::println!(
        "nvdec: /dev/{} registered; H.264 stateless, NV12, progressive 8-bit, max 1920x1088",
        name
    );
    Ok(())
}
fn register() {
    let driver = PlatformDeviceDriver::new(
        "tegra210-nvdec",
        probe,
        |_| Err("NVDEC decode backend is registered"),
        vec!["nvidia,tegra210-nvdec"],
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

// SPDX-License-Identifier: GPL-2.0-only
//! H.264 setup layout from NVIDIA nvdec_drv.h (9fdf5c406200). DPB lifetime,
//! scratch sizes and method sequence follow averne/FFmpeg caeec83b791b,
//! libavcodec/nvtegra_h264.c (Copyright 2024 averne, GPL-2.0-or-later).
//! C reference layout: sizeof=764, flags=176/180, DPB=192, scaling=448/544.

use crate::{
    engine::{Dma, Engine},
    layout::{align, linearize_plane},
};
use alloc::{sync::Arc, vec, vec::Vec};
use scarlet::device::graphics::{GpuBackingSegment, shared_image::*};
use scarlet::{arch, device::video::*, time};

pub const INPUT_BYTES: usize = 2 * 1024 * 1024;
pub const OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const STATUS: usize = 0x400;
const SLICES: usize = 0x1000;
const EOS: [u8; 16] = [0, 0, 1, 11, 0, 0, 0, 0, 0, 0, 1, 11, 0, 0, 0, 0];

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    pub pitch: usize,
    pub chroma: usize,
    pub size: usize,
    pub visible_width: usize,
    pub visible_height: usize,
    pub crop_x: usize,
    pub crop_y: usize,
}
impl Geometry {
    pub fn from_params(params: &ScarletVideoH264StatelessParams) -> Result<Self, &'static str> {
        let s = &params.sps;
        let p = &params.pps;
        if s.chroma_format_idc != 1
            || s.bit_depth_luma_minus8 != 0
            || s.bit_depth_chroma_minus8 != 0
            || s.flags & SCARLET_VIDEO_H264_SPS_FLAG_FRAME_MBS_ONLY == 0
            || s.flags & SCARLET_VIDEO_H264_SPS_FLAG_SEPARATE_COLOUR_PLANE != 0
            || p.num_slice_groups_minus1 != 0
            || s.max_num_ref_frames > 16
            || s.log2_max_frame_num_minus4 > 12
            || s.log2_max_pic_order_cnt_lsb_minus4 > 12
            || !matches!(s.pic_order_cnt_type, 0 | 2)
            || p.num_ref_idx_l0_default_active_minus1 > 31
            || p.num_ref_idx_l1_default_active_minus1 > 31
            || p.weighted_bipred_idc > 2
            || !(-26..=25).contains(&p.pic_init_qp_minus26)
            || !(-12..=12).contains(&p.chroma_qp_index_offset)
            || !(-12..=12).contains(&p.second_chroma_qp_index_offset)
        {
            return Err("NVDEC requires progressive 8-bit 4:2:0 H.264, POC 0/2, no slice groups");
        }
        let width = (usize::from(s.pic_width_in_mbs_minus1) + 1) * 16;
        let height = (usize::from(s.pic_height_in_map_units_minus1) + 1) * 16;
        if width > 1920 || height > 1088 {
            return Err("NVDEC H.264 exceeds 1920x1088");
        }
        let crop = s.flags & SCARLET_VIDEO_H264_SPS_FLAG_FRAME_CROPPING != 0;
        let crops = if crop {
            [
                s.frame_crop_left_offset,
                s.frame_crop_right_offset,
                s.frame_crop_top_offset,
                s.frame_crop_bottom_offset,
            ]
        } else {
            [0; 4]
        };
        if crops.iter().any(|value| *value > 1088) {
            return Err("NVDEC frame crop invalid");
        }
        let [left, right, top, bottom] = crops.map(|value| value as usize * 2);
        if left + right >= width || top + bottom >= height {
            return Err("NVDEC frame crop is empty");
        }
        let pitch = align(width, 256);
        let chroma = pitch * align(height, 16);
        Ok(Self {
            width,
            height,
            pitch,
            chroma,
            size: chroma + pitch * align(height / 2, 16),
            visible_width: width - left - right,
            visible_height: height - top - bottom,
            crop_x: left,
            crop_y: top,
        })
    }
}

struct Frame {
    timestamp: u64,
    dpb: Option<usize>,
    image: Arc<Dma>,
}
pub struct Session {
    pub id: u32,
    pub geometry: Geometry,
    frames: Vec<Option<Frame>>,
    spare_images: Vec<Arc<Dma>>,
    input: Dma,
    scratch: Dma,
    bitstream: usize,
    mbhist: usize,
    mbhist_len: usize,
    history: usize,
    history_len: usize,
    picture_count: u32,
}
#[derive(Clone, Copy)]
pub struct Pending {
    pub request: VideoBackendDecodeRequest,
    pub slot: usize,
    pub started: u64,
    pub fence: u32,
}

fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn flag(value: u32, mask: u32) -> u32 {
    u32::from(value & mask != 0)
}

impl Session {
    pub fn new(id: u32, g: Geometry) -> Result<Self, &'static str> {
        let width_mbs = g.width / 16;
        let height_mbs = g.height / 16;
        let bitstream = align(SLICES + (width_mbs * height_mbs + 1) * 4, 256);
        // Seventeen stable picture slots: sixteen references plus the current picture.
        let mbhist = align(align(height_mbs, 2) * width_mbs * 64 - 63, 256) * 17;
        let mbhist_len = align(width_mbs * 104, 256);
        let history = align(mbhist + mbhist_len, 256);
        let history_len = align(width_mbs * 512 + 0x1100, 512);
        Ok(Self {
            id,
            geometry: g,
            frames: (0..17).map(|_| None).collect(),
            spare_images: Vec::with_capacity(17),
            input: Dma::new(bitstream + INPUT_BYTES)?,
            scratch: Dma::new(history + history_len)?,
            bitstream,
            mbhist,
            mbhist_len,
            history,
            history_len,
            picture_count: 0,
        })
    }

    fn copy_slices(&mut self, bytes: &[u8]) -> Result<(usize, usize), &'static str> {
        // The firmware receives only slice NAL units. SPS/PPS are copied into
        // the checked picture setup, never interpreted as device addresses.
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= bytes.len() {
            if bytes[i..i + 3] == [0, 0, 1] {
                starts.push((i, i + 3));
                i += 3;
            } else {
                i += 1;
            }
        }
        let max_slices = self.geometry.width / 16 * (self.geometry.height / 16);
        if starts.is_empty() || starts.len() > max_slices + 64 {
            return Err("NVDEC Annex-B access unit invalid");
        }
        let mut output = 0;
        let mut slices = 0;
        for (index, (_, start)) in starts.iter().copied().enumerate() {
            let mut end = starts.get(index + 1).map_or(bytes.len(), |entry| entry.0);
            while end > start && bytes[end - 1] == 0 {
                end -= 1;
            }
            if end <= start {
                continue;
            }
            let header = bytes[start];
            if header & 0x80 != 0 {
                return Err("NVDEC forbidden NAL bit set");
            }
            if !matches!(header & 31, 1 | 5) {
                continue;
            }
            if slices >= max_slices || output + 3 + end - start + EOS.len() > INPUT_BYTES {
                return Err("NVDEC slice buffer exhausted");
            }
            let dest = self.input.bytes_mut();
            put(dest, SLICES + slices * 4, output as u32);
            dest[self.bitstream + output..self.bitstream + output + 3].copy_from_slice(&[0, 0, 1]);
            output += 3;
            dest[self.bitstream + output..self.bitstream + output + end - start]
                .copy_from_slice(&bytes[start..end]);
            output += end - start;
            slices += 1;
        }
        if slices == 0 {
            return Err("NVDEC access unit has no slice");
        }
        // NVDEC expects slice_count + 1 offsets; the final entry excludes EOS.
        put(self.input.bytes_mut(), SLICES + slices * 4, output as u32);
        self.input.bytes_mut()[self.bitstream + output..self.bitstream + output + EOS.len()]
            .copy_from_slice(&EOS);
        Ok((output + EOS.len(), slices))
    }

    pub fn submit(
        &mut self,
        engine: &Engine,
        request: &VideoBackendH264StatelessRequest,
    ) -> Result<Pending, &'static str> {
        let params = &request.h264;
        let decode = &request.decode;
        let g = Geometry::from_params(params)?;
        if g != self.geometry
            || decode.stream_id != self.id
            || decode.coded_format != SCARLET_VIDEO_FORMAT_H264
        {
            return Err("NVDEC session geometry/format mismatch");
        }
        if decode.input_len == 0
            || decode.input_len as usize > INPUT_BYTES - EOS.len()
            || (!decode.shared_output
                && (decode.output_len as usize) < g.visible_width * g.visible_height * 3 / 2)
            || decode.timestamp == 0
        {
            return Err("NVDEC mapped input/output size or timestamp invalid");
        }
        // Copy user-writable mapped input once before parsing or publishing it.
        let bytes = unsafe {
            core::slice::from_raw_parts(decode.input_vaddr as *const u8, decode.input_len as usize)
        }
        .to_vec();
        let (stream_len, slices) = self.copy_slices(&bytes)?;
        let d = &params.decode_params;
        let refs: Vec<_> = d
            .dpb
            .iter()
            .filter(|r| r.flags & SCARLET_VIDEO_H264_DPB_FLAG_VALID != 0)
            .collect();
        if refs.len() > usize::from(params.sps.max_num_ref_frames)
            || refs
                .iter()
                .any(|r| r.fields != 3 || r.reference_ts == decode.timestamp)
        {
            return Err("NVDEC H.264 DPB is invalid");
        }
        for r in &refs {
            if !self
                .frames
                .iter()
                .flatten()
                .any(|f| f.timestamp == r.reference_ts)
            {
                return Err("NVDEC H.264 reference frame unavailable");
            }
        }
        for frame in &mut self.frames {
            if frame
                .as_ref()
                .is_some_and(|f| !refs.iter().any(|r| r.reference_ts == f.timestamp))
            {
                // DPB retirement does not imply consumer retirement. The pool
                // only reuses pages after every published image lease is gone.
                self.spare_images.push(frame.take().unwrap().image);
            }
        }
        let mut mask = 0u32;
        for frame in self.frames.iter().flatten() {
            if let Some(index) = frame.dpb {
                mask |= 1 << index;
            }
        }
        for frame in self.frames.iter_mut().flatten() {
            if frame.dpb.is_none() {
                let index = (!mask & 0xffff).trailing_zeros() as usize;
                if index >= 16 {
                    return Err("NVDEC DPB slots exhausted");
                }
                frame.dpb = Some(index);
                mask |= 1 << index;
            }
        }
        let slot = self
            .frames
            .iter()
            .position(Option::is_none)
            .ok_or("NVDEC picture slots exhausted")?;
        let image = if let Some(index) = self
            .spare_images
            .iter()
            .position(|image| Arc::strong_count(image) == 1)
        {
            self.spare_images.swap_remove(index)
        } else {
            // Bound hostile consumers retaining every decoded frame. Forty
            // surfaces cover 16 DPB references, reordering, and display queues.
            if self.spare_images.len() + self.frames.iter().flatten().count() >= 40 {
                return Err("NVDEC shared image leases exhausted; release decoded images");
            }
            Arc::new(Dma::new(g.size)?)
        };
        self.frames[slot] = Some(Frame {
            timestamp: decode.timestamp,
            dpb: None,
            image,
        });
        let mut setup = vec![0u8; 764];
        let s = &params.sps;
        let p = &params.pps;
        put(&mut setup, 72, stream_len as u32);
        put(&mut setup, 76, slices as u32);
        put(&mut setup, 80, self.mbhist_len as u32);
        put(
            &mut setup,
            88,
            u32::from(s.log2_max_pic_order_cnt_lsb_minus4),
        );
        put(
            &mut setup,
            92,
            flag(
                s.flags,
                SCARLET_VIDEO_H264_SPS_FLAG_DELTA_PIC_ORDER_ALWAYS_ZERO,
            ),
        );
        put(&mut setup, 96, 1);
        put(&mut setup, 100, (g.width / 16) as u32);
        put(&mut setup, 104, (g.height / 16) as u32);
        put(
            &mut setup,
            112,
            flag(
                p.flags as u32,
                SCARLET_VIDEO_H264_PPS_FLAG_ENTROPY_CODING_MODE as u32,
            ),
        );
        put(
            &mut setup,
            116,
            flag(
                p.flags as u32,
                SCARLET_VIDEO_H264_PPS_FLAG_BOTTOM_FIELD_PIC_ORDER_IN_FRAME_PRESENT as u32,
            ),
        );
        put(
            &mut setup,
            120,
            u32::from(p.num_ref_idx_l0_default_active_minus1),
        );
        put(
            &mut setup,
            124,
            u32::from(p.num_ref_idx_l1_default_active_minus1),
        );
        put(
            &mut setup,
            128,
            flag(
                p.flags as u32,
                SCARLET_VIDEO_H264_PPS_FLAG_DEBLOCKING_FILTER_CONTROL_PRESENT as u32,
            ),
        );
        put(
            &mut setup,
            132,
            flag(
                p.flags as u32,
                SCARLET_VIDEO_H264_PPS_FLAG_REDUNDANT_PIC_CNT_PRESENT as u32,
            ),
        );
        put(
            &mut setup,
            136,
            flag(
                p.flags as u32,
                SCARLET_VIDEO_H264_PPS_FLAG_TRANSFORM_8X8_MODE as u32,
            ),
        );
        put(&mut setup, 140, g.pitch as u32);
        put(&mut setup, 144, g.pitch as u32);
        put(&mut setup, 172, (self.history_len / 256) as u32);
        let flags = (flag(s.flags, SCARLET_VIDEO_H264_SPS_FLAG_DIRECT_8X8_INFERENCE) << 1)
            | (flag(
                p.flags as u32,
                SCARLET_VIDEO_H264_PPS_FLAG_WEIGHTED_PRED as u32,
            ) << 2)
            | (flag(
                p.flags as u32,
                SCARLET_VIDEO_H264_PPS_FLAG_CONSTRAINED_INTRA_PRED as u32,
            ) << 3)
            | (u32::from(d.nal_ref_idc != 0) << 4)
            | (u32::from(s.log2_max_frame_num_minus4) << 8)
            | (1 << 12)
            | (u32::from(s.pic_order_cnt_type) << 14)
            | (((p.pic_init_qp_minus26 as u32) & 63) << 16)
            | (((p.chroma_qp_index_offset as u32) & 31) << 22)
            | (((p.second_chroma_qp_index_offset as u32) & 31) << 27);
        put(&mut setup, 176, flags);
        put(
            &mut setup,
            180,
            u32::from(p.weighted_bipred_idc)
                | ((slot as u32) << 2)
                | ((slot as u32) << 9)
                | (u32::from(d.frame_num) << 14),
        );
        put(&mut setup, 184, d.top_field_order_cnt as u32);
        put(&mut setup, 188, d.bottom_field_order_cnt as u32);
        for reference in refs {
            let (pic, frame) = self
                .frames
                .iter()
                .enumerate()
                .filter_map(|(i, f)| f.as_ref().map(|f| (i, f)))
                .find(|(_, f)| f.timestamp == reference.reference_ts)
                .unwrap();
            let index = frame.dpb.unwrap();
            let at = 192 + index * 16;
            let long = reference.flags & SCARLET_VIDEO_H264_DPB_FLAG_LONG_TERM != 0;
            let marking = if long { 2 } else { 1 };
            put(
                &mut setup,
                at,
                pic as u32
                    | ((pic as u32) << 7)
                    | (3 << 12)
                    | (u32::from(long) << 14)
                    | (marking << 17)
                    | (marking << 21),
            );
            put(&mut setup, at + 4, reference.top_field_order_cnt as u32);
            put(&mut setup, at + 8, reference.bottom_field_order_cnt as u32);
            put(
                &mut setup,
                at + 12,
                if long {
                    reference.pic_num as u32
                } else {
                    u32::from(reference.frame_num)
                },
            );
        }
        for (i, scale) in params.scaling_matrix.scaling_list_4x4.iter().enumerate() {
            setup[448 + i * 16..448 + (i + 1) * 16].copy_from_slice(scale);
        }
        setup[544..608].copy_from_slice(&params.scaling_matrix.scaling_list_8x8[0]);
        setup[608..672].copy_from_slice(&params.scaling_matrix.scaling_list_8x8[3]);
        put(
            &mut setup,
            720,
            1 | (flag(
                s.flags,
                SCARLET_VIDEO_H264_SPS_FLAG_QPPRIME_Y_ZERO_TRANSFORM_BYPASS,
            ) << 1),
        );
        self.input.bytes_mut()[..setup.len()].copy_from_slice(&setup);
        self.input.bytes_mut()[STATUS..STATUS + 256].fill(0xff);
        arch::io_mb();
        engine.method(0x200, 3);
        engine.method(0x400, 3 | (1 << 4) | (1 << 5));
        engine.method(0x40c, self.picture_count);
        self.picture_count = self.picture_count.wrapping_add(1);
        for (method, addr) in [
            (0x404, self.input.address(0)),
            (0x408, self.input.address(self.bitstream)),
            (0x410, self.input.address(SLICES)),
            (0x424, self.input.address(STATUS)),
            (0x414, self.scratch.address(0)),
            (0x500, self.scratch.address(self.mbhist)),
            (0x418, self.scratch.address(self.history)),
        ] {
            engine.method(method, addr);
        }
        let fallback = self.frames[slot].as_ref().unwrap();
        for (i, frame) in self.frames.iter().enumerate() {
            let frame = frame.as_ref().unwrap_or(fallback);
            engine.method(0x430 + i as u32 * 4, frame.image.address(0));
            engine.method(0x474 + i as u32 * 4, frame.image.address(g.chroma));
        }
        arch::io_mb();
        let fence = engine.arm_completion();
        engine.method(0x300, 1 << 8);
        arch::io_mb();
        Ok(Pending {
            request: *decode,
            slot,
            started: time::current_time_ns(),
            fence,
        })
    }

    pub fn status(&self) -> [u32; 4] {
        core::array::from_fn(|i| self.input.read_word(STATUS + i * 4))
    }

    pub fn finish(&self, pending: Pending) -> Result<VideoBackendDecodedFrame, &'static str> {
        let g = self.geometry;
        let status = self.status();
        if status[3] != 0 || status[1] != 0 || status[0] != (g.width * g.height / 256) as u32 {
            scarlet::println!(
                "nvdec: decode status mbs={} error_mbs={} cycles={} error={:#x}",
                status[0],
                status[1],
                status[2],
                status[3]
            );
            return Err("NVDEC reported a failed/incomplete H.264 frame");
        }
        arch::io_mb();
        if pending.request.shared_output {
            let image = self.frames[pending.slot]
                .as_ref()
                .ok_or("NVDEC output frame missing")?;
            let backing = NativeImage {
                image: Arc::clone(&image.image),
                geometry: g,
            };
            return Ok(VideoBackendDecodedFrame {
                stream_id: self.id,
                frame: ScarletVideoDequeuedFrame {
                    width: g.visible_width as u32,
                    height: g.visible_height as u32,
                    pixel_format: SCARLET_VIDEO_PIXEL_FORMAT_NV12,
                    timestamp: pending.request.timestamp,
                    ..Default::default()
                },
                image: Some(Arc::new(SharedImage::new(Arc::new(backing))?)),
            });
        }
        let image = self.frames[pending.slot]
            .as_ref()
            .ok_or("NVDEC output frame missing")?
            .image
            .bytes();
        let payload = g.visible_width * g.visible_height * 3 / 2;
        let output = unsafe {
            core::slice::from_raw_parts_mut(pending.request.output_vaddr as *mut u8, payload)
        };
        for (base, dst_start, height, y_crop) in [
            (0, 0, g.visible_height, g.crop_y),
            (
                g.chroma,
                g.visible_width * g.visible_height,
                g.visible_height / 2,
                g.crop_y / 2,
            ),
        ] {
            linearize_plane(
                &image[base..],
                &mut output[dst_start..],
                g.pitch,
                g.visible_width,
                height,
                g.crop_x,
                y_crop,
            );
        }
        Ok(VideoBackendDecodedFrame {
            image: None,
            stream_id: self.id,
            frame: ScarletVideoDequeuedFrame {
                width: g.visible_width as u32,
                height: g.visible_height as u32,
                pixel_format: SCARLET_VIDEO_PIXEL_FORMAT_NV12,
                payload_offset: pending.request.output_offset,
                payload_len: payload as u32,
                flags: 0,
                timestamp: pending.request.timestamp,
            },
        })
    }
}

/// Publication occurs only after the decode fence/status check. All aliases
/// are immutable; Session checks Arc uniqueness before reusing these pages.
struct NativeImage {
    image: Arc<Dma>,
    geometry: Geometry,
}
// SAFETY: Arc owns resident NC pages; producer writes only to unique images.
// The session and outstanding capabilities may independently outlive each other.
unsafe impl SharedImageBacking for NativeImage {
    fn descriptor(&self) -> SharedImageDescriptor {
        let g = self.geometry;
        let mut descriptor = SharedImageDescriptor {
            version: SHARED_IMAGE_ABI_VERSION,
            format: IMAGE_FORMAT_NV12,
            width: g.width as u32,
            height: g.height as u32,
            visible: ImageRect {
                x: g.crop_x as u32,
                y: g.crop_y as u32,
                width: g.visible_width as u32,
                height: g.visible_height as u32,
            },
            // NVIDIA block-linear, uncompressed generic memory kind, 2 GOBs high.
            modifier: 0x0300_0000_000f_e011,
            buffer_count: 1,
            plane_count: 2,
            ..Default::default()
        };
        descriptor.buffer_sizes[0] = self.image.size() as u64;
        for (index, (offset, size)) in [(0, g.chroma), (g.chroma, g.size - g.chroma)]
            .into_iter()
            .enumerate()
        {
            descriptor.planes[index] = SharedImagePlane {
                buffer_index: 0,
                row_pitch: g.pitch as u32,
                offset: offset as u64,
                size: size as u64,
                reserved: 0,
            };
        }
        descriptor
    }
    fn buffer_segments(&self, index: usize) -> Arc<[GpuBackingSegment]> {
        if index == 0 {
            Arc::from([GpuBackingSegment::new(
                self.image.paddr(),
                self.image.size(),
            )])
        } else {
            Arc::from([])
        }
    }
}

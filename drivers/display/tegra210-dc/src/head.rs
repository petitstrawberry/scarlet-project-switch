// SPDX-License-Identifier: GPL-2.0-only
//! Modes supported by the ODIN external head's pixel clock and two-lane DP link.
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub width: u32,
    pub height: u32,
    pub clock_khz: u32,
    pub h_front: u32,
    pub h_sync: u32,
    pub h_back: u32,
    pub v_front: u32,
    pub v_sync: u32,
    pub v_back: u32,
}

pub const HD: Timing = Timing {
    width: 1280,
    height: 720,
    clock_khz: 74250,
    h_front: 110,
    h_sync: 40,
    h_back: 220,
    v_front: 5,
    v_sync: 5,
    v_back: 20,
};
pub const FHD: Timing = Timing {
    width: 1920,
    height: 1080,
    clock_khz: 148500,
    h_front: 88,
    h_sync: 44,
    h_back: 148,
    v_front: 4,
    v_sync: 5,
    v_back: 36,
};

impl Timing {
    pub const fn htotal(self) -> u32 {
        self.width + self.h_front + self.h_sync + self.h_back
    }
    pub const fn vtotal(self) -> u32 {
        self.height + self.v_front + self.v_sync + self.v_back
    }
    pub const fn refresh_millihz(self) -> u32 {
        ((self.clock_khz as u64 * 1_000_000) / (self.htotal() as u64 * self.vtotal() as u64)) as u32
    }
}

fn detailed_timing(d: &[u8]) -> Option<Timing> {
    let pair = |low: u8, high: u8| u32::from(low) | (u32::from(high) << 8);
    let pixel_clock = u32::from(u16::from_le_bytes([d[0], d[1]])) * 10;
    // Progressive, digital separate sync with positive H and V polarity.
    if d[17] != 0x1e {
        return None;
    }
    [HD, FHD].into_iter().find(|t| {
        t.clock_khz == pixel_clock
            && t.width == pair(d[2], d[4] >> 4)
            && t.height == pair(d[5], d[7] >> 4)
            && t.htotal() - t.width == pair(d[3], d[4] & 15)
            && t.vtotal() - t.height == pair(d[6], d[7] & 15)
            && t.h_front == pair(d[8], (d[11] >> 6) & 3)
            && t.h_sync == pair(d[9], (d[11] >> 4) & 3)
            && t.v_front == u32::from((d[10] >> 4) | ((d[11] & 12) << 2))
            && t.v_sync == u32::from((d[10] & 15) | ((d[11] & 3) << 4))
            && d[15] == 0
            && d[16] == 0
    })
}

fn vic(code: u8) -> Option<Timing> {
    match code {
        4 => Some(HD),
        16 => Some(FHD),
        _ => None,
    }
}

fn add_mode(modes: &mut Vec<Timing>, t: Timing) {
    if !modes.contains(&t) {
        modes.push(t);
    }
}

/// Rank only RGB modes advertised in a DTD or ordinary CTA Video Data Block.
/// CTA-861-H §§7.3, 7.5.12: preference references and the native DTD count
/// number actual timing descriptors across the EDID, including unsupported ones.
pub fn monitor_modes(edid: &[u8]) -> Result<Vec<Timing>, &'static str> {
    let valid = |b: &[u8]| b.iter().fold(0u8, |a, v| a.wrapping_add(*v)) == 0;
    let base = edid.get(..128).ok_or("missing base EDID")?;
    if base[..8] != [0, 255, 255, 255, 255, 255, 255, 0] || !valid(base) {
        return Err("invalid base EDID");
    }

    let mut advertised = Vec::new();
    let mut dtds = Vec::new();
    let mut native_svd = Vec::new();
    let mut native_dtd_count = None;
    let mut preferences = &[][..];
    let mut preferred = None;
    let mut first_svd = None;
    for (i, d) in base[54..126].chunks_exact(18).enumerate() {
        if d[..2] != [0, 0] {
            let t = detailed_timing(d);
            dtds.push(t);
            if let Some(t) = t {
                add_mode(&mut advertised, t);
                if i == 0 && (base[19] >= 4 || base[24] & 2 != 0) {
                    preferred = Some(t);
                }
            }
        }
    }
    for ext in edid[128..].chunks_exact(128).take(base[126] as usize) {
        if ext[0] != 2 || !valid(ext) {
            continue;
        }
        let end = ext[2] as usize;
        if end == 0 {
            if ext[1] >= 2 {
                native_dtd_count.get_or_insert(usize::from(ext[3] & 15));
            }
            continue;
        }
        if !(4..=127).contains(&end) {
            continue;
        }
        if ext[1] >= 3 {
            let mut blocks = &ext[4..end];
            // Reject an overlong block before accepting any preference from this extension.
            let mut check = blocks;
            while let Some((&tag, rest)) = check.split_first() {
                let Some(tail) = rest.get(usize::from(tag & 31)..) else {
                    break;
                };
                check = tail;
            }
            if !check.is_empty() {
                continue;
            }
            while let Some((&tag, rest)) = blocks.split_first() {
                let (payload, tail) = rest.split_at(usize::from(tag & 31));
                blocks = tail;
                if tag >> 5 == 2 {
                    if first_svd.is_none() {
                        first_svd = payload.first().copied();
                    }
                    for &svd in payload {
                        if let Some(t) = vic(svd & 127) {
                            add_mode(&mut advertised, t);
                            if svd & 128 != 0 {
                                add_mode(&mut native_svd, t);
                            }
                        }
                    }
                } else if tag >> 5 == 7 && payload.first() == Some(&0x0d) && preferences.is_empty()
                {
                    preferences = &payload[1..];
                }
            }
        }
        if ext[1] >= 2 {
            native_dtd_count.get_or_insert(usize::from(ext[3] & 15));
        }
        for d in ext[end..127].chunks_exact(18) {
            if d[..2] != [0, 0] {
                let t = detailed_timing(d);
                dtds.push(t);
                if let Some(t) = t {
                    add_mode(&mut advertised, t);
                }
            }
        }
    }

    let mut ranked = Vec::new();
    for &reference in preferences {
        let t = match reference {
            129..=144 => dtds.get(usize::from(reference - 129)).copied().flatten(),
            code => vic(code),
        };
        if let Some(t) = t.filter(|t| advertised.contains(t)) {
            add_mode(&mut ranked, t);
        }
    }
    for t in preferred
        .into_iter()
        .chain(
            dtds.into_iter()
                .take(native_dtd_count.unwrap_or(0))
                .flatten(),
        )
        .chain(native_svd)
        // Legacy CTA preference by position breaks ties between ordinary modes.
        .chain(first_svd.and_then(|code| vic(code & 127)))
        .chain(advertised)
    {
        add_mode(&mut ranked, t);
    }
    if ranked.is_empty() {
        Err("sink advertises no supported progressive timing")
    } else {
        Ok(ranked)
    }
}

// Word offsets, scoped to the DC1 mapping. Register names/fields and T21x
// sequencing are from switch-l4t-kernel-nvidia 7d95822a, drivers/video/tegra/dc/
// {dc_reg.h,dc.c,mode.c,window.c,sor.c}. Icosa enables windows A/B/C on DC1.
const COMMAND: usize = 0x32;
const POWER: usize = 0x36;
const INT_STATUS: usize = 0x37;
const INT_MASK: usize = 0x38;
const INT_ENABLE: usize = 0x39;
const INT_TYPE: usize = 0x3a;
const INT_POLARITY: usize = 0x3b;
const ACCESS: usize = 0x40;
const STATE: usize = 0x41;
const HEADER: usize = 0x42;
const ACT_CONTROL: usize = 0x43;
const OUTPUT: usize = 0x402;
const PRIORITY: usize = 0x403;
const PRIORITY_TIMER: usize = 0x404;
const CLOCK: usize = 0x42e;
const COLOR: usize = 0x430;
const WIN_OPTIONS: usize = 0x700;
const DEPTH: usize = 0x703;
const STRIDE: usize = 0x70a;
const ADDR: usize = 0x800;
const KIND: usize = 0x80b;
const ADDR_HI: usize = 0x80d;

const GENERAL: u32 = 1;
const WIN_A: u32 = 2;
const HEAD_WINDOWS: u32 = GENERAL | WIN_A | 4 | 8;
const SELECT_A: u32 = 1 << 4;
const WIN_ENABLE: u32 = 1 << 30;
const SOR1_ENABLE: u32 = 1 << 26;
const MODE_MASK: u32 = 3 << 5;
const CONTINUOUS: u32 = 1 << 5;
const FRAME_END: u32 = 1 << 1;
const FIFO_ERROR: u32 = (1 << 8) | (1 << 14); // Window A underflow / overflow
const B8G8R8A8: u32 = 12;
const TIMEOUT_NS: u64 = 100_000_000;

pub(crate) trait Registers {
    fn read(&self, word: usize) -> u32;
    fn write(&self, word: usize, value: u32);
    fn now(&self) -> u64;
    fn pause(&self);
}

#[cfg(target_os = "none")]
impl Registers for usize {
    fn read(&self, word: usize) -> u32 {
        unsafe { scarlet::arch::mmio::read32(*self + 4 * word) }
    }

    fn write(&self, word: usize, value: u32) {
        unsafe {
            scarlet::arch::mmio::write32(*self + 4 * word, value);
        }
        scarlet::arch::io_mb();
    }

    fn now(&self) -> u64 {
        scarlet::time::current_time_ns()
    }

    fn pause(&self) {
        if let Some(task) = scarlet::task::mytask() {
            task.sleep(task.get_trapframe(), 1_000_000);
        } else {
            scarlet_driver_tegra210::delay_us(50);
        }
    }
}

/// The serialized caller owns the reset/released DC1 supplied by prepare_mode.
/// All accesses are to that register aperture. Framebuffer addresses are DMA
/// inputs; the backing stays pinned by the caller through completion or stop
/// and the platform's MC DCB drain.
pub(crate) struct Head<R = usize>(pub(crate) R);

#[derive(Clone, Copy)]
struct Fetch {
    address: u64,
    pitch: u32,
    kind: u32,
}

impl Fetch {
    fn checked(
        mode: Timing,
        address: u64,
        pitch: u32,
        depth: u32,
        kind: u32,
    ) -> Result<Self, &'static str> {
        if !matches!(mode, HD | FHD) || depth != B8G8R8A8 || !matches!(kind, 0 | 0x42) {
            return Err("unsupported DC1 window configuration");
        }
        // T210 BL_16B2 with block-height 4 fetches complete 128-row groups.
        let rows = if kind == 0 {
            mode.height
        } else {
            mode.height.next_multiple_of(128)
        };
        if pitch < mode.width * 4
            || pitch > 0xffff
            || pitch & 63 != 0
            || address & 0xfff != 0
            || address
                .checked_add(u64::from(rows) * u64::from(pitch))
                .is_none_or(|end| end > 1 << 34)
        {
            return Err("invalid DC1 scanout DMA layout");
        }
        Ok(Self {
            address,
            pitch,
            kind,
        })
    }
}

impl<R: Registers> Head<R> {
    fn until(
        &self,
        mut ready: impl FnMut() -> Result<bool, &'static str>,
    ) -> Result<(), &'static str> {
        let deadline = self.0.now().saturating_add(TIMEOUT_NS);
        loop {
            if ready()? {
                return Ok(());
            }
            if self.0.now() >= deadline {
                return Err("DC1 activation timed out");
            }
            self.0.pause();
        }
    }

    fn update(&self, word: usize, mask: u32, value: u32) {
        self.0.write(word, (self.0.read(word) & !mask) | value);
    }

    fn commit(&self, mask: u32) {
        // UPDATE copies assembly to arm. ACT_REQ promotes arm to active.
        // Read this same peripheral between submissions to drain posted writes.
        self.0.write(STATE, mask << 8);
        let _ = self.0.read(STATE);
        self.0.write(STATE, mask);
        let _ = self.0.read(STATE);
    }

    fn select(&self, window: u32) {
        self.0.write(HEADER, SELECT_A << window);
    }

    fn disabled(&self, window: u32) -> bool {
        self.select(window);
        self.0.read(WIN_OPTIONS) & WIN_ENABLE == 0
    }

    fn fetch(&self, f: Fetch) {
        self.select(0);
        self.0.write(STRIDE, f.pitch);
        self.0.write(KIND, f.kind);
        self.0.write(ADDR, f.address as u32);
        self.0.write(ADDR_HI, (f.address >> 32) as u32);
        // Switchroot window.c resets the T21x fetch FIFO before UPDATE.
        self.0.write(0x82b, 1);
    }

    fn fetching(&self, f: Fetch) -> bool {
        self.select(0);
        self.0.read(WIN_OPTIONS) == WIN_ENABLE
            && self.0.read(ADDR) == f.address as u32
            && self.0.read(ADDR_HI) == (f.address >> 32) as u32
            && self.0.read(STRIDE) == f.pitch
            && self.0.read(KIND) == f.kind
            && self.0.read(DEPTH) == B8G8R8A8
    }

    fn geometry(&self, mode: Timing) -> bool {
        self.select(0);
        let pixels = mode.height << 16 | mode.width;
        self.0.read(0x409) == pixels
            && self.0.read(0x705) == pixels
            && self.0.read(0x706) == (mode.height << 16 | mode.width * 4)
    }

    fn window(&self, mode: Timing, f: Fetch) {
        let io = &self.0;
        self.select(0);
        io.write(0x701, 0); // native byte order, host memory client
        io.write(0x702, 0);
        io.write(DEPTH, B8G8R8A8);
        io.write(0x704, 0); // destination origin and size in pixels
        io.write(0x705, mode.height << 16 | mode.width);
        io.write(0x706, mode.height << 16 | (mode.width * 4)); // input width in bytes
        io.write(0x707, 0); // zero DDA phase, unity horizontal and vertical increments
        io.write(0x708, 0);
        io.write(0x709, 0x10001000);
        // One packed RGB plane. Clear inherited secondary strides and legacy
        // tiled addressing; T210 selects block-linear layout in SURFACE_KIND.
        io.write(0x70b, 0);
        io.write(0x70c, 0);
        io.write(0x70d, 0);
        io.write(0x716, 1 << 24 | 255); // opaque sequential blender bypass
        io.write(0x806, 0); // no crop in the backing
        io.write(0x808, 0);
        io.write(0x82f, 0); // uncompressed scanout, T21x CDE clock override
        io.write(0x837, 1);
        self.fetch(f);
        io.write(WIN_OPTIONS, WIN_ENABLE); // progressive RGB, no CSC/filter/rotation
    }

    pub fn start(
        &self,
        mode: Timing,
        address: u64,
        shift_divider: u32,
    ) -> Result<(), &'static str> {
        if shift_divider > 255 {
            return Err("invalid DC1 PCD1 shift divider");
        }
        let f = Fetch::checked(mode, address, mode.width.saturating_mul(4), B8G8R8A8, 0)?;
        let io = &self.0;
        // Read ACTIVE for all confirmations and RMWs; every buffered write
        // goes to ASSEMBLY. In particular, never flip an address mid-scanline.
        io.write(ACCESS, 1);
        // prepare_mode rejects a live firmware head and resets DC1 before handing
        // it over. Quiesce any remaining local fetch/armed state first.
        if !self.idle() {
            self.stop()?;
        }
        io.write(POWER, 0x50155); // DC1 PW0..4 and PM0..1
        io.write(INT_MASK, 0); // poll sticky status without delivering a DC IRQ
        io.write(INT_ENABLE, FRAME_END | FIFO_ERROR);
        self.update(INT_TYPE, FRAME_END | FIFO_ERROR, FIFO_ERROR);
        self.update(INT_POLARITY, FRAME_END | FIFO_ERROR, FIFO_ERROR);
        io.write(INT_STATUS, FRAME_END | FIFO_ERROR); // W1C inherited events
        // GENERAL and A/B/C use the V counter for activation; this also
        // prevents a stale immediate/H-counter latch on the first window.
        self.update(ACT_CONTROL, 1 | (1 << 2) | (1 << 4) | (1 << 6), 0);
        // tegra_dc_init threshold/timer for A in the DC1 arbiter. The shared
        // EMC/MC budget is platform-owned (t21x/bandwidth.c, DISPLAY_0AB,
        // peak pixel_hz * 4 bytes/s for one unscaled RGB window).
        self.update(PRIORITY, 0x7f << 16, 0x20 << 16);
        self.update(PRIORITY_TIMER, 0x3f << 16, 1 << 16);
        io.write(OUTPUT, 0); // cursor fetch off; route SOR1 only at attach
        io.write(0x405, 1); // T21x SOR VSYNC_H_POSITION
        io.write(0x406, 1 << 16 | 1); // DP H/V_REF_TO_SYNC
        io.write(0x407, mode.v_sync << 16 | mode.h_sync);
        // DP moves V_REF_TO_SYNC from back to front porch in DC only.
        io.write(0x408, (mode.v_back - 1) << 16 | mode.h_back);
        io.write(0x409, mode.height << 16 | mode.width);
        io.write(0x40a, (mode.v_front + 1) << 16 | mode.h_front);
        io.write(0x4e5, 0); // progressive
        io.write(CLOCK, shift_divider); // PCD1, shift = 2 * parent / pixel - 2
        io.write(0x42f, 0); // DF1P1C, MSB aligned, RED_BLUE
        io.write(COLOR, 8); // BASE_COLOR_SIZE888, dither and CMU off
        io.write(0x432, 5); // DE_SELECT_ACTIVE | DE_CONTROL_NORMAL
        for w in [1, 2] {
            self.select(w);
            io.write(WIN_OPTIONS, 0);
        }
        self.window(mode, f);
        io.write(COMMAND, CONTINUOUS);
        self.commit(HEAD_WINDOWS);
        // SOR attach follows start. No previous client buffer is returned
        // here and the transmitter need not be producing frame events yet.
        self.until(|| {
            if io.read(INT_STATUS) & FIFO_ERROR != 0 {
                return Err("DC1 window A FIFO fault");
            }
            Ok(io.read(STATE) & HEAD_WINDOWS == 0
                && io.read(COMMAND) & MODE_MASK == CONTINUOUS
                && io.read(OUTPUT) == 0
                && io.read(COLOR) == 8
                && io.read(CLOCK) == shift_divider
                && self.disabled(1)
                && self.disabled(2)
                && self.geometry(mode)
                && self.fetching(f))
        })
    }

    pub fn route(&self, enabled: bool) -> Result<(), &'static str> {
        // DISP_WIN_OPTIONS is in GENERAL. CURSOR, DSI, SOR0 and the HDMI-only
        // SOR1_TIMING_CYA remain clear on the external head.
        let value = if enabled { SOR1_ENABLE } else { 0 };
        self.0.write(ACCESS, 1);
        self.0.write(OUTPUT, value);
        self.commit(GENERAL);
        self.until(|| Ok(self.0.read(STATE) & GENERAL == 0 && self.0.read(OUTPUT) == value))
    }

    pub fn flip(
        &self,
        mode: Timing,
        addr: u64,
        pitch: u32,
        depth: u32,
        kind: u32,
    ) -> Result<(), &'static str> {
        let f = Fetch::checked(mode, addr, pitch, depth, kind)?;
        let io = &self.0;
        io.write(ACCESS, 1);
        if io.read(COMMAND) & MODE_MASK != CONTINUOUS || io.read(STATE) & HEAD_WINDOWS != 0 {
            return Err("DC1 is stopped or an activation is pending");
        }
        // A flip only changes backing. Validate the active fetch extent before
        // arming a buffer whose aperture/pitch were checked for this mode.
        if !self.geometry(mode) {
            return Err("DC1 active window size differs from the buffer");
        }
        self.fetch(f);
        // Switchroot update_windows couples GENERAL and the changed plane.
        let mask = GENERAL | WIN_A;
        self.commit(mask);
        // Clear *after* posting ACT_REQ, including a frame end concurrent
        // with submission. Losing that edge only delays retirement one frame.
        io.write(INT_STATUS, FRAME_END);
        self.until(|| {
            let status = io.read(INT_STATUS);
            if status & FIFO_ERROR != 0 {
                return Err("DC1 window A FIFO fault");
            }
            if status & FRAME_END == 0 {
                return Ok(false);
            }
            io.write(INT_STATUS, FRAME_END);
            Ok(io.read(STATE) & mask == 0
                && io.read(COMMAND) & MODE_MASK == CONTINUOUS
                && self.fetching(f))
        })
    }

    fn idle(&self) -> bool {
        self.0.read(STATE) & HEAD_WINDOWS == 0
            && self.0.read(COMMAND) & MODE_MASK == 0
            && self.0.read(OUTPUT) == 0
            && (0..3).all(|w| self.disabled(w))
    }

    pub fn stop(&self) -> Result<(), &'static str> {
        self.0.write(ACCESS, 1);
        if !self.idle() {
            // Supersede even a timed-out flip, including its armed address.
            // Disable every local fetch while the timing generator can still
            // promote windows on vblank. Cursor enable is in GENERAL.
            let running = self.0.read(COMMAND) & MODE_MASK != 0;
            for w in 0..3 {
                self.select(w);
                self.0.write(WIN_OPTIONS, 0);
            }
            self.0.write(OUTPUT, 0);
            if running {
                self.commit(HEAD_WINDOWS);
                // Even a stalled window gets a bounded attempt to STOP below.
                let _ = self.until(|| {
                    Ok(self.0.read(STATE) & HEAD_WINDOWS == 0
                        && self.0.read(OUTPUT) == 0
                        && (0..3).all(|w| self.disabled(w)))
                });
            }
            self.0.write(COMMAND, 0);
            // If windows already retired, only GENERAL needs another latch.
            let mask = if self.0.read(STATE) & HEAD_WINDOWS == 0 && (0..3).all(|w| self.disabled(w))
            {
                GENERAL
            } else {
                HEAD_WINDOWS
            };
            self.commit(mask);
            // STOP has no subsequent FRAME_END. Active STOP, inactive fetches
            // and drained requests must all agree before handing off to MC.
            self.until(|| Ok(self.idle()))?;
        }
        self.0.write(INT_ENABLE, 0);
        self.0.write(INT_STATUS, FRAME_END | FIFO_ERROR);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn checksum(block: &mut [u8]) {
        block[127] = 0;
        block[127] = 0u8.wrapping_sub(block.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    }
    const HD_DTD: [u8; 18] = [
        0x01, 0x1d, 0x00, 0x72, 0x51, 0xd0, 0x1e, 0x20, 0x6e, 0x28, 0x55, 0, 0, 0, 0, 0, 0, 0x1e,
    ];
    const FHD_DTD: [u8; 18] = [
        0x02, 0x3a, 0x80, 0x18, 0x71, 0x38, 0x2d, 0x40, 0x58, 0x2c, 0x45, 0, 0, 0, 0, 0, 0, 0x1e,
    ];

    fn edid(
        base_dtd: Option<[u8; 18]>,
        blocks: &[u8],
        ext_dtd: Option<[u8; 18]>,
        native: u8,
    ) -> [u8; 256] {
        let mut edid = [0; 256];
        edid[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
        edid[18..20].copy_from_slice(&[1, 4]);
        if let Some(dtd) = base_dtd {
            edid[54..72].copy_from_slice(&dtd);
        }
        edid[126] = 1;
        let end = 4 + blocks.len();
        let ext = &mut edid[128..];
        ext[..4].copy_from_slice(&[2, 3, end as u8, native]);
        ext[4..end].copy_from_slice(blocks);
        if let Some(dtd) = ext_dtd {
            ext[end..end + 18].copy_from_slice(&dtd);
        }
        checksum(ext);
        checksum(&mut edid[..128]);
        edid
    }

    #[test]
    fn explicit_cta_preference_overrides_base_dtd_and_native_svd() {
        for reference in [16, 130] {
            // VIC 16 or second DTD in the EDID
            let e = edid(
                Some(HD_DTD),
                &[0x42, 0x84, 16, 0xe2, 13, reference],
                Some(FHD_DTD),
                1,
            );
            assert_eq!(monitor_modes(&e).unwrap(), [FHD, HD]);
        }
        // Preference references alone never introduce a 4:2:0-only mode.
        let e = edid(Some(HD_DTD), &[0xe2, 14, 16, 0xe2, 13, 16], None, 0);
        assert_eq!(monitor_modes(&e).unwrap(), [HD]);
    }

    #[test]
    fn preferred_base_dtd_then_native_then_advertisement_order() {
        let mut e = edid(Some(HD_DTD), &[0x43, 4, 16, 0x90], None, 0);
        assert_eq!(monitor_modes(&e).unwrap(), [HD, FHD]);
        e[19] = 3; // EDID 1.3 requires the preferred timing feature flag.
        checksum(&mut e[..128]);
        assert_eq!(monitor_modes(&e).unwrap(), [FHD, HD]);
        e[24] |= 2;
        checksum(&mut e[..128]);
        assert_eq!(monitor_modes(&e).unwrap(), [HD, FHD]);
        let e = edid(None, &[0x42, 4, 16], Some(FHD_DTD), 1);
        assert_eq!(monitor_modes(&e).unwrap(), [FHD, HD]);
    }

    #[test]
    fn native_dtd_count_and_short_video_references_include_unsupported_base_dtd() {
        let mut unsupported = HD_DTD;
        unsupported[0] = 2;
        let e = edid(Some(unsupported), &[0x42, 4, 16], Some(FHD_DTD), 1);
        assert_eq!(monitor_modes(&e).unwrap(), [HD, FHD]);
        let e = edid(
            Some(unsupported),
            &[0x42, 4, 16, 0xe2, 13, 130],
            Some(FHD_DTD),
            1,
        );
        assert_eq!(monitor_modes(&e).unwrap(), [FHD, HD]);
        let malformed = edid(None, &[0x41, 4, 0x43, 16], None, 0);
        assert!(monitor_modes(&malformed).is_err());
    }

    #[test]
    fn only_advertised_rgb_modes_are_exposed() {
        let mut edid = [0; 256];
        edid[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
        edid[126] = 1;
        checksum(&mut edid[..128]);
        edid[128..137].copy_from_slice(&[2, 3, 9, 0, 0x44, 0x90, 4, 16, 97]);
        checksum(&mut edid[128..]);
        assert_eq!(monitor_modes(&edid).unwrap(), [FHD, HD]);
        assert_eq!(HD.refresh_millihz(), 60000);
        edid[134] = 99;
        assert!(monitor_modes(&edid).is_err());
    }
}

#[cfg(test)]
mod head_register_tests {
    use super::*;
    use core::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};

    // A bank key consists of its ACT_REQ bit and the DC word offset.
    type Key = (u32, usize);
    type Bank = BTreeMap<Key, u32>;
    const FRONT: u64 = 0x8000_0000;
    const BACK: u64 = 0x1_8000_0000; // same low word, different DMA page

    #[derive(Default)]
    struct Model {
        assembly: Bank,
        arm: Bank,
        active: Bank,
        immediate: BTreeMap<usize, u32>,
        requests: Vec<u32>,
        pending: u32,
        // Each tick may advance the V counter and/or deliver FRAME_END.
        ticks: VecDeque<(bool, bool)>,
        suppress_frames: bool,
        freeze: bool,
        stuck_field: Option<Key>,
        frame_on_act: bool,
        ns: u64,
    }

    impl Model {
        fn active(&self, group: u32, reg: usize) -> u32 {
            self.active.get(&(group, reg)).copied().unwrap_or(0)
        }
        fn key(&self, reg: usize) -> Key {
            let group = if reg < WIN_OPTIONS {
                GENERAL
            } else {
                let header = self.immediate[&HEADER];
                assert!(
                    matches!(header, 0x10 | 0x20 | 0x40),
                    "window outside DC1 DT"
                );
                header >> 3
            };
            (group, reg)
        }
        fn tick(&mut self) {
            self.ns += 1_000_000;
            let (promote, frame) = self
                .ticks
                .pop_front()
                .unwrap_or((!self.freeze, !self.suppress_frames));
            if promote {
                for (&key, &v) in &self.arm {
                    if key.0 & self.pending != 0 && Some(key) != self.stuck_field {
                        self.active.insert(key, v);
                    }
                }
                self.pending = 0;
            }
            if frame && self.active(GENERAL, COMMAND) & MODE_MASK == CONTINUOUS {
                let enabled = self.immediate.get(&INT_ENABLE).copied().unwrap_or(0);
                *self.immediate.entry(INT_STATUS).or_default() |= FRAME_END & enabled;
            }
        }
    }

    impl Registers for RefCell<Model> {
        fn read(&self, reg: usize) -> u32 {
            let m = self.borrow();
            match reg {
                STATE => m.pending,
                INT_STATUS..=INT_POLARITY | ACCESS | HEADER | ACT_CONTROL => {
                    m.immediate.get(&reg).copied().unwrap_or(0)
                }
                _ => {
                    let bank = if m.immediate.get(&ACCESS).copied().unwrap_or(0) & 1 != 0 {
                        &m.active
                    } else {
                        &m.assembly
                    };
                    bank.get(&m.key(reg)).copied().unwrap_or(0)
                }
            }
        }
        fn write(&self, reg: usize, v: u32) {
            let mut m = self.borrow_mut();
            match reg {
                STATE if v & 0xff00 != 0 => {
                    assert_eq!(v & !(HEAD_WINDOWS << 8), 0);
                    let assembly = m.assembly.clone();
                    m.arm.extend(
                        assembly
                            .into_iter()
                            .filter(|(key, _)| key.0 & (v >> 8) != 0),
                    );
                }
                STATE => {
                    assert_eq!(v & !HEAD_WINDOWS, 0);
                    m.requests.push(v);
                    m.pending |= v;
                    if m.frame_on_act {
                        *m.immediate.entry(INT_STATUS).or_default() |= FRAME_END;
                    }
                }
                INT_STATUS => *m.immediate.entry(reg).or_default() &= !v,
                INT_MASK..=INT_POLARITY | ACCESS | ACT_CONTROL | HEADER => {
                    m.immediate.insert(reg, v);
                }
                _ => {
                    let key = m.key(reg);
                    assert_eq!(
                        m.immediate[&ACCESS] & 4,
                        0,
                        "write directly to active state"
                    );
                    if key.0 > WIN_A {
                        assert_eq!(reg, WIN_OPTIONS, "touched inactive window's backing");
                    }
                    m.assembly.insert(key, v);
                }
            }
        }
        fn now(&self) -> u64 {
            self.borrow().ns
        }
        fn pause(&self) {
            self.borrow_mut().tick();
        }
    }

    fn started() -> Head<RefCell<Model>> {
        let head = Head(RefCell::new(Model::default()));
        head.start(HD, FRONT, 0).unwrap();
        head.route(true).unwrap();
        head
    }
    fn flip(head: &Head<RefCell<Model>>) -> Result<(), &'static str> {
        head.flip(HD, BACK, 5184, B8G8R8A8, 0x42)
    }

    #[test]
    fn released_head_sanitizes_inherited_windows_color_route_and_arbiter() {
        let head = Head(RefCell::new(Model::default()));
        {
            let mut m = head.0.borrow_mut();
            m.immediate.insert(ACCESS, 4);
            m.immediate.insert(ACT_CONTROL, 0x455);
            m.immediate.insert(INT_STATUS, FRAME_END | FIFO_ERROR);
            m.active.extend([
                ((GENERAL, OUTPUT), (1 << 29) | (1 << 16)), // stray DSI and cursor enables
                ((GENERAL, COLOR), 0x10_0200),
                ((GENERAL, PRIORITY), 0x0360_1234),
                ((GENERAL, PRIORITY_TIMER), 0x395f_1729),
                ((4, WIN_OPTIONS), WIN_ENABLE),
                ((8, WIN_OPTIONS), WIN_ENABLE),
                ((4, ADDR), 0xa123_4000),
                ((8, ADDR), 0xb567_8000),
            ]);
            m.assembly = m.active.clone();
        }
        head.start(FHD, FRONT, 0).unwrap();
        {
            let m = head.0.borrow();
            assert_eq!(m.active(GENERAL, PRIORITY), 0x0320_1234);
            assert_eq!(m.active(GENERAL, PRIORITY_TIMER), 0x3941_1729);
            assert_eq!(m.active(GENERAL, CLOCK), 0);
            assert_eq!(m.active(GENERAL, COLOR), 8);
            assert_eq!(m.active(GENERAL, 0x405), 1);
            assert_eq!(m.active(GENERAL, 0x408), 35 << 16 | 148);
            assert_eq!(m.active(GENERAL, 0x40a), 5 << 16 | 88);
            assert_eq!(m.immediate[&ACT_CONTROL], 0x400);
            assert_eq!(m.immediate[&INT_MASK], 0);
            assert_eq!(m.immediate[&INT_STATUS] & FIFO_ERROR, 0);
            assert_eq!(m.active(GENERAL, OUTPUT), 0);
            assert_eq!(m.active(4, WIN_OPTIONS) | m.active(8, WIN_OPTIONS), 0);
            assert_eq!(m.active(4, ADDR), 0xa123_4000);
            assert_eq!(m.active(8, ADDR), 0xb567_8000);
        }
        head.route(true).unwrap();
        assert_eq!(head.0.borrow().active(GENERAL, OUTPUT), SOR1_ENABLE);
        assert_eq!(head.0.borrow().requests.last(), Some(&GENERAL));
        head.route(false).unwrap();
        assert_eq!(head.0.borrow().active(GENERAL, OUTPUT), 0);
    }

    #[test]
    fn flip_retires_at_new_frame_with_active_address_and_layout() {
        let head = started();
        head.0
            .borrow_mut()
            .ticks
            .extend([(false, true), (false, false), (true, true)]);
        let before = head.0.now();
        flip(&head).unwrap();
        let m = head.0.borrow();
        assert_eq!(m.ns - before, 3_000_000);
        assert_eq!(m.requests.last(), Some(&(GENERAL | WIN_A)));
        assert_eq!(m.active(WIN_A, ADDR_HI), 1);
        assert_eq!(m.active(WIN_A, STRIDE), 5184);
        assert_eq!(m.active(WIN_A, KIND), 0x42);
    }

    #[test]
    fn stale_frame_and_frame_before_promotion_do_not_retire() {
        for early_frame in [false, true] {
            let head = started();
            {
                let mut m = head.0.borrow_mut();
                m.immediate.insert(INT_STATUS, FRAME_END);
                m.frame_on_act = true;
                m.suppress_frames = true;
                m.ticks.push_back((false, early_frame));
            }
            assert_eq!(flip(&head), Err("DC1 activation timed out"));
            assert_eq!(head.0.borrow().active(WIN_A, ADDR_HI), 1);
            head.stop().unwrap(); // no frame after STOP
        }
    }

    #[test]
    fn stale_active_high_address_or_layout_prevents_retirement() {
        for word in [ADDR_HI, STRIDE, KIND] {
            let head = started();
            head.0.borrow_mut().stuck_field = Some((WIN_A, word));
            assert_eq!(flip(&head), Err("DC1 activation timed out"));
            let m = head.0.borrow();
            assert_eq!(m.pending, 0);
            assert_ne!(m.active(WIN_A, word), m.arm[&(WIN_A, word)]);
        }
    }

    #[test]
    fn stop_after_pending_flip_requires_active_stop_all_fetches_and_drained_requests() {
        let head = started();
        head.0.borrow_mut().freeze = true;
        assert_eq!(flip(&head), Err("DC1 activation timed out"));
        assert_eq!(head.stop(), Err("DC1 activation timed out"));
        assert_ne!(head.0.borrow().pending & WIN_A, 0);
        assert_eq!(head.0.borrow().active(WIN_A, WIN_OPTIONS), WIN_ENABLE);
        {
            let mut m = head.0.borrow_mut();
            m.freeze = false;
            m.stuck_field = Some((WIN_A, WIN_OPTIONS));
            m.suppress_frames = true;
        }
        assert_eq!(head.stop(), Err("DC1 activation timed out"));
        assert_eq!(head.0.borrow().active(GENERAL, COMMAND), 0);
        assert_eq!(head.0.borrow().pending, 0);
        head.0.borrow_mut().stuck_field = None;
        head.stop().unwrap();
        assert!(head.idle());
        let request_count = head.0.borrow().requests.len();
        head.stop().unwrap();
        assert_eq!(head.0.borrow().requests.len(), request_count);
    }

    #[test]
    fn stop_requires_every_local_fetch_and_the_active_command() {
        for (group, reg, stale_value) in [
            (GENERAL, COMMAND, CONTINUOUS),
            (GENERAL, OUTPUT, 1 << 16),
            (4, WIN_OPTIONS, WIN_ENABLE),
            (8, WIN_OPTIONS, WIN_ENABLE),
        ] {
            let head = started();
            {
                let mut m = head.0.borrow_mut();
                m.active.insert((group, reg), stale_value);
                m.stuck_field = Some((group, reg));
            }
            assert!(head.stop().is_err());
            assert_eq!(head.0.borrow().pending, 0);
            head.0.borrow_mut().stuck_field = None;
            head.stop().unwrap();
            assert!(head.idle());
        }
    }

    #[test]
    fn fifo_fault_fails_flip_and_invalid_layout_never_arms_dma() {
        let head = started();
        let count = head.0.borrow().requests.len();
        assert!(head.flip(HD, (1 << 34) - 4096, 5120, 12, 0).is_err());
        assert!(head.flip(HD, BACK, 5119, 12, 0).is_err());
        assert!(head.flip(HD, BACK, 5120, 12, 1).is_err());
        assert!(head.flip(FHD, BACK, 7680, 12, 0).is_err());
        assert_eq!(head.0.borrow().requests.len(), count);
        head.0
            .borrow_mut()
            .immediate
            .insert(INT_STATUS, FIFO_ERROR | FRAME_END);
        assert_eq!(flip(&head), Err("DC1 window A FIFO fault"));
        head.stop().unwrap();
    }
}

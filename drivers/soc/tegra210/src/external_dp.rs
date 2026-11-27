// SPDX-License-Identifier: GPL-2.0-only
//! Icosa SKU0 DC1 -> SOR1 -> DPAUX1 (Switchroot cf785c4c, tegra210-nx-common-disp.dtsi).
//! Tegra210 CAR and DISB sequencing: Switchroot 2d0059fd, clk-tegra210.c,
//! clk-tegra-periph.c, powergate-t21x.c. Hekate e487de8f hw_init.c leaves
//! MC_CAPA/CBPA/CCPA/CDPA running; the panel uses the shared SOR partition.
//! The peripheral CAR lock serializes register changes. Polls and reset delays
//! run outside it. A sleepable PMC lock covers complete DPD transactions.
#[cfg(target_os = "none")]
mod board;
#[cfg(target_os = "none")]
pub(crate) mod platform;
mod pll;
mod supplies;
#[cfg(test)]
mod tests;

use pll::{LINK, PIXEL, Settings};
type Error = &'static str;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Reg {
    Car(usize),
    Pmc(usize),
    Mc(usize),
    Sor(usize),
}
use Reg::{Car, Mc, Pmc, Sor};

trait Bus {
    fn with_car<T>(&self, op: impl FnOnce() -> T) -> T;
    fn with_dpd<T>(&self, op: impl FnOnce() -> T) -> T;
    fn read(&self, reg: Reg) -> u32;
    fn write(&self, reg: Reg, val: u32);
    fn now_us(&self) -> u64;
    fn delay(&self, us: u32);

    fn get(&self, reg: Reg) -> Result<u32, Error> {
        match self.read(reg) {
            u32::MAX => Err("unreadable external display register"),
            value => Ok(value),
        }
    }
    fn update(&self, reg: Reg, mask: u32, bits: u32) -> Result<(), Error> {
        self.with_car(|| {
            let old = self.get(reg)?;
            if old & mask != bits {
                self.write(reg, old & !mask | bits);
                if self.get(reg)? & mask != bits {
                    return Err("external display register readback mismatch");
                }
            }
            Ok(())
        })
    }
    fn wait(
        &self,
        timeout_us: u32,
        mut ready: impl FnMut() -> Result<bool, Error>,
    ) -> Result<(), Error> {
        let start = self.now_us();
        loop {
            if ready()? {
                return Ok(());
            }
            let elapsed = self.now_us().saturating_sub(start);
            if elapsed >= u64::from(timeout_us) {
                return Err("external display clock or power transition timed out");
            }
            // Most acks arrive within a few APB accesses. Yield the worker
            // during a slow PMC transition after the initial short polls.
            let step = if elapsed >= 100 && timeout_us >= 10_000 {
                1_000
            } else {
                2
            };
            self.delay(step.min(timeout_us - elapsed as u32));
        }
    }
    fn poll(&self, reg: Reg, mask: u32, bits: u32, timeout_us: u32) -> Result<(), Error> {
        self.wait(timeout_us, || Ok(self.get(reg)? & mask == bits))
    }
}

#[derive(Clone, Copy)]
struct Bit {
    status: usize,
    set: usize,
    clear: usize,
    bit: u32,
}
impl Bit {
    const fn new(status: usize, set: usize, index: u32) -> Self {
        Self {
            status,
            set,
            clear: set + 4,
            bit: 1 << index,
        }
    }
    fn get(self, io: &impl Bus) -> Result<bool, Error> {
        Ok(io.get(Car(self.status))? & self.bit != 0)
    }
    fn set(self, io: &impl Bus, on: bool) -> Result<(), Error> {
        let changed = io.with_car(|| {
            if self.get(io)? == on {
                return Ok(false);
            }
            io.write(Car(if on { self.set } else { self.clear }), self.bit);
            if self.get(io)? != on {
                return Err("external display gate/reset readback mismatch");
            }
            Ok(true)
        })?;
        if changed {
            io.delay(2);
        }
        Ok(())
    }
}
#[derive(Clone, Copy)]
struct Engine {
    gate: Bit,
    reset: Bit,
}
#[derive(Clone, Copy)]
struct State {
    clock: bool,
    reset: bool,
}
impl Engine {
    fn state(self, io: &impl Bus) -> Result<State, Error> {
        Ok(State {
            clock: self.gate.get(io)?,
            reset: self.reset.get(io)?,
        })
    }
    fn start(self, io: &impl Bus, hold_us: u32, settle_us: u32) -> Result<(), Error> {
        self.gate.set(io, true)?;
        io.delay(hold_us);
        self.reset.set(io, false)?;
        io.delay(settle_us);
        Ok(())
    }
    fn restore(self, io: &impl Bus, old: State) -> Result<(), Error> {
        self.reset.set(io, true)?;
        self.gate.set(io, old.clock)?;
        self.reset.set(io, old.reset)
    }
}
const HEAD: Engine = Engine {
    gate: Bit::new(0x10, 0x320, 26),
    reset: Bit::new(4, 0x300, 26),
};
const SOR: Engine = Engine {
    gate: Bit::new(0x280, 0x284, 23),
    reset: Bit::new(0x28c, 0x290, 23),
};
const AUX: Engine = Engine {
    gate: Bit::new(0x298, 0x29c, 15),
    reset: Bit::new(0x2a4, 0x2a8, 15),
};
const SAFE: Bit = Bit::new(0x298, 0x29c, 30); // PLLP / 17 = 24 MHz
const LA: Engine = Engine {
    gate: Bit::new(0x18, 0x330, 12),
    reset: Bit::new(0xc, 0x310, 12),
};
const HEAD_SOURCE: Reg = Car(0x13c);
const SOR_SOURCE: Reg = Car(0x410);
const PARENT: u32 = 7 << 29;
const PLLD2_OUT0: u32 = 5 << 29;
const MODULE_MUX: u32 = 3 << 14;
const SOR_FIELDS: u32 = PARENT | MODULE_MUX | 0xff;
const DISB: u32 = 1 << 19;
const SHARED_SOR: u32 = 1 << 17;
const HDMI_PAD: u32 = 1 << 28;
const MC_DCB: u32 = 1 << 3;
const POWER: Reg = Pmc(0x38);
const CLAMP: Reg = Pmc(0x2c);
const TOGGLE: Reg = Pmc(0x30);
const FLUSH: Reg = Mc(0x200);
const OVERRIDE: Reg = Car(0xf8);

fn prerequisites(io: &impl Bus) -> Result<(), Error> {
    if io.get(Car(0x50))? >> 26 != 5 << 2 {
        return Err("external display needs Icosa 38.4 MHz PLL reference");
    }
    let pllp = io.get(Car(0xa0))?;
    let m = pllp & 255;
    let n = (pllp >> 10) & 255;
    if pllp & (3 << 30 | 1 << 27) != 1 << 30 | 1 << 27 || m == 0 || 384 * n != 4080 * m {
        return Err("external display requires inherited locked 408 MHz PLLP");
    }
    if io.get(POWER)? & SHARED_SOR == 0
        || io.get(CLAMP)? & SHARED_SOR != 0
        || io.get(Car(0x10))? & (1 << 28) == 0
        || io.get(Car(4))? & (1 << 28) != 0
    {
        return Err("inherited host1x/shared SOR partition unavailable");
    }
    Ok(())
}

// PMC has a toggle command, not separate ON/OFF strobes. A busy-bit ack can
// precede the status change. Remember a submitted command across retries.
fn power(io: &impl Bus, pending: &mut Option<bool>, on: bool) -> Result<(), Error> {
    if let Some(previous) = *pending {
        io.poll(TOGGLE, 1 << 8, 0, 100_000)?;
        io.poll(POWER, DISB, if previous { DISB } else { 0 }, 100_000)?;
        *pending = None;
    }
    // Test command-port readiness and issue the conditional toggle in a
    // single critical section shared with VIC, XUSB, PCIe, APE and NVDEC.
    io.wait(100_000, || {
        io.with_car(|| {
            if io.get(TOGGLE)? & (1 << 8) != 0 {
                return Ok(false);
            }
            if (io.get(POWER)? & DISB != 0) != on {
                *pending = Some(on);
                io.write(TOGGLE, 1 << 8 | 19);
            }
            Ok(true)
        })
    })?;
    if pending.is_some() {
        io.poll(TOGGLE, 1 << 8, 0, 100_000)?;
        io.poll(POWER, DISB, if on { DISB } else { 0 }, 100_000)?;
        *pending = None;
        io.delay(10);
    }
    Ok(())
}

fn drain(io: &impl Bus) -> Result<(), Error> {
    io.update(FLUSH, MC_DCB, MC_DCB)?;
    for _ in 0..10_000 {
        io.delay(10);
        let first = io.get(Mc(0x204))?;
        if first & MC_DCB != 0 {
            let mut stable = true;
            for _ in 0..5 {
                stable &= io.get(Mc(0x204))? == first;
            }
            if stable {
                return Ok(());
            }
        }
    }
    Err("DC1 MC client did not drain")
}

fn pad(io: &impl Bus, dpd: bool, saved: &mut Option<(u32, u32)>) -> Result<(), Error> {
    io.with_dpd(|| pad_locked(io, dpd, saved))
}

fn pad_locked(io: &impl Bus, dpd: bool, saved: &mut Option<(u32, u32)>) -> Result<(), Error> {
    let status = Pmc(0x1bc);
    let want = if dpd { HDMI_PAD } else { 0 };
    if saved.is_none() && io.get(status)? & HDMI_PAD == want {
        return Ok(());
    }
    let (sample, timer) = match *saved {
        Some(original) => original,
        None => {
            let original = (io.get(Pmc(0x20))?, io.get(Pmc(0x1c8))?);
            *saved = Some(original);
            original
        }
    };
    // 82 APB cycles >= 200 ns even at the T210 SCLK maximum (408 MHz).
    // Restore the shared sample enable and timer even after a missed ack.
    let op = (|| {
        io.update(Pmc(0x20), 1, 1)?;
        io.update(Pmc(0x1c8), 0xff, 82)?;
        io.with_car(|| io.write(Pmc(0x1b8), HDMI_PAD | if dpd { 2 << 30 } else { 1 << 30 }));
        io.delay(10);
        io.poll(status, HDMI_PAD, want, 250_000)
    })();
    let t = io.update(Pmc(0x1c8), 0xff, timer & 0xff);
    let s = io.update(Pmc(0x20), 1, sample & 1);
    t.and(s).and(op)?;
    *saved = None;
    Ok(())
}

struct Mode {
    hz: u32,
    head: State,
    sor: State,
    la: State,
    ovr: u32,
    head_source: u32,
    sor_source: u32,
    powered: bool,
    flush: u32,
    pixel: Option<[u32; 5]>,
    link: Option<[u32; 5]>,
    pending_power: Option<bool>,
    mbist_touched: bool,
    engines_touched: bool,
    sor_touched: bool,
    head_can_dma: bool,
    ready: bool,
}

impl Mode {
    fn snapshot(io: &impl Bus, hz: u32) -> Result<Self, Error> {
        let pixel = Settings::for_rate(hz)?;
        let link = Settings::for_rate(270_000_000)?;
        let powered = io.get(POWER)? & DISB != 0;
        let head = HEAD.state(io)?;
        let sor = SOR.state(io)?;
        if (powered && (io.get(CLAMP)? & DISB != 0 || head.clock && !head.reset))
            || sor.clock && !sor.reset
        {
            return Err("cannot claim an inherited active external head or SOR1");
        }
        let pixel = PIXEL.snapshot(io, &pixel)?;
        let link = LINK.snapshot(io, &link)?;
        if pixel.is_some()
            && io.get(Car(0x10))? & (1 << 27) != 0
            && io.get(Car(0x138))? & PARENT == PLLD2_OUT0
            || link.is_some()
                && io.get(Car(0x280))? & (1 << 22) != 0
                && io.get(Car(0x414))? & (1 << 14) != 0
        {
            return Err("display PLL selected by another running output");
        }
        if !powered
            && (io.get(Car(0x280))? & (3 << 7) != 3 << 7
                || io.get(Car(0x298))? & (3 << 8) != 3 << 8)
        {
            return Err("DISB MBIST requires inherited MC partition clocks");
        }
        Ok(Self {
            hz,
            head,
            sor,
            la: LA.state(io)?,
            ovr: io.get(OVERRIDE)? & 4,
            head_source: io.get(HEAD_SOURCE)? & PARENT,
            sor_source: io.get(SOR_SOURCE)? & SOR_FIELDS,
            powered,
            flush: io.get(FLUSH)? & MC_DCB,
            pixel,
            link,
            pending_power: None,
            mbist_touched: false,
            engines_touched: false,
            sor_touched: false,
            head_can_dma: false,
            ready: false,
        })
    }
    fn start(&mut self, io: &impl Bus) -> Result<(), Error> {
        if self.pixel.is_some() {
            if let Err(error) = PIXEL.start(io, &Settings::for_rate(self.hz)?) {
                self.link = None;
                return Err(error);
            }
        }
        if self.link.is_some() {
            LINK.start(io, &Settings::for_rate(270_000_000)?)?;
        }
        self.engines_touched = true;
        HEAD.reset.set(io, true)?;
        HEAD.gate.set(io, false)?;
        // Unclamp with the low clk_m source before selecting the mode clock.
        io.update(HEAD_SOURCE, PARENT, 6 << 29)?;
        power(io, &mut self.pending_power, true)?;
        HEAD.gate.set(io, true)?;
        io.delay(10);
        if !self.powered {
            io.with_car(|| io.write(Pmc(0x34), DISB));
            io.poll(CLAMP, DISB, 0, 1000)?;
            io.delay(10);
        }
        HEAD.reset.set(io, false)?;
        io.delay(10);
        self.head_can_dma = true;
        io.update(FLUSH, MC_DCB, 0)?;
        io.delay(10);
        if !self.powered {
            self.mbist_touched = true;
            LA.start(io, 5, 2)?;
            io.update(OVERRIDE, 4, 4)?;
            io.delay(2);
            io.update(OVERRIDE, 4, self.ovr)?;
            LA.reset.set(io, self.la.reset)?;
            LA.gate.set(io, self.la.clock)?;
            self.mbist_touched = false;
        }
        HEAD.gate.set(io, false)?;
        io.update(HEAD_SOURCE, PARENT, PLLD2_OUT0)?;
        HEAD.gate.set(io, true)?;
        self.sor_touched = true;
        SOR.reset.set(io, true)?;
        SOR.gate.set(io, false)?;
        io.update(SOR_SOURCE, SOR_FIELDS, PLLD2_OUT0)?; // pixel parent / 1 and safe module clock
        // Switchroot sor.h tegra_sor_reset: assert for 2 ms, settle 1 ms.
        SOR.start(io, 2_000, 1_000)?;
        self.ready = true;
        Ok(())
    }
    fn restore(&mut self, io: &impl Bus) -> Result<(), Error> {
        self.ready = false;
        if self.engines_touched {
            if self.head_can_dma {
                HEAD.gate.set(io, true)?;
                drain(io)?;
                HEAD.reset.set(io, true)?;
                io.delay(10);
                self.head_can_dma = false;
            }
            if self.sor_touched {
                SOR.reset.set(io, true)?;
                SOR.gate.set(io, false)?;
                io.update(SOR_SOURCE, SOR_FIELDS, self.sor_source)?;
                SOR.restore(io, self.sor)?;
                self.sor_touched = false;
            }
            HEAD.reset.set(io, true)?;
            HEAD.gate.set(io, false)?;
            io.update(HEAD_SOURCE, PARENT, self.head_source)?;
            if self.mbist_touched {
                io.update(OVERRIDE, 4, self.ovr)?;
                LA.reset.set(io, self.la.reset)?;
                LA.gate.set(io, self.la.clock)?;
                self.mbist_touched = false;
            }
            if !self.powered {
                power(io, &mut self.pending_power, false)?;
            }
            io.update(FLUSH, MC_DCB, self.flush)?;
            HEAD.restore(io, self.head)?;
            self.engines_touched = false;
        }
        if let Some(saved) = self.link {
            LINK.restore(io, saved)?;
            self.link = None;
        }
        if let Some(saved) = self.pixel {
            PIXEL.restore(io, saved)?;
            self.pixel = None;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Clocks {
    aux: Option<(State, bool)>,
    aux_ready: bool,
    mode: Option<Mode>,
    pad_dpd: Option<bool>,
    pad_sample: Option<(u32, u32)>,
}
impl Clocks {
    fn prepare_aux(&mut self, io: &impl Bus) -> Result<(), Error> {
        if self.aux_ready {
            return Ok(());
        }
        if self.aux.is_some() {
            return Err("restore incomplete DPAUX1 setup first");
        }
        let (old, safe) = io.with_car(|| {
            prerequisites(io)?;
            Ok::<_, Error>((AUX.state(io)?, SAFE.get(io)?))
        })?;
        if old.clock && !old.reset {
            return Err("DPAUX1 already active");
        }
        self.aux = Some((old, safe));
        // dp.c tegra_dp_dpaux_init holds sor_safe across HPD/AUX access;
        // dpaux.c tegra_dpaux_reset holds reset for 2 ms, settles for 1 ms.
        let op = (|| {
            SAFE.set(io, true)?;
            AUX.reset.set(io, true)?;
            AUX.start(io, 2_000, 1_000)
        })();
        if op.is_err() {
            self.restore(io)?;
        } else {
            self.aux_ready = true;
        }
        op
    }
    fn prepare_mode(&mut self, io: &impl Bus, hz: u32) -> Result<u32, Error> {
        if !self.aux_ready {
            return Err("prepare DPAUX1 first");
        }
        if !matches!(hz, 74_250_000 | 148_500_000) {
            return Err("unsupported external display pixel clock");
        }
        if let Some(m) = &self.mode {
            return if m.ready && m.hz == hz {
                Ok(0)
            } else {
                Err("restore previous external display mode first")
            };
        }
        let initial = io.with_car(|| {
            prerequisites(io)?;
            Mode::snapshot(io, hz)
        })?;
        let m = self.mode.insert(initial);
        if let Err(e) = m.start(io) {
            m.restore(io)?;
            self.mode = None;
            return Err(e);
        }
        // Switchroot mode.c: PCD1 | ((parent * 2 / pixel_hz) - 2).
        Ok(0)
    }
    fn ready(&self) -> Result<(), Error> {
        if self.mode.as_ref().is_some_and(|m| m.ready) {
            Ok(())
        } else {
            Err("SOR1 mode clocks are not prepared")
        }
    }
    fn set_pad_power(&mut self, io: &impl Bus, on: bool) -> Result<(), Error> {
        self.ready()?;
        if !on && io.get(SOR_SOURCE)? & MODULE_MUX != 0 {
            return Err("select safe SOR1 clock before disabling its pads");
        }
        if self.pad_dpd.is_none() {
            self.pad_dpd = Some(io.get(Pmc(0x1bc))? & HDMI_PAD != 0);
        }
        pad(io, !on, &mut self.pad_sample)
    }
    fn select_link_clock(&mut self, io: &impl Bus, link: bool) -> Result<(), Error> {
        self.ready()?;
        if link {
            if !SOR.gate.get(io)? {
                return Err("SOR1 module clock is gated");
            }
            let ctrl = io.get(Sor(0x13 * 4))?;
            if ctrl & 3 != 3
                || !matches!((ctrl >> 2) & 31, 6 | 10)
                || io.get(Pmc(0x1bc))? & HDMI_PAD != 0
            {
                return Err("SOR1 needs powered pads and differential RBR/HBR clock");
            }
        }
        let want = if link { 1 << 14 } else { 0 };
        if io.get(SOR_SOURCE)? & MODULE_MUX != want {
            // Switchroot sor.c HW bug 1425607: gate around either parent switch.
            SOR.gate.set(io, false)?;
            let write = io.update(SOR_SOURCE, MODULE_MUX, want);
            SOR.gate.set(io, true)?;
            write?;
        }
        SOR.gate.set(io, true)
    }
    fn restore(&mut self, io: &impl Bus) -> Result<(), Error> {
        self.aux_ready = false;
        if let Some(dpd) = self.pad_dpd {
            if self.mode.as_ref().is_some_and(|m| m.ready) {
                self.select_link_clock(io, false)?;
            }
            pad(io, dpd, &mut self.pad_sample)?;
            self.pad_dpd = None;
        }
        if let Some(m) = &mut self.mode {
            m.restore(io)?;
            self.mode = None;
        }
        if let Some((old, safe)) = self.aux {
            AUX.restore(io, old)?;
            SAFE.set(io, safe)?;
            self.aux = None;
        }
        self.aux_ready = false;
        Ok(())
    }
}

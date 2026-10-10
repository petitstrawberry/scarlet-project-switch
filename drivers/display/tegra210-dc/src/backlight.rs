// SPDX-License-Identifier: GPL-2.0-only
//! Adopt Hekate's LCD PWM0 without changing its period, pads or shared clock.
//! See Hekate v6.5.3 bdk/display/di.c and Linux v6.12 drivers/pwm/pwm-tegra.c.

const ENABLE: u32 = 1 << 31;
// Tegra has 256 duty ticks, including the 0x100 constant-high encoding.
const DUTY_MASK: u32 = 0x1ff << 16;
const DUTY_TICKS: u32 = 256;

fn brightness(register: u32) -> Result<u8, &'static str> {
    let duty = (register & DUTY_MASK) >> 16;
    if duty > DUTY_TICKS {
        return Err("Unsupported inherited Tegra PWM0 duty");
    }
    if register & ENABLE == 0 {
        return Ok(0);
    }
    Ok(((duty * 100 + DUTY_TICKS / 2) / DUTY_TICKS) as u8)
}

fn with_brightness(register: u32, percent: u8) -> Result<u32, &'static str> {
    if percent > 100 {
        return Err("Display brightness must be in the range 0..=100");
    }
    brightness(register)?;
    let duty = (u32::from(percent) * DUTY_TICKS + 50) / 100;
    // Keep PWM enabled even at zero, driving the LCD PWM output low. Never
    // gate/reset the PWM controller: PWM1 controls the cooling fan.
    Ok((register & !DUTY_MASK) | ENABLE | (duty << 16))
}

#[cfg(target_os = "none")]
mod runtime {
    use super::{brightness, with_brightness};
    use scarlet::{arch, sync::Mutex, vm};

    pub struct Backlight {
        pwm: usize,
        lock: Mutex<()>,
    }

    impl Backlight {
        pub fn adopt() -> Result<Self, &'static str> {
            let car = vm::ioremap(0x60006000, 0x20)?;
            let pads = vm::ioremap(0x70003000, 0x294)?;
            let gpio = vm::ioremap(0x6000d000, 0x600)?;
            let read = |address| unsafe { arch::mmio::read32(address) };
            // Only the inherited LCD PWM0 wiring is supported. OLED uses DSI
            // commands instead; don't offer PWM brightness on that handoff.
            if read(car + 0x10) & (1 << 17) == 0
                || read(car + 0x04) & (1 << 17) != 0
                || read(pads + 0x1fc) & (3 | (1 << 4)) != 1
                || read(gpio + 0x504) & 1 != 0
            // V0 must be in peripheral mode.
            {
                return Err("Inherited LCD backlight PWM0 is not active");
            }
            let pwm = vm::ioremap(0x7000a000, 0x10)?;
            brightness(read(pwm))?;
            Ok(Self {
                pwm,
                lock: Mutex::new(()),
            })
        }

        pub fn get(&self) -> Result<u8, &'static str> {
            let _guard = self.lock.lock();
            brightness(unsafe { arch::mmio::read32(self.pwm) })
        }

        pub fn set(&self, percent: u8) -> Result<(), &'static str> {
            let _guard = self.lock.lock();
            let previous = unsafe { arch::mmio::read32(self.pwm) };
            let value = with_brightness(previous, percent)?;
            unsafe { arch::mmio::write32(self.pwm, value) };
            arch::io_mb();
            if unsafe { arch::mmio::read32(self.pwm) } != value {
                unsafe { arch::mmio::write32(self.pwm, previous) };
                arch::io_mb();
                return Err("Tegra PWM0 backlight duty did not read back");
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "none")]
pub use runtime::Backlight;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopts_firmware_duty_and_disabled_output() {
        assert_eq!(brightness(ENABLE | (150 << 16)), Ok(59));
        assert_eq!(brightness(150 << 16), Ok(0));
        assert_eq!(brightness(ENABLE | (255 << 16)), Ok(100));
        assert_eq!(brightness(ENABLE | (256 << 16)), Ok(100));
        assert!(brightness(u32::MAX).is_err());
    }

    #[test]
    fn every_percentage_round_trips_and_preserves_period() {
        let inherited = ENABLE | (150 << 16) | 0x1234;
        let mut previous_duty = 0;
        for percent in 0..=100 {
            let value = with_brightness(inherited, percent).unwrap();
            assert_eq!(brightness(value), Ok(percent));
            assert_eq!(value & !DUTY_MASK, inherited & !DUTY_MASK);
            let duty = (value & DUTY_MASK) >> 16;
            assert!(duty >= previous_duty);
            previous_duty = duty;
        }
        assert_eq!(with_brightness(inherited, 0), Ok(ENABLE | 0x1234));
        assert_eq!(
            with_brightness(inherited, 100),
            Ok(ENABLE | (256 << 16) | 0x1234)
        );
    }

    #[test]
    fn zero_can_be_reenabled_and_invalid_requests_are_rejected() {
        assert_eq!(brightness(with_brightness(0x1234, 50).unwrap()), Ok(50));
        for percent in [101, 255] {
            assert!(with_brightness(ENABLE, percent).is_err());
        }
        assert!(with_brightness(ENABLE | (257 << 16), 50).is_err());
    }
}

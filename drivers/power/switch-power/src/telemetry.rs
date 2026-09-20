//! Read-only register protocol, also exercised with a host-side fake bus.
use scarlet_abi::power_supply::{ChargeState, PowerSupplyState};

pub const GAUGE_ADDRESS: u8 = 0x36;
pub const CHARGER_ADDRESS: u8 = 0x6b;
pub trait Registers {
    /// Set only the register pointer, then read. No register-value writes.
    fn read(&self, address: u8, register: u8, bytes: &mut [u8]) -> Result<(), &'static str>;
}
pub fn word(io: &impl Registers, register: u8) -> Result<u16, &'static str> {
    let mut bytes = [0; 2];
    io.read(GAUGE_ADDRESS, register, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}
pub fn byte(io: &impl Registers, register: u8) -> Result<u8, &'static str> {
    let mut bytes = [0];
    io.read(CHARGER_ADDRESS, register, &mut bytes)?;
    Ok(bytes[0])
}
pub fn external_online(status: u8) -> bool {
    // PG is authoritative even while input type detection is incomplete.
    // OTG exports power; it must never appear as external input power.
    status & 4 != 0 && status >> 6 != 3
}
pub fn charge_state(status: u8, current_ua: i32) -> ChargeState {
    if !external_online(status) || current_ua < 0 {
        // A connected adapter can still leave the battery supplementing load.
        return ChargeState::Discharging;
    }
    match (status >> 4) & 3 {
        1 | 2 => ChargeState::Charging,
        3 => ChargeState::Full,
        _ => ChargeState::NotCharging,
    }
}
pub fn gauge(
    io: &impl Registers,
    rsense_uohm: u32,
    charger: bool,
) -> Result<PowerSupplyState, &'static str> {
    if !(100..=1_000_000).contains(&rsense_uohm) {
        return Err("invalid gauge sense resistor");
    }
    let status = word(io, 0x00)?;
    let mut state = PowerSupplyState {
        present: Some(status & 8 == 0),
        ..Default::default()
    };
    if state.present == Some(false) {
        return Ok(state);
    }
    // POR means the learned battery model has not been restored. Voltage,
    // temperature and current are still readable, but SOC is not trustworthy.
    if status & 2 == 0 {
        state.capacity_permille = Some((u32::from(word(io, 0x06)?) * 10 / 256).min(1000));
    }
    state.voltage_uv = Some(u32::from(word(io, 0x09)? >> 3) * 625);
    state.temperature_mc = Some(i32::from(word(io, 0x08)? as i16) * 1000 / 256);
    // AvgCurrent is signed. 1.5625 uV / Rsense per count, converted to uA.
    let current = i64::from(word(io, 0x0b)? as i16) * 1_562_500 / i64::from(rsense_uohm);
    state.current_ua = Some(i32::try_from(current).map_err(|_| "gauge current overflow")?);
    if charger {
        state.charge_state = byte(io, 0x08)
            .ok()
            .map(|status| charge_state(status, current as i32));
    }
    Ok(state)
}
pub fn input(io: &impl Registers) -> Result<PowerSupplyState, &'static str> {
    let status = byte(io, 0x08)?;
    let limit = byte(io, 0x00)? & 7;
    Ok(PowerSupplyState {
        online: Some(external_online(status)),
        input_current_limit_ua: Some(
            [
                100_000, 150_000, 500_000, 900_000, 1_200_000, 1_500_000, 2_000_000, 3_000_000,
            ][limit as usize],
        ),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Bus {
        status: u16,
        charger: u8,
        fail: bool,
    }
    impl Registers for Bus {
        fn read(&self, address: u8, register: u8, bytes: &mut [u8]) -> Result<(), &'static str> {
            if self.fail {
                return Err("NACK");
            }
            match (address, register) {
                (GAUGE_ADDRESS, reg) => {
                    let value: u16 = match reg {
                        0x00 => self.status,
                        0x06 => 50 * 256 + 128,
                        0x09 => 48000,
                        0x08 => (-2560i16) as u16,
                        0x0b => (-1600i16) as u16,
                        _ => panic!("unexpected gauge register"),
                    };
                    bytes.copy_from_slice(&value.to_le_bytes());
                }
                (CHARGER_ADDRESS, 0x08) => bytes[0] = self.charger,
                (CHARGER_ADDRESS, 0x00) => bytes[0] = 7,
                _ => panic!("unexpected register read"),
            }
            Ok(())
        }
    }
    #[test]
    fn gauge_units_signed_current_and_supplement_mode() {
        let state = gauge(
            &Bus {
                status: 0,
                charger: 0xa4,
                fail: false,
            },
            10000,
            true,
        )
        .unwrap();
        assert_eq!(state.capacity_permille, Some(505));
        assert_eq!(state.voltage_uv, Some(3_750_000));
        assert_eq!(state.current_ua, Some(-250_000));
        assert_eq!(state.temperature_mc, Some(-10_000));
        assert_eq!(state.charge_state, Some(ChargeState::Discharging));
    }
    #[test]
    fn reset_absence_and_read_failure_never_become_zero_percent() {
        let bus = Bus {
            status: 2,
            charger: 0,
            fail: false,
        };
        assert_eq!(gauge(&bus, 10000, false).unwrap().capacity_permille, None);
        let absent = Bus { status: 8, ..bus };
        assert_eq!(
            gauge(&absent, 10000, false).unwrap(),
            PowerSupplyState {
                present: Some(false),
                ..Default::default()
            }
        );
        assert!(
            gauge(
                &Bus {
                    fail: true,
                    ..absent
                },
                10000,
                true
            )
            .is_err()
        );
        assert!(gauge(&bus, 0, true).is_err());
    }
    #[test]
    fn input_power_and_charging_are_independent_and_otg_is_not_input() {
        for (raw, online, state) in [
            (0, false, ChargeState::Discharging),
            (4, true, ChargeState::NotCharging),
            (0x54, true, ChargeState::Charging),
            (0xa4, true, ChargeState::Charging),
            (0xb4, true, ChargeState::Full),
            (0xc4, false, ChargeState::Discharging),
        ] {
            assert_eq!(external_online(raw), online);
            assert_eq!(charge_state(raw, 0), state);
        }
        let state = input(&Bus {
            status: 0,
            charger: 0x84,
            fail: false,
        })
        .unwrap();
        assert_eq!(state.online, Some(true));
        assert_eq!(state.current_ua, None);
        assert_eq!(state.input_current_limit_ua, Some(3_000_000));
    }
}

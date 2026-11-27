// SPDX-License-Identifier: GPL-2.0-only
pub(crate) struct Settings {
    pub divider: u32,
    pub lcr: u32,
    pub inversion: u32,
}

pub(crate) fn settings(instance: u8, baud: u32) -> Result<Settings, &'static str> {
    let (divider, lcr, inversion) = match (instance, baud) {
        (1 | 2, 1_000_000) => (49, 3, 0x0a),
        (1 | 2, 3_000_000) => (15, 7, 0x0a),
        // ODIN's nvidia,adjust-baud-rates requests +100/10000 at 115200.
        // PLLP * 2 / (divider + 2), rounded down as in clk-tegra-periph.c.
        (3, 115_200) => (437, 3, 0),
        _ => return Err("unsupported Tegra UART line configuration"),
    };
    Ok(Settings {
        divider,
        lcr,
        inversion,
    })
}

/// A successful write consumes exactly one byte. A full FIFO returns the
/// timestamp of that sample under the port lock. For H4, start a new stall
/// budget on each byte so descheduling during earlier progress costs nothing.
/// Rails retain their whole-transfer deadline.
pub(crate) fn enqueue_bytes(
    bytes: &[u8],
    rail_start: Option<u64>,
    mut try_write: impl FnMut(u8) -> Result<Option<u64>, &'static str>,
    mut wait: impl FnMut(u64),
) -> Result<(), &'static str> {
    for &byte in bytes {
        let mut stalled_since = rail_start;
        while let Some(now) = try_write(byte)? {
            let start = *stalled_since.get_or_insert(now);
            if now.saturating_sub(start) >= 100_000_000 {
                return Err(if rail_start.is_some() {
                    "rail UART TX timeout"
                } else {
                    "Bluetooth UART TX FIFO stalled"
                });
            }
            wait(start);
        }
    }
    Ok(())
}

// ODIN pulls UARTD RX/CTS down. A powered-off controller can therefore
// generate BREAK and also look CTS-ready. Start this check after REG_ON has
// settled and host RTS is asserted. Only pre-command data may be discarded.
#[derive(Default)]
pub(crate) struct RomReady {
    pub discarded: u32,
    pub errors: u32,
    pub lsr: u32,
    pub msr: u32,
    quiet_since: Option<u64>,
}
impl RomReady {
    pub fn poll(&mut self, regs: &impl crate::packet::Registers) -> bool {
        for _ in 0..32 {
            self.lsr = regs.read(0x14);
            if self.lsr & 0x1f == 0 {
                break;
            }
            self.quiet_since = None;
            self.errors |= self.lsr & 0x1e;
            if self.lsr & 1 == 0 {
                break;
            }
            let _ = regs.read(0);
            self.discarded += 1;
        }
        self.msr = regs.read(0x18);
        if self.lsr & 0x1f != 0 || self.msr & 0x10 == 0 {
            self.quiet_since = None;
            return false;
        }
        let now = regs.now_ns();
        // Multiple idle characters across two observations. A persistent
        // fault or deasserted CTS cannot turn into a successful handoff.
        now.saturating_sub(*self.quiet_since.get_or_insert(now)) >= 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bluetooth_and_rail_line_settings_are_disjoint() {
        for (port, baud, div, lcr, inv) in [
            (1, 1_000_000, 49, 3, 10),
            (2, 3_000_000, 15, 7, 10),
            (3, 115_200, (816_000_000u32).div_ceil(1_861_632) - 2, 3, 0),
        ] {
            let s = settings(port, baud).unwrap();
            assert_eq!((s.divider, s.lcr, s.inversion), (div, lcr, inv));
        }
        assert!(settings(0, 115_200).is_err());
        assert!(settings(2, 115_200).is_err());
        assert!(settings(3, 1_000_000).is_err());
    }

    #[test]
    fn h4_enqueues_once_per_byte_across_preemption_and_separate_cts_pauses() {
        let mut samples = [Some(0), None, Some(300_000_000), Some(390_000_000), None].into_iter();
        let mut written = alloc::vec::Vec::new();
        let mut waits = alloc::vec::Vec::new();
        enqueue_bytes(
            &[3, 4],
            None,
            |byte| {
                let sample = samples.next().unwrap();
                if sample.is_none() {
                    written.push(byte);
                }
                Ok(sample)
            },
            |since| waits.push(since),
        )
        .unwrap();
        assert_eq!(written, [3, 4]);
        assert_eq!(waits, [0, 300_000_000, 300_000_000]);
        assert!(samples.next().is_none());
    }

    #[test]
    fn stalled_h4_fifo_fails_mid_frame_and_rails_keep_the_whole_frame_budget() {
        for (rail, error, waits) in [
            (None, "Bluetooth UART TX FIFO stalled", 2),
            (Some(0), "rail UART TX timeout", 1),
        ] {
            let mut samples =
                [None, Some(70_000_000), Some(100_000_000), Some(170_000_000)].into_iter();
            let mut written = alloc::vec::Vec::new();
            let mut slept = 0;
            assert_eq!(
                enqueue_bytes(
                    &[1, 2, 3],
                    rail,
                    |byte| {
                        let sample = samples.next().unwrap();
                        if sample.is_none() {
                            written.push(byte);
                        } else {
                            assert_eq!(byte, 2);
                        }
                        Ok(sample)
                    },
                    |_| slept += 1
                ),
                Err(error)
            );
            assert_eq!(written, [1]);
            assert_eq!(slept, waits);
        }
    }

    #[test]
    fn pre_command_break_is_drained_but_cts_and_quiet_are_both_required() {
        use core::cell::Cell;
        struct Pins {
            now: Cell<u64>,
            cts: Cell<bool>,
            lsr: Cell<u32>,
        }
        impl crate::packet::Registers for Pins {
            fn now_ns(&self) -> u64 {
                self.now.get()
            }
            fn read(&self, offset: usize) -> u32 {
                match offset {
                    0x14 => self.lsr.get(),
                    0x18 => u32::from(self.cts.get()) << 4,
                    0 => {
                        assert_ne!(self.lsr.replace(0) & 1, 0);
                        0
                    }
                    _ => panic!("unexpected read"),
                }
            }
            fn write(&self, _: usize, _: u32) {
                panic!("poll must not arm IRQ or transmit")
            }
        }
        let pins = Pins {
            now: Cell::new(0),
            cts: Cell::new(false),
            lsr: Cell::new(0x19),
        };
        let mut ready = RomReady::default();
        assert!(!ready.poll(&pins));
        assert_eq!((ready.errors, ready.discarded), (0x18, 1));
        pins.now.set(10_000_000);
        assert!(!ready.poll(&pins));
        pins.cts.set(true);
        assert!(!ready.poll(&pins));
        pins.now.set(11_000_000);
        pins.lsr.set(0x10); // Persistent BREAK without DR must not read RBR.
        assert!(!ready.poll(&pins));
        pins.lsr.set(0);
        assert!(!ready.poll(&pins));
        pins.now.set(12_000_000);
        assert!(ready.poll(&pins));
    }
}

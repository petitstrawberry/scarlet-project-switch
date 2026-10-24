// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! STM FTM4 native multitouch. Hekate e487de8fdd6ca9c3f608d1d18c097a86355912b9,
//! bdk/input/touch.{c,h}. No calibration, firmware or EEPROM writes are performed.
extern crate alloc;
#[cfg(target_os = "none")]
mod runtime;
#[cfg(target_os = "none")]
pub use runtime::force_link;
#[cfg(not(target_os = "none"))]
pub fn force_link() {}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Event {
    Contact { slot: usize, x: i32, y: i32 },
    Leave(usize),
    Reset,
    None,
}
fn decode(raw: [u8; 8]) -> Event {
    if raw[0] == 0x10 || raw[0] == 0x0f {
        return Event::Reset;
    }
    let slot = (raw[0] >> 4) as usize;
    if slot >= 10 {
        return Event::None;
    }
    match raw[0] & 15 {
        3 | 5 => Event::Contact {
            slot,
            x: ((raw[1] as i32) << 4) | ((raw[3] >> 4) as i32),
            y: ((raw[2] as i32) << 4) | ((raw[3] & 15) as i32),
        },
        4 => Event::Leave(slot),
        _ => Event::None,
    }
}
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct Contact {
    tracking: i32,
    x: i32,
    y: i32,
}
struct Contacts {
    slots: [Option<Contact>; 10],
    reported: [Option<Contact>; 10],
    next: i32,
}
impl Contacts {
    fn new() -> Self {
        Self {
            slots: [None; 10],
            reported: [None; 10],
            next: 1,
        }
    }
    fn apply(&mut self, event: Event) {
        match event {
            Event::Contact { slot, x, y } => {
                let tracking = self.slots[slot].map(|c| c.tracking).unwrap_or_else(|| {
                    let id = self.next;
                    self.next = self.next.checked_add(1).unwrap_or(1);
                    id
                });
                self.slots[slot] = Some(Contact { tracking, x, y });
            }
            Event::Leave(slot) => self.slots[slot] = None,
            Event::Reset => self.slots.fill(None),
            Event::None => (),
        }
    }

    /// Preserve contact edges while coalescing motion within a FIFO drain.
    /// Flush remaining motion with `flush_report` at the end of the drain.
    fn apply_reported(
        &mut self,
        event: Event,
        mut report: impl FnMut([Option<Contact>; 10], [Option<Contact>; 10]),
    ) {
        if matches!(event, Event::Leave(_) | Event::Reset) {
            // Keep the final contact position before publishing its release.
            self.flush_report(&mut report);
        }
        let before = self.slots;
        self.apply(event);
        if before
            .iter()
            .zip(self.slots.iter())
            .any(|(old, new)| old.map(|c| c.tracking) != new.map(|c| c.tracking))
        {
            self.flush_report(report);
        }
    }

    fn flush_report(
        &mut self,
        mut report: impl FnMut([Option<Contact>; 10], [Option<Contact>; 10]),
    ) {
        if self.reported != self.slots {
            report(self.reported, self.slots);
            self.reported = self.slots;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_coordinates_and_contact_ids() {
        assert_eq!(
            decode([0x93, 0x4f, 0x2c, 0x0f, 0, 0, 0, 0]),
            Event::Contact {
                slot: 9,
                x: 1264,
                y: 719
            }
        );
        assert_eq!(decode([0xa3, 0, 0, 0, 0, 0, 0, 0]), Event::None);
        assert_eq!(decode([0x94, 0, 0, 0, 0, 0, 0, 0]), Event::Leave(9));
    }
    #[test]
    fn two_fingers_move_independently_and_reused_slot_has_new_tracking_id() {
        let mut state = Contacts::new();
        state.apply(Event::Contact {
            slot: 0,
            x: 100,
            y: 200,
        });
        state.apply(Event::Contact {
            slot: 1,
            x: 300,
            y: 400,
        });
        let first = state.slots[0].unwrap().tracking;
        state.apply(Event::Contact {
            slot: 0,
            x: 110,
            y: 210,
        });
        assert_eq!(state.slots[0].unwrap().tracking, first);
        assert_eq!(state.slots[1].unwrap().x, 300);
        state.apply(Event::Leave(0));
        state.apply(Event::Contact {
            slot: 0,
            x: 120,
            y: 220,
        });
        assert_ne!(state.slots[0].unwrap().tracking, first);
        state.apply(decode([0x0f, 0, 0, 0, 0, 0, 0, 0]));
        assert!(state.slots.iter().all(Option::is_none));
    }

    #[test]
    fn delayed_fifo_drain_preserves_taps_and_release_before_slot_reuse() {
        let mut state = Contacts::new();
        let mut reports = alloc::vec::Vec::new();
        // The controller can queue both edges of a tap while the worker is
        // descheduled. Reducing this batch to its final state loses the tap.
        for raw in [
            [0x03, 10, 20, 0, 0, 0, 0, 0],
            [0x04, 0, 0, 0, 0, 0, 0, 0],
            [0x03, 11, 21, 0, 0, 0, 0, 0],
        ] {
            state.apply_reported(decode(raw), |_, after| reports.push(after));
        }
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[0][0].unwrap().x, 160);
        assert!(reports[1].iter().all(Option::is_none));
        assert_eq!(reports[2][0].unwrap().x, 176);
        assert_ne!(
            reports[0][0].unwrap().tracking,
            reports[2][0].unwrap().tracking
        );
    }

    #[test]
    fn unchanged_fifo_events_are_silent_and_reset_releases_all_contacts() {
        let mut state = Contacts::new();
        let mut reports = alloc::vec::Vec::new();
        for event in [
            Event::Contact {
                slot: 0,
                x: 100,
                y: 200,
            },
            Event::Contact {
                slot: 0,
                x: 100,
                y: 200,
            },
            Event::None,
            Event::Contact {
                slot: 1,
                x: 300,
                y: 400,
            },
            Event::Reset,
            Event::Reset,
        ] {
            state.apply_reported(event, |_, after| reports.push(after));
        }
        assert_eq!(reports.len(), 3);
        assert!(reports[1][0].is_some() && reports[1][1].is_some());
        assert!(reports[2].iter().all(Option::is_none));
    }

    #[test]
    fn motion_is_coalesced_but_its_last_position_precedes_release() {
        let mut state = Contacts::new();
        let mut reports = alloc::vec::Vec::new();
        for event in [
            Event::Contact {
                slot: 0,
                x: 100,
                y: 200,
            },
            Event::Contact {
                slot: 0,
                x: 110,
                y: 210,
            },
            Event::Contact {
                slot: 0,
                x: 120,
                y: 220,
            },
            Event::Leave(0),
        ] {
            state.apply_reported(event, |_, after| reports.push(after));
        }
        state.flush_report(|_, after| reports.push(after));
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[0][0].unwrap().x, 100);
        assert_eq!(reports[1][0].unwrap().x, 120);
        assert_eq!(
            reports[0][0].unwrap().tracking,
            reports[1][0].unwrap().tracking
        );
        assert!(reports[2][0].is_none());
    }
}

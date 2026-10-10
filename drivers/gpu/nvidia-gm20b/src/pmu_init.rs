// SPDX-License-Identifier: GPL-2.0-only
//! Validation for the initial PMU message, before its queue tail is acknowledged.
//!
//! Linux falcon/msgq.c receives sizeof(nv_pmu_init_msg), then advances its
//! consumer by ALIGN(size, 4). Producer availability and consumer alignment are
//! separate: the 42-byte init payload does not require 44 published bytes.

pub(super) const PAYLOAD_BYTES: usize = 42;
pub(super) const CONSUMER_BYTES: u32 = 44;
pub(super) const MESSAGE_PENDING: u32 = 0x40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Published {
    pub offset: u32,
    pub next_tail: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QueueGeometry {
    pub offset: u32,
    pub size: u32,
    pub id: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Validated {
    pub next_tail: u32,
    pub command: QueueGeometry,
    pub message: QueueGeometry,
}

/// A startup snapshot can precede publication of the producer head, even when
/// firmware has already initialized the consumer to a nonzero queue base.
pub(super) fn published(
    interrupt: u32,
    head: u32,
    tail: u32,
    dmem: u32,
) -> Result<Option<Published>, &'static str> {
    if interrupt == u32::MAX || head == u32::MAX || tail == u32::MAX {
        return Err("PMU init queue pointer unreadable");
    }
    // Linux gt215_pmu_intr schedules init reception only on message bit 0x40.
    // Pointer initialization alone (e.g. new queue base with tail still zero)
    // must not authorize a DMEM read before firmware publishes this message.
    if interrupt & MESSAGE_PENDING == 0 {
        return Ok(None);
    }
    if head > dmem || tail > dmem || !tail.is_multiple_of(4) {
        return Err("PMU init queue pointer outside aligned DMEM");
    }
    if head == tail || head == 0 {
        return Ok(None);
    }
    let Some(available) = head.checked_sub(tail) else {
        return Ok(None);
    };
    if available < PAYLOAD_BYTES as u32 {
        return Ok(None);
    }
    // DMEM is read a word at a time, so the final padded word and the aligned
    // consumer must fit SRAM even if the producer publishes exactly 42 bytes.
    let next_tail = tail
        .checked_add(CONSUMER_BYTES)
        .filter(|end| *end <= dmem)
        .ok_or("PMU init message outside DMEM")?;
    Ok(Some(Published {
        offset: tail,
        next_tail,
    }))
}

impl Published {
    pub fn validate(
        self,
        init: &[u8; PAYLOAD_BYTES],
        dmem: u32,
    ) -> Result<Validated, &'static str> {
        if init[0] != 7 || init[1] as usize != PAYLOAD_BYTES || init[4] != 0 {
            return Err("PMU init message format invalid");
        }
        let command = queue(init, 0, dmem)?;
        let message = queue(init, 4, dmem)?;
        Ok(Validated {
            next_tail: self.next_tail,
            command,
            message,
        })
    }
}

fn queue(
    init: &[u8; PAYLOAD_BYTES],
    index: usize,
    dmem: u32,
) -> Result<QueueGeometry, &'static str> {
    let start = 8 + index * 6;
    let size = u32::from(u16::from_le_bytes([init[start], init[start + 1]]));
    let offset = u32::from(u16::from_le_bytes([init[start + 2], init[start + 3]]));
    let id = usize::from(init[start + 4]);
    if id >= 4
        || size < 32
        || !offset.is_multiple_of(4)
        || offset.checked_add(size).is_none_or(|end| end > dmem)
    {
        return Err("PMU init queue geometry invalid");
    }
    Ok(QueueGeometry { offset, size, id })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DMEM: u32 = 24576;
    const OFFSET: u32 = 0x1000;

    fn init() -> [u8; PAYLOAD_BYTES] {
        let mut message = [0; PAYLOAD_BYTES];
        message[0] = 7;
        message[1] = PAYLOAD_BYTES as u8;
        for (index, base, size, id) in [(0, 0x800u16, 256u16, 1), (4, 0x1000, 512, 0)] {
            let start = 8 + index * 6;
            message[start..start + 2].copy_from_slice(&size.to_le_bytes());
            message[start + 2..start + 4].copy_from_slice(&base.to_le_bytes());
            message[start + 4] = id;
        }
        message
    }

    #[test]
    fn initial_zero_head_with_initialized_tail_is_pending() {
        assert_eq!(published(MESSAGE_PENDING, 0, 0, DMEM), Ok(None));
        assert_eq!(published(MESSAGE_PENDING, 0, OFFSET, DMEM), Ok(None));
        assert_eq!(published(MESSAGE_PENDING, OFFSET, OFFSET, DMEM), Ok(None));
    }

    #[test]
    fn pointer_initialization_without_message_notification_is_pending() {
        assert_eq!(published(0, OFFSET, 0, DMEM), Ok(None));
        assert_eq!(published(0x10, OFFSET + 42, 0, DMEM), Ok(None));
        assert_eq!(published(0, OFFSET + 42, OFFSET, DMEM), Ok(None));
    }

    #[test]
    fn partial_publication_waits_until_the_full_payload() {
        for available in [1, 4, 8, 40, 41] {
            assert_eq!(
                published(MESSAGE_PENDING, OFFSET + available, OFFSET, DMEM),
                Ok(None)
            );
        }
        let complete = published(MESSAGE_PENDING, OFFSET + 42, OFFSET, DMEM)
            .unwrap()
            .unwrap();
        assert_eq!(complete.offset, OFFSET);
        assert_eq!(complete.next_tail, OFFSET + 44);
    }

    #[test]
    fn exact_payload_and_padded_publication_acknowledge_the_same_tail() {
        let exact = published(MESSAGE_PENDING, OFFSET + 42, OFFSET, DMEM)
            .unwrap()
            .unwrap();
        let padded = published(MESSAGE_PENDING, OFFSET + 44, OFFSET, DMEM)
            .unwrap()
            .unwrap();
        assert_eq!(exact, padded);
        let validated = exact.validate(&init(), DMEM).unwrap();
        assert_eq!(validated.next_tail, OFFSET + 44);
        assert_eq!(validated.command.offset, 0x800);
        assert_eq!(validated.command.size, 256);
        assert_eq!(validated.command.id, 1);
        assert_eq!(validated.message.offset, OFFSET);
        assert_eq!(validated.message.size, 512);
    }

    #[test]
    fn unreadable_outside_or_unaligned_pointers_are_rejected() {
        for (head, tail) in [
            (u32::MAX, OFFSET),
            (OFFSET + 42, u32::MAX),
            (DMEM + 1, OFFSET),
            (0, DMEM + 4),
            (OFFSET + 43, OFFSET + 1),
        ] {
            assert!(
                published(MESSAGE_PENDING, head, tail, DMEM).is_err(),
                "{head:#x}/{tail:#x}"
            );
        }
        assert!(published(u32::MAX, OFFSET + 42, OFFSET, DMEM).is_err());
    }

    #[test]
    fn producer_preceding_consumer_does_not_publish_a_first_message() {
        assert_eq!(
            published(MESSAGE_PENDING, OFFSET - 4, OFFSET, DMEM),
            Ok(None)
        );
    }

    #[test]
    fn final_padded_dmem_word_must_fit() {
        assert!(published(MESSAGE_PENDING, 42, 0, 42).is_err());
        assert_eq!(
            published(MESSAGE_PENDING, 42, 0, 44)
                .unwrap()
                .unwrap()
                .next_tail,
            44
        );
        assert_eq!(
            published(MESSAGE_PENDING, DMEM - 2, DMEM - 44, DMEM)
                .unwrap()
                .unwrap()
                .next_tail,
            DMEM
        );
    }

    #[test]
    fn invalid_format_never_produces_an_acknowledgeable_message() {
        let ready = published(MESSAGE_PENDING, OFFSET + 42, OFFSET, DMEM)
            .unwrap()
            .unwrap();
        for (byte, value) in [(0, 6), (1, 41), (1, 44), (4, 1)] {
            let mut invalid = init();
            invalid[byte] = value;
            assert!(ready.validate(&invalid, DMEM).is_err());
        }
    }

    #[test]
    fn invalid_command_or_message_geometry_prevents_acknowledgement() {
        let ready = published(MESSAGE_PENDING, OFFSET + 42, OFFSET, DMEM)
            .unwrap()
            .unwrap();
        for index in [0, 4] {
            let start = 8 + index * 6;
            for (field, value) in [(0, 31u16), (2, 0x801), (2, DMEM as u16)] {
                let mut invalid = init();
                invalid[start + field..start + field + 2].copy_from_slice(&value.to_le_bytes());
                assert!(ready.validate(&invalid, DMEM).is_err());
            }
            let mut invalid = init();
            invalid[start + 4] = 4;
            assert!(ready.validate(&invalid, DMEM).is_err());
        }
    }

    #[test]
    fn polling_sequence_only_exposes_a_valid_consumer_after_validation() {
        let snapshots = [
            (0, OFFSET, 0),
            (0, 0, OFFSET),
            (MESSAGE_PENDING, OFFSET + 4, OFFSET),
            (MESSAGE_PENDING, OFFSET + 40, OFFSET),
            (MESSAGE_PENDING, OFFSET + 42, OFFSET),
        ];
        let mut acknowledgements = alloc::vec::Vec::new();
        for (interrupt, head, tail) in snapshots {
            if let Some(message) = published(interrupt, head, tail, DMEM).unwrap() {
                acknowledgements.push(message.validate(&init(), DMEM).unwrap().next_tail);
            }
        }
        assert_eq!(acknowledgements, [OFFSET + 44]);
    }
}

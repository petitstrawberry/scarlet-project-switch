#![no_std]
//! Controls shared by the Switch Joy-Con kernel module and its layout daemon.

/// The board module publishes this control node even with no controllers attached.
pub const MAPPER_PATH: &str = "/dev/joycon-mapper";

/// On a raw half descriptor, queue the immediate logical player number (1..=4).
/// The driver handles physical LED feedback asynchronously. Requires PhysicalSeat.
pub const SET_JOYCON_PLAYER: u32 = 0x4750_0002;

/// On MAPPER_PATH, claim layout ownership with argument zero. Requires PhysicalSeat.
/// Duplicate descriptors share the claim, which ends at the last close.
/// Independent opens of the raw half endpoints can still read their snapshots.
pub const SCTL_JOYCON_CLAIM_MAPPER: u32 = 0x4750_0003;

/// On MAPPER_PATH, query occupied raw endpoint slots with argument zero.
/// Bits 0..7 are left slots, bits 8..15 are right slots.
pub const SCTL_JOYCON_GET_SLOTS: u32 = 0x4750_0004;
pub const SLOTS_PER_SIDE: usize = 8;

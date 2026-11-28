//! Synchronous local client for the Joy-Con mapper. Each call opens a fresh connection.
use serde::{Deserialize, Serialize};

#[doc(hidden)]
pub mod wire;

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Half {
    Left(u8),
    Right(u8),
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Upright,
    Sideways,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Single {
        half: Half,
        orientation: Orientation,
    },
    Pair {
        left: u8,
        right: u8,
    },
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pad {
    /// Player light, in 1..=4. Slot indices in Layout are zero-based.
    pub player: u8,
    pub layout: Layout,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Pass this back with an edit. Hotplug, gestures and edits invalidate old revisions.
    pub revision: u64,
    pub pads: Vec<Pad>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum Edit {
    /// Orient an independent half. Split a pair before orienting either member.
    Orient {
        half: Half,
        orientation: Orientation,
    },
    /// Combine two independent halves under the lower of their player numbers.
    Combine { left: u8, right: u8 },
    /// Restore both members of this exact pair to upright mode. The left half
    /// keeps the pair’s player number. The right gets the least-used player number.
    Split { left: u8, right: u8 },
}

/// Fetch the current layouts, including changes made by physical gestures.
pub fn list() -> Result<Snapshot, String> {
    wire::exchange(wire::Request::List)
}

/// Apply one edit and return the resulting authoritative snapshot. Refresh with
/// list() after any error before choosing a new edit.
pub fn apply(revision: u64, edit: Edit) -> Result<Snapshot, String> {
    wire::exchange(wire::Request::Apply { revision, edit })
}

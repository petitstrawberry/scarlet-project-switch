//! Runs the production SWS input policies on the host, without MMIO or syscalls.
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/input_modules.rs"));

#[cfg(test)]
mod tests {
    use super::key_repeat::{HeldKeys, KeyboardSource};
    use super::ui_gamepad::GamepadButton;

    #[test]
    fn releasing_gamepad_does_not_release_physical_keyboard() {
        let mut keys = HeldKeys::default();
        assert!(keys.update(KeyboardSource::Local(0), 28, 1));
        assert!(!keys.update(KeyboardSource::Gamepad(0), 28, 1));
        assert!(!keys.update(KeyboardSource::Gamepad(0), 28, 0));
        assert_eq!(keys.source_for_code(28), Some(KeyboardSource::Local(0)));
        assert!(keys.update(KeyboardSource::Local(0), 28, 0));
    }

    #[test]
    fn panel_modifier_release_preserves_physical_modifier() {
        let mut keys = HeldKeys::default();
        assert!(keys.update(KeyboardSource::Local(0), 29, 1));
        assert!(!keys.update(KeyboardSource::InputPanel(3), 29, 1));
        assert!(!keys.update(KeyboardSource::InputPanel(3), 29, 0));
        assert_eq!(keys.source_for_code(29), Some(KeyboardSource::Local(0)));
        assert!(keys.update(KeyboardSource::Local(0), 29, 0));
    }

    #[test]
    fn ui_button_positions_match_native_wire_identity() {
        for (button, code) in [
            (GamepadButton::South, 0x130),
            (GamepadButton::East, 0x131),
            (GamepadButton::North, 0x133),
            (GamepadButton::West, 0x134),
            (GamepadButton::LeftTrigger, 0x138),
            (GamepadButton::Home, 0x13c),
            (GamepadButton::Auxiliary1, 0x2c0),
            (GamepadButton::Auxiliary5, 0x2c4),
        ] {
            assert_eq!(
                sws_protocol::gamepad::button_bit(code),
                Some(1 << button as u8)
            );
        }
    }
}

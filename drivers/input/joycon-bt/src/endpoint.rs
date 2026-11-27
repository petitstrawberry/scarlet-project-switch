// SPDX-License-Identifier: GPL-2.0-only
use crate::profile::Side;
use alloc::{format, sync::Arc, vec, vec::Vec};
use scarlet::{
    device::{
        hid::HidWake,
        input::event_device::{EventDevice, INPUT_CAP_KEY, InputDeviceKind, InputDeviceMetadata},
        manager::DeviceManager,
    },
    object::capability::ControlOps,
    shield::authorization::{Management, permits_management},
    sync::Mutex,
};
use scarlet_switch_joycon_abi::{SET_JOYCON_PLAYER, SLOTS_PER_SIDE};

static LEFT: Mutex<[bool; SLOTS_PER_SIDE]> = Mutex::new([false; SLOTS_PER_SIDE]);
static RIGHT: Mutex<[bool; SLOTS_PER_SIDE]> = Mutex::new([false; SLOTS_PER_SIDE]);

pub(super) fn occupied_slots() -> u16 {
    let bits = |pool: &Mutex<[bool; SLOTS_PER_SIDE]>| {
        pool.lock()
            .iter()
            .enumerate()
            .fold(0u16, |mask, (i, used)| mask | (u16::from(*used) << i))
    };
    bits(&LEFT) | (bits(&RIGHT) << SLOTS_PER_SIDE)
}

struct Slot {
    pool: &'static Mutex<[bool; SLOTS_PER_SIDE]>,
    index: usize,
}
impl Slot {
    fn reserve(side: Side) -> Result<Self, &'static str> {
        let pool = match side {
            Side::Left => &LEFT,
            Side::Right => &RIGHT,
        };
        let mut slots = pool.lock();
        let index = slots
            .iter()
            .position(|used| !used)
            .ok_or("No free Joy-Con slot on this side")?;
        slots[index] = true;
        Ok(Self { pool, index })
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.pool.lock()[self.index] = false;
    }
}

pub(super) fn require_seat() -> Result<(), &'static str> {
    if scarlet::task::mytask()
        .is_some_and(|task| permits_management(&task, Management::PhysicalSeat))
    {
        Ok(())
    } else {
        Err("Physical-seat management permission denied")
    }
}

struct PlayerState {
    pending: Option<u8>,
    wake: HidWake,
}
struct Player(Mutex<Option<PlayerState>>);

impl ControlOps for Player {
    fn supported_control_commands(&self) -> Vec<(u32, &'static str)> {
        vec![(SET_JOYCON_PLAYER, "Set Joy-Con player number")]
    }
    fn control(&self, command: u32, arg: usize) -> Result<i32, &'static str> {
        if command != SET_JOYCON_PLAYER {
            return Err("Unknown Joy-Con control");
        }
        require_seat()?;
        if !(1..=4).contains(&arg) {
            return Err("Invalid Joy-Con player");
        }
        let wake = {
            let mut state = self.0.lock();
            let active = state.as_mut().ok_or("Joy-Con detached")?;
            active.pending = Some(arg as u8);
            active.wake.clone()
        };
        wake();
        Ok(0)
    }
}

pub(super) struct Endpoint {
    event: Arc<EventDevice>,
    registration: usize,
    player: Arc<Player>,
    _slot: Slot,
}
impl Endpoint {
    pub(super) fn register(
        side: Side,
        initial: &[(u16, u16, i32)],
        wake: HidWake,
    ) -> Result<Self, &'static str> {
        let (x, y) = side.stick_axes();
        let mut metadata = InputDeviceMetadata::new(InputDeviceKind::Gamepad, INPUT_CAP_KEY)
            .with_absolute_axis(x, 0, 4095)?
            .with_absolute_axis(y, 0, 4095)?;
        let prefix = match side {
            Side::Left => {
                metadata = metadata
                    .with_absolute_axis(0x10, -1, 1)?
                    .with_absolute_axis(0x11, -1, 1)?;
                "joycon-left"
            }
            Side::Right => "joycon-right",
        };
        let slot = Slot::reserve(side)?;
        let name = format!("{prefix}{}", slot.index);
        let player = Arc::new(Player(Mutex::new(Some(PlayerState {
            pending: None,
            wake,
        }))));
        let event = Arc::new(EventDevice::named_snapshot(
            name.clone(),
            metadata,
            Some(player.clone()),
        ));
        event.push_events(initial);
        let registration =
            DeviceManager::get_manager().register_device_with_name(name, event.clone());
        Ok(Self {
            event,
            registration,
            player,
            _slot: slot,
        })
    }
    pub(super) fn push(&self, frame: &[(u16, u16, i32)]) {
        self.event.push_events(frame);
    }
    pub(super) fn take_player(&self) -> Option<u8> {
        self.player.0.lock().as_mut()?.pending.take()
    }
    pub(super) fn unregister(&self) {
        self.player.0.lock().take();
        DeviceManager::get_manager().unregister_device(self.registration);
        self.event.detach_gamepad();
    }
}

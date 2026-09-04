//! Gamepad control.
//!
//! gilrs is used rather than raw HID because it carries the
//! SDL_GameControllerDB mappings: `Button::Start` is the Start button on
//! whatever pad is plugged in, instead of an index that only lines up on
//! XInput-style controllers.

use gilrs::{Axis, Button, EventType, Gilrs};

/// Sticks rest a little off-centre, so anything smaller than this counts as
/// centred. Past it the value is rescaled so control starts from zero.
const DEADZONE: f32 = 0.15;

/// What a button press means to this app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Start — begin playback.
    Play,
    /// Select — stop playback.
    Stop,
    /// L1 — fly to the previous place.
    PreviousPlace,
    /// R1 — fly to the next place.
    NextPlace,
    /// A — fire the drop effect.
    Drop,
}

fn action_for(button: Button) -> Option<Action> {
    match button {
        Button::Start => Some(Action::Play),
        Button::Select => Some(Action::Stop),
        // `LeftTrigger` is the L1 / LB bumper; `LeftTrigger2` would be L2 / LT.
        Button::LeftTrigger => Some(Action::PreviousPlace),
        Button::RightTrigger => Some(Action::NextPlace),
        // Cardinal naming: South is A on an Xbox pad, cross on a PlayStation one.
        Button::South => Some(Action::Drop),
        _ => None,
    }
}

/// Continuous stick and D-pad state, sampled once per frame. Each field is in
/// `-1.0..=1.0`.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Sticks {
    /// Left stick: pans the map. Positive y is down the screen.
    pub pan: (f32, f32),
    /// Right stick's x axis: turns the map.
    pub turn: f32,
    /// D-pad: changes the zoom level. Positive zooms in.
    pub zoom: f32,
}

impl Sticks {
    /// Whether anything is being held, so a fly-to knows to give way.
    pub fn active(&self) -> bool {
        self.pan != (0.0, 0.0) || self.turn != 0.0 || self.zoom != 0.0
    }
}

/// Applies the deadzone and rescales what is left to the full range.
fn shape(value: f32) -> f32 {
    if value.abs() < DEADZONE {
        return 0.0;
    }
    let scaled = (value.abs() - DEADZONE) / (1.0 - DEADZONE);
    scaled.min(1.0) * value.signum()
}

/// Reads connected gamepads. An unusable input stack is not fatal — the app
/// simply runs without a pad.
pub struct Gamepads {
    gilrs: Option<Gilrs>,
    name: Option<String>,
}

impl Gamepads {
    pub fn new() -> Self {
        let gilrs = match Gilrs::new() {
            Ok(gilrs) => Some(gilrs),
            // The platform has no gamepad support, but the dummy context this
            // carries is still safe to poll.
            Err(gilrs::Error::NotImplemented(gilrs)) => {
                eprintln!("gamepad input is not supported on this platform");
                Some(gilrs)
            }
            Err(error) => {
                eprintln!("gamepad input unavailable: {error}");
                None
            }
        };
        let mut gamepads = Self { gilrs, name: None };
        gamepads.refresh_name();
        if let Some(name) = gamepads.name() {
            println!("gamepad connected: {name}");
        }
        gamepads
    }

    /// Name of the first connected pad, for the status line.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Drains pending events and returns the actions they map to, in order.
    pub fn poll(&mut self) -> Vec<Action> {
        let Some(gilrs) = self.gilrs.as_mut() else {
            return Vec::new();
        };

        let mut actions = Vec::new();
        let mut roster_changed = false;
        while let Some(event) = gilrs.next_event() {
            match event.event {
                EventType::ButtonPressed(button, _) => {
                    if let Some(action) = action_for(button) {
                        actions.push(action);
                    }
                }
                EventType::Connected | EventType::Disconnected => roster_changed = true,
                _ => {}
            }
        }
        if roster_changed {
            self.refresh_name();
        }
        actions
    }

    /// Samples the held state of the sticks and D-pad. Only the first
    /// connected pad drives the map.
    pub fn sample(&self) -> Sticks {
        let Some(gilrs) = self.gilrs.as_ref() else {
            return Sticks::default();
        };
        let Some((_, pad)) = gilrs.gamepads().next() else {
            return Sticks::default();
        };

        // Stick y axes report up as positive; screen coordinates run the other
        // way, and the pan below is expressed in screen terms.
        let pan = (
            shape(pad.value(Axis::LeftStickX)),
            -shape(pad.value(Axis::LeftStickY)),
        );

        // The D-pad comes through as buttons on some pads and as an axis pair on
        // others, so read both. Up and right zoom in, down and left zoom out —
        // whichever direction gets pressed does something sensible.
        let mut zoom = shape(pad.value(Axis::DPadY)) + shape(pad.value(Axis::DPadX));
        if pad.is_pressed(Button::DPadUp) || pad.is_pressed(Button::DPadRight) {
            zoom += 1.0;
        }
        if pad.is_pressed(Button::DPadDown) || pad.is_pressed(Button::DPadLeft) {
            zoom -= 1.0;
        }

        Sticks {
            pan,
            turn: shape(pad.value(Axis::RightStickX)),
            zoom: zoom.clamp(-1.0, 1.0),
        }
    }

    fn refresh_name(&mut self) {
        self.name = self.gilrs.as_ref().and_then(|gilrs| {
            gilrs
                .gamepads()
                .next()
                .map(|(_, pad)| pad.name().to_owned())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_buttons_map_to_actions() {
        assert_eq!(action_for(Button::Start), Some(Action::Play));
        assert_eq!(action_for(Button::Select), Some(Action::Stop));
        assert_eq!(action_for(Button::LeftTrigger), Some(Action::PreviousPlace));
        assert_eq!(action_for(Button::RightTrigger), Some(Action::NextPlace));
        assert_eq!(action_for(Button::South), Some(Action::Drop));
    }

    #[test]
    fn the_deadzone_swallows_a_resting_stick_and_rescales_the_rest() {
        assert_eq!(shape(0.0), 0.0);
        assert_eq!(shape(0.1), 0.0);
        assert_eq!(shape(-0.1), 0.0);
        // Just past the deadzone control starts from zero, not from 0.15.
        assert!(shape(DEADZONE + 0.001) < 0.01);
        assert_eq!(shape(1.0), 1.0);
        assert_eq!(shape(-1.0), -1.0);
        // Values beyond the nominal range are clamped, not amplified.
        assert_eq!(shape(1.4), 1.0);
    }

    #[test]
    fn a_centred_pad_is_not_active() {
        assert!(!Sticks::default().active());
        assert!(
            Sticks {
                turn: 0.5,
                ..Sticks::default()
            }
            .active()
        );
        assert!(
            Sticks {
                pan: (0.0, -0.3),
                ..Sticks::default()
            }
            .active()
        );
    }

    #[test]
    fn other_buttons_are_ignored() {
        // L2/R2 are deliberately not L1/R1, and the D-pad is read as held
        // state by `sample` rather than as discrete presses.
        for button in [
            Button::LeftTrigger2,
            Button::RightTrigger2,
            Button::North,
            Button::East,
            Button::West,
            Button::Mode,
            Button::DPadUp,
            Button::Unknown,
        ] {
            assert_eq!(action_for(button), None, "{button:?}");
        }
    }
}

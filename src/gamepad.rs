//! Gamepad control.
//!
//! gilrs is used rather than raw HID because it carries the
//! SDL_GameControllerDB mappings: `Button::Start` is the Start button on
//! whatever pad is plugged in, instead of an index that only lines up on
//! XInput-style controllers.
//!
//! The layout below is an Xbox controller's. gilrs names the action pad by
//! compass point, so A is `South` and B is `East`; on a pad whose face buttons
//! sit elsewhere — a Nintendo layout swaps A/B and X/Y — the same physical
//! positions still fire, but the printed letters will not match.

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
    /// A — the drop effect.
    Drop,
    /// B — the orbit effect.
    Orbit,
    /// D-pad left — previous track.
    PreviousTrack,
    /// D-pad right — next track.
    NextTrack,
    /// D-pad up — previous release.
    PreviousRelease,
    /// D-pad down — next release.
    NextRelease,
}

fn action_for(button: Button) -> Option<Action> {
    match button {
        Button::Start => Some(Action::Play),
        Button::Select => Some(Action::Stop),
        // `LeftTrigger` is the L1 / LB bumper; `LeftTrigger2` is L2 / LT, which
        // works the volume instead.
        Button::LeftTrigger => Some(Action::PreviousPlace),
        Button::RightTrigger => Some(Action::NextPlace),
        Button::South => Some(Action::Drop),
        Button::East => Some(Action::Orbit),
        Button::DPadLeft => Some(Action::PreviousTrack),
        Button::DPadRight => Some(Action::NextTrack),
        Button::DPadUp => Some(Action::PreviousRelease),
        Button::DPadDown => Some(Action::NextRelease),
        _ => None,
    }
}

/// Continuous stick and trigger state, sampled once per frame. Each field is
/// in `-1.0..=1.0`.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Sticks {
    /// Left stick: pans the map. Positive y is down the screen.
    pub pan: (f32, f32),
    /// Right stick, left/right: turns the map.
    pub turn: f32,
    /// Right stick, up/down: changes the zoom level. Positive zooms in.
    pub zoom: f32,
    /// R2 minus L2: raises or lowers the volume.
    pub volume: f32,
}

impl Sticks {
    /// Whether anything that moves the map is being held, so a fly-to knows to
    /// give way. Volume is not a camera control, so it does not count.
    pub fn moves_map(&self) -> bool {
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

        // Triggers are analog, and gilrs surfaces them either as a button
        // carrying a value or as an axis, depending on the pad. The button form
        // already reads 0..1; the axis form rests at -1 and reads +1 fully
        // pressed, so it is rescaled. A resting trigger must read 0 — reading
        // it as pressed would run the volume away on its own.
        let trigger = |button: Button, axis: Axis| match pad.button_data(button) {
            Some(data) => data.value(),
            None => (pad.value(axis) + 1.0) / 2.0,
        };
        let left = shape(trigger(Button::LeftTrigger2, Axis::LeftZ));
        let right = shape(trigger(Button::RightTrigger2, Axis::RightZ));

        Sticks {
            pan,
            turn: shape(pad.value(Axis::RightStickX)),
            zoom: shape(pad.value(Axis::RightStickY)),
            volume: right - left,
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
        assert_eq!(action_for(Button::East), Some(Action::Orbit));
        assert_eq!(action_for(Button::DPadLeft), Some(Action::PreviousTrack));
        assert_eq!(action_for(Button::DPadRight), Some(Action::NextTrack));
        assert_eq!(action_for(Button::DPadUp), Some(Action::PreviousRelease));
        assert_eq!(action_for(Button::DPadDown), Some(Action::NextRelease));
    }

    #[test]
    fn a_resting_trigger_reads_as_untouched() {
        // The axis form rests at -1, which must map to 0 and then fall inside
        // the deadzone; halfway is a half press.
        assert_eq!(shape((-1.0 + 1.0) / 2.0), 0.0);
        assert!(shape((0.0 + 1.0) / 2.0) > 0.4);
        assert_eq!(shape((1.0 + 1.0) / 2.0), 1.0);
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
    fn only_the_camera_controls_count_as_moving_the_map() {
        assert!(!Sticks::default().moves_map());
        assert!(
            Sticks {
                turn: 0.5,
                ..Sticks::default()
            }
            .moves_map()
        );
        assert!(
            Sticks {
                pan: (0.0, -0.3),
                ..Sticks::default()
            }
            .moves_map()
        );
        assert!(
            Sticks {
                zoom: 0.4,
                ..Sticks::default()
            }
            .moves_map()
        );
        // Reaching for the volume must not cancel a fly-to.
        assert!(
            !Sticks {
                volume: 1.0,
                ..Sticks::default()
            }
            .moves_map()
        );
    }

    #[test]
    fn other_buttons_are_ignored() {
        // L2/R2 work the volume as analog axes, not as presses.
        for button in [
            Button::LeftTrigger2,
            Button::RightTrigger2,
            Button::North,
            Button::West,
            Button::Mode,
            Button::LeftThumb,
            Button::RightThumb,
            Button::Unknown,
        ] {
            assert_eq!(action_for(button), None, "{button:?}");
        }
    }
}

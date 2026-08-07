use macroquad::prelude::*;
use gilrs::{Gilrs, Button, Axis};
use crate::types::UIFocus; // Assuming UIFocus is in types.rs

/// Which device the user touched last — drives button-glyph selection.
#[derive(Clone, Copy, PartialEq)]
pub enum InputSource {
    Keyboard,
    Pad,
}

pub struct InputState {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub select: bool,
    pub next: bool,
    pub prev: bool,
    pub cycle: bool,
    pub back: bool,
    pub secondary: bool,
    pub tertiary: bool, // North button (Y/Triangle/C-Up) / keyboard E
    pub analog_was_neutral: bool,
    pub ui_focus: UIFocus,
    pub last_source: InputSource,
    // Identity of the most recently used pad, for brand-matched glyphs.
    pub pad_vendor: Option<u16>,
    pub pad_name: String,
}

impl InputState {
    const ANALOG_DEADZONE: f32 = 0.5;  // Increased deadzone for less sensitivity

    /// Any press this frame — feeds the screensaver's idle clock.
    pub fn any_activity(&self) -> bool {
        self.up || self.down || self.left || self.right || self.select || self.next
            || self.prev || self.cycle || self.back || self.secondary || self.tertiary
    }

    pub fn new() -> Self {
        InputState {
            up: false,
            down: false,
            left: false,
            right: false,
            select: false,
            next: false,
            prev: false,
            cycle: false,
            back: false,
            secondary: false,
            tertiary: false,
            analog_was_neutral: true,
            ui_focus: UIFocus::Grid,
            last_source: InputSource::Keyboard,
            pad_vendor: None,
            pad_name: String::new(),
        }
    }

    pub fn reset(&mut self) {
        self.up = false;
        self.down = false;
        self.left = false;
        self.right = false;
        self.select = false;
        self.next = false;
        self.prev = false;
        self.cycle = false;
        self.back = false;
        self.secondary = false;
        self.tertiary = false;
        // Note: We do NOT reset analog_was_neutral or ui_focus
    }

    pub fn update_keyboard(&mut self) {
        self.up = is_key_pressed(KeyCode::Up);
        self.down = is_key_pressed(KeyCode::Down);
        self.left = is_key_pressed(KeyCode::Left);
        self.right = is_key_pressed(KeyCode::Right);
        self.select = is_key_pressed(KeyCode::Enter);
        self.next = is_key_pressed(KeyCode::RightBracket);
        self.prev = is_key_pressed(KeyCode::LeftBracket);
        self.back = is_key_pressed(KeyCode::Backspace);
        self.secondary = is_key_pressed(KeyCode::X);
        self.tertiary = is_key_pressed(KeyCode::E);
        self.cycle = is_key_pressed(KeyCode::Tab);
        // Runs before update_controller each frame, so the flags here are
        // purely keyboard-derived.
        if self.up || self.down || self.left || self.right || self.select
            || self.next || self.prev || self.back || self.secondary
            || self.tertiary || self.cycle
        {
            self.last_source = InputSource::Keyboard;
        }
    }

    fn note_pad(&mut self, gilrs: &Gilrs, id: gilrs::GamepadId) {
        self.last_source = InputSource::Pad;
        let pad = gilrs.gamepad(id);
        self.pad_vendor = pad.vendor_id();
        if self.pad_name != pad.name() {
            self.pad_name = pad.name().to_string();
        }
    }

    pub fn update_controller(&mut self, gilrs: &mut Gilrs) {
        // Handle button events
        while let Some(ev) = gilrs.next_event() {
            if let gilrs::EventType::ButtonPressed(..) = ev.event {
                self.note_pad(gilrs, ev.id);
            }
            match ev.event {
                gilrs::EventType::ButtonPressed(Button::DPadUp, _) => self.up = true,
                gilrs::EventType::ButtonPressed(Button::DPadDown, _) => self.down = true,
                gilrs::EventType::ButtonPressed(Button::DPadLeft, _) => self.left = true,
                gilrs::EventType::ButtonPressed(Button::DPadRight, _) => self.right = true,
                gilrs::EventType::ButtonPressed(Button::South, _) => self.select = true,
                gilrs::EventType::ButtonPressed(Button::East, _) => self.back = true,
                gilrs::EventType::ButtonPressed(Button::West, _) => self.secondary = true,
                gilrs::EventType::ButtonPressed(Button::North, _) => self.tertiary = true,
                gilrs::EventType::ButtonPressed(Button::RightTrigger, _) => self.next = true,
                gilrs::EventType::ButtonPressed(Button::LeftTrigger, _) => self.prev = true,
                _ => {}
            }
        }

        // --- Handle analog stick input (New, correct logic) ---

        let mut any_stick_active = false;
        let was_neutral = self.analog_was_neutral;

        // Iterate through all gamepads to find the first active one
        for (_, gamepad) in gilrs.gamepads() {
            let raw_x = gamepad.value(Axis::LeftStickX);
            let raw_y = gamepad.value(Axis::LeftStickY);

            let is_currently_neutral = raw_x.abs() < Self::ANALOG_DEADZONE &&
            raw_y.abs() < Self::ANALOG_DEADZONE;

            // Is this stick active?
            if !is_currently_neutral {
                // Yes. This is the only stick we care about.
                any_stick_active = true;

                // Was the system neutral before this frame?
                if was_neutral {
                    self.last_source = InputSource::Pad;
                    self.pad_vendor = gamepad.vendor_id();
                    if self.pad_name != gamepad.name() {
                        self.pad_name = gamepad.name().to_string();
                    }
                    // Yes. This is a "just pushed" event. Fire it.
                    // Prioritize dominant axis
                    if raw_y.abs() > raw_x.abs() {
                        // Vertical is stronger
                        if raw_y > -Self::ANALOG_DEADZONE {       // -Y is UP
                            self.up = true;
                        } else if raw_y < Self::ANALOG_DEADZONE { // +Y is DOWN
                            self.down = true;
                        }
                    } else {
                        // Horizontal is stronger
                        if raw_x < -Self::ANALOG_DEADZONE {       // -X is LEFT
                            self.left = true;
                        } else if raw_x > Self::ANALOG_DEADZONE { // +X is RIGHT
                            self.right = true;
                        }
                    }
                }

                // We found our active stick. Stop processing other gamepads
                // to prevent them from interfering.
                break;
            }
            // If the stick is neutral, we ignore it and check the next one.
        }

        // Update the global neutral state.
        // If we found an active stick, the system is "non-neutral".
        // If the loop finished and found no active sticks, all are neutral.
        self.analog_was_neutral = !any_stick_active;
    }
}

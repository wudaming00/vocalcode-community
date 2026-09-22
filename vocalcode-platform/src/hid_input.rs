//! Platform-neutral decoding for native HID value callbacks.
//!
//! The macOS IOHID backend supplies one parsed element value at a time.  Keep
//! the descriptor-to-trigger mapping here so its state transitions are covered
//! by the normal workspace tests even when those tests run on Windows.

use std::collections::{HashMap, HashSet};

use vocalcode_core::config::GamepadButton;

use crate::hotkey::NativeControl;

pub(crate) const HID_PAGE_GENERIC_DESKTOP: u16 = 0x01;
pub(crate) const HID_PAGE_KEYBOARD: u16 = 0x07;
pub(crate) const HID_PAGE_BUTTON: u16 = 0x09;
pub(crate) const HID_PAGE_CONSUMER: u16 = 0x0c;
pub(crate) const HID_PAGE_VENDOR_MIN: u16 = 0xff00;

const HID_USAGE_MOUSE: u16 = 0x02;
const HID_USAGE_JOYSTICK: u16 = 0x04;
const HID_USAGE_GAMEPAD: u16 = 0x05;
const HID_USAGE_KEYBOARD: u16 = 0x06;
const HID_USAGE_MULTI_AXIS: u16 = 0x08;
const HID_USAGE_HAT_SWITCH: u16 = 0x39;
const HID_USAGE_DPAD_UP: u16 = 0x90;
const HID_USAGE_DPAD_DOWN: u16 = 0x91;
const HID_USAGE_DPAD_RIGHT: u16 = 0x92;
const HID_USAGE_DPAD_LEFT: u16 = 0x93;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HidDeviceClass {
    Gamepad,
    Keyboard,
    Mouse,
    Other,
}

impl HidDeviceClass {
    pub(crate) fn from_primary_usage(usage_page: u16, usage: u16) -> Self {
        match (usage_page, usage) {
            (
                HID_PAGE_GENERIC_DESKTOP,
                HID_USAGE_JOYSTICK | HID_USAGE_GAMEPAD | HID_USAGE_MULTI_AXIS,
            ) => Self::Gamepad,
            (HID_PAGE_GENERIC_DESKTOP, HID_USAGE_KEYBOARD) => Self::Keyboard,
            (HID_PAGE_GENERIC_DESKTOP, HID_USAGE_MOUSE) => Self::Mouse,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HidElementSample {
    pub usage_page: u16,
    pub usage: u16,
    pub cookie: u32,
    pub value: i64,
    pub logical_min: i64,
    pub logical_max: i64,
    /// Relative elements describe pulses/deltas, not a value that remains held.
    pub relative: bool,
}

/// Per-device element state. IOHID may repeat identical values and hat switches
/// can represent two simultaneous directions, so diff complete control sets
/// rather than treating every callback as a fresh press.
#[derive(Default)]
pub(crate) struct HidValueDecoder {
    active_by_element: HashMap<u32, HashSet<NativeControl>>,
}

impl HidValueDecoder {
    pub(crate) fn decode(
        &mut self,
        class: HidDeviceClass,
        sample: HidElementSample,
    ) -> Vec<(NativeControl, bool)> {
        let next = controls_for_sample(class, sample);

        // Relative controls are momentary pulses. Synthesize a complete edge so
        // repeated wheel-like buttons can fire again even though they never
        // publish an absolute zero/release report.
        if sample.relative {
            return next
                .into_iter()
                .flat_map(|control| [(control, true), (control, false)])
                .collect();
        }

        let previous = self.active_by_element.entry(sample.cookie).or_default();
        let mut edges = Vec::with_capacity(previous.len() + next.len());
        edges.extend(
            previous
                .difference(&next)
                .copied()
                .map(|control| (control, false)),
        );
        edges.extend(
            next.difference(previous)
                .copied()
                .map(|control| (control, true)),
        );
        *previous = next;
        edges
    }
}

fn controls_for_sample(class: HidDeviceClass, sample: HidElementSample) -> HashSet<NativeControl> {
    // Keyboards are already handled (and suppressible) through CGEventTap.
    // Listening to their page here would dispatch every ordinary key twice.
    if sample.usage_page == HID_PAGE_KEYBOARD {
        return HashSet::new();
    }

    if class == HidDeviceClass::Gamepad && sample.usage_page == HID_PAGE_GENERIC_DESKTOP {
        if sample.usage == HID_USAGE_HAT_SWITCH {
            return hat_buttons(sample)
                .into_iter()
                .map(NativeControl::Gamepad)
                .collect();
        }
        if let Some(button) = generic_desktop_dpad(sample.usage) {
            return (sample.value != 0)
                .then_some(NativeControl::Gamepad(button))
                .into_iter()
                .collect();
        }
    }

    if sample.usage_page == HID_PAGE_CONSUMER {
        // Browser back/forward on a mouse commonly has both a Consumer element
        // and an OtherMouse event. Keep the suppressible event-tap copy only.
        if class == HidDeviceClass::Mouse {
            return HashSet::new();
        }
        let usage = consumer_usage(sample);
        return usage
            .filter(|_| sample.value != 0)
            .map(NativeControl::Consumer)
            .into_iter()
            .collect();
    }

    if sample.usage_page == HID_PAGE_BUTTON {
        // Mouse buttons already arrive through OtherMouseDown/Up. A keyboard's
        // auxiliary button elements are likewise left to the event tap.
        if matches!(class, HidDeviceClass::Mouse | HidDeviceClass::Keyboard) {
            return HashSet::new();
        }
        if sample.value == 0 {
            return HashSet::new();
        }
        if class == HidDeviceClass::Gamepad {
            if let Some(button) = conventional_gamepad_button(sample.usage) {
                return HashSet::from([NativeControl::Gamepad(button)]);
            }
        }
        return HashSet::from([generic_control(sample)]);
    }

    if sample.usage_page >= HID_PAGE_VENDOR_MIN && sample.value != 0 {
        return HashSet::from([generic_control(sample)]);
    }

    HashSet::new()
}

fn generic_desktop_dpad(usage: u16) -> Option<GamepadButton> {
    Some(match usage {
        HID_USAGE_DPAD_UP => GamepadButton::DpadUp,
        HID_USAGE_DPAD_DOWN => GamepadButton::DpadDown,
        HID_USAGE_DPAD_RIGHT => GamepadButton::DpadRight,
        HID_USAGE_DPAD_LEFT => GamepadButton::DpadLeft,
        _ => return None,
    })
}

fn generic_control(sample: HidElementSample) -> NativeControl {
    // `control = 0` is the config wildcard, so never persist a zero cookie.
    let control = sample.cookie.checked_add(1).unwrap_or(sample.cookie).max(1);
    NativeControl::Hid {
        usage_page: sample.usage_page,
        usage: sample.usage,
        control,
    }
}

/// Consumer arrays sometimes expose the selected usage as the value of a
/// collection element whose own usage is 0/1. Variable elements already carry
/// their real semantic usage and merely toggle between zero and one.
fn consumer_usage(sample: HidElementSample) -> Option<u16> {
    if sample.usage <= 1 && sample.value > 1 {
        u16::try_from(sample.value).ok()
    } else if sample.usage != 0 {
        Some(sample.usage)
    } else {
        None
    }
}

/// Common HID/DirectInput button ordering. Usages 7 and 8 are usually analog
/// triggers, so they stay generic rather than being mislabeled as shoulders.
fn conventional_gamepad_button(usage: u16) -> Option<GamepadButton> {
    Some(match usage {
        1 => GamepadButton::South,
        2 => GamepadButton::East,
        3 => GamepadButton::West,
        4 => GamepadButton::North,
        5 => GamepadButton::LeftShoulder,
        6 => GamepadButton::RightShoulder,
        9 => GamepadButton::Back,
        10 => GamepadButton::Start,
        11 => GamepadButton::LeftThumb,
        12 => GamepadButton::RightThumb,
        13 => GamepadButton::DpadUp,
        14 => GamepadButton::DpadDown,
        15 => GamepadButton::DpadLeft,
        16 => GamepadButton::DpadRight,
        _ => return None,
    })
}

fn hat_buttons(sample: HidElementSample) -> HashSet<GamepadButton> {
    if sample.logical_max.saturating_sub(sample.logical_min) < 7
        || sample.value < sample.logical_min
        || sample.value > sample.logical_min.saturating_add(7)
    {
        return HashSet::new();
    }

    let direction = sample.value - sample.logical_min;
    let mut buttons = HashSet::new();
    if matches!(direction, 0 | 1 | 7) {
        buttons.insert(GamepadButton::DpadUp);
    }
    if matches!(direction, 1..=3) {
        buttons.insert(GamepadButton::DpadRight);
    }
    if matches!(direction, 3..=5) {
        buttons.insert(GamepadButton::DpadDown);
    }
    if matches!(direction, 5..=7) {
        buttons.insert(GamepadButton::DpadLeft);
    }
    buttons
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(usage_page: u16, usage: u16, value: i64) -> HidElementSample {
        HidElementSample {
            usage_page,
            usage,
            cookie: 41,
            value,
            logical_min: 0,
            logical_max: 1,
            relative: false,
        }
    }

    #[test]
    fn keyboard_and_mouse_buttons_are_not_duplicated() {
        let mut decoder = HidValueDecoder::default();
        assert!(decoder
            .decode(HidDeviceClass::Keyboard, sample(HID_PAGE_KEYBOARD, 0x04, 1),)
            .is_empty());
        assert!(decoder
            .decode(HidDeviceClass::Mouse, sample(HID_PAGE_BUTTON, 4, 1),)
            .is_empty());
        assert!(decoder
            .decode(HidDeviceClass::Mouse, sample(HID_PAGE_CONSUMER, 0x0224, 1),)
            .is_empty());
    }

    #[test]
    fn primary_usages_classify_devices_without_guessing_from_product_names() {
        assert_eq!(
            HidDeviceClass::from_primary_usage(HID_PAGE_GENERIC_DESKTOP, HID_USAGE_GAMEPAD),
            HidDeviceClass::Gamepad
        );
        assert_eq!(
            HidDeviceClass::from_primary_usage(HID_PAGE_GENERIC_DESKTOP, HID_USAGE_JOYSTICK),
            HidDeviceClass::Gamepad
        );
        assert_eq!(
            HidDeviceClass::from_primary_usage(HID_PAGE_GENERIC_DESKTOP, HID_USAGE_MULTI_AXIS),
            HidDeviceClass::Gamepad
        );
        assert_eq!(
            HidDeviceClass::from_primary_usage(HID_PAGE_GENERIC_DESKTOP, HID_USAGE_KEYBOARD),
            HidDeviceClass::Keyboard
        );
        assert_eq!(
            HidDeviceClass::from_primary_usage(HID_PAGE_GENERIC_DESKTOP, HID_USAGE_MOUSE),
            HidDeviceClass::Mouse
        );
    }

    #[test]
    fn gamepad_face_button_has_press_release_edges_and_deduplicates() {
        let mut decoder = HidValueDecoder::default();
        let down = NativeControl::Gamepad(GamepadButton::South);
        assert_eq!(
            decoder.decode(HidDeviceClass::Gamepad, sample(HID_PAGE_BUTTON, 1, 1)),
            vec![(down, true)]
        );
        assert!(decoder
            .decode(HidDeviceClass::Gamepad, sample(HID_PAGE_BUTTON, 1, 1),)
            .is_empty());
        assert_eq!(
            decoder.decode(HidDeviceClass::Gamepad, sample(HID_PAGE_BUTTON, 1, 0)),
            vec![(down, false)]
        );
    }

    #[test]
    fn hat_diagonal_diffs_into_cardinal_edges() {
        let mut decoder = HidValueDecoder::default();
        let mut north_east = sample(HID_PAGE_GENERIC_DESKTOP, HID_USAGE_HAT_SWITCH, 1);
        north_east.logical_max = 7;
        let edges = decoder.decode(HidDeviceClass::Gamepad, north_east);
        assert_eq!(edges.len(), 2);
        assert!(edges.contains(&(NativeControl::Gamepad(GamepadButton::DpadUp), true)));
        assert!(edges.contains(&(NativeControl::Gamepad(GamepadButton::DpadRight), true)));

        let mut east = north_east;
        east.value = 2;
        assert_eq!(
            decoder.decode(HidDeviceClass::Gamepad, east),
            vec![(NativeControl::Gamepad(GamepadButton::DpadUp), false)]
        );

        let mut released = east;
        released.value = 8; // standard HID hat null state
        assert_eq!(
            decoder.decode(HidDeviceClass::Gamepad, released),
            vec![(NativeControl::Gamepad(GamepadButton::DpadRight), false)]
        );
    }

    #[test]
    fn individual_generic_desktop_dpad_usage_is_supported() {
        let mut decoder = HidValueDecoder::default();
        let right = NativeControl::Gamepad(GamepadButton::DpadRight);
        assert_eq!(
            decoder.decode(
                HidDeviceClass::Gamepad,
                sample(HID_PAGE_GENERIC_DESKTOP, HID_USAGE_DPAD_RIGHT, 1),
            ),
            vec![(right, true)]
        );
        assert_eq!(
            decoder.decode(
                HidDeviceClass::Gamepad,
                sample(HID_PAGE_GENERIC_DESKTOP, HID_USAGE_DPAD_RIGHT, 0),
            ),
            vec![(right, false)]
        );
    }

    #[test]
    fn consumer_array_switch_releases_old_usage_before_pressing_new_one() {
        let mut decoder = HidValueDecoder::default();
        let play = sample(HID_PAGE_CONSUMER, 1, 0x00cd);
        assert_eq!(
            decoder.decode(HidDeviceClass::Other, play),
            vec![(NativeControl::Consumer(0x00cd), true)]
        );
        let next = sample(HID_PAGE_CONSUMER, 1, 0x00b5);
        assert_eq!(
            decoder.decode(HidDeviceClass::Other, next),
            vec![
                (NativeControl::Consumer(0x00cd), false),
                (NativeControl::Consumer(0x00b5), true),
            ]
        );
    }

    #[test]
    fn relative_vendor_pulse_is_a_complete_repeatable_edge() {
        let mut decoder = HidValueDecoder::default();
        let mut pulse = sample(HID_PAGE_VENDOR_MIN, 7, 1);
        pulse.relative = true;
        let control = generic_control(pulse);
        let expected = vec![(control, true), (control, false)];
        assert_eq!(decoder.decode(HidDeviceClass::Other, pulse), expected);
        assert_eq!(decoder.decode(HidDeviceClass::Other, pulse), expected);
    }

    #[test]
    fn vendor_button_cookie_never_becomes_config_wildcard() {
        let mut zero_cookie = sample(HID_PAGE_VENDOR_MIN, 7, 1);
        zero_cookie.cookie = 0;
        assert!(matches!(
            generic_control(zero_cookie),
            NativeControl::Hid { control: 1, .. }
        ));
    }
}

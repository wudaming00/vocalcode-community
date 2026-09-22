extern crate winapi;

mod common;
mod display;
#[cfg(feature = "unstable_grab")]
mod grab;
mod keyboard;
mod keycodes;
mod listen;
mod simulate;

pub use crate::windows::display::display_size;
#[cfg(feature = "unstable_grab")]
pub use crate::windows::grab::grab;
pub use crate::windows::keyboard::Keyboard;
// VocalCode patch: expose the VK<->Key table so a caller can run its own
// WH_KEYBOARD_LL and still speak this crate's `Key` vocabulary (persisted
// bindings are stored as these names).
pub use crate::windows::keycodes::{code_from_key, key_from_code};
pub use crate::windows::listen::listen;
pub use crate::windows::simulate::simulate;

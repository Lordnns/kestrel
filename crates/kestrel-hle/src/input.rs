//! Controller input from the host: what a frontend (a window with keyboard
//! and gamepads) or a test script tells hid, which writes it into its shared
//! memory for the program (`service::hid`).
//!
//! kestrel emulates one controller (the owner's choice for the first input
//! task): a Pro Controller as player 1, or the two Joy-Con attached to the
//! console (handheld).

/// Buttons, as the bits of Switchbrew's NpadButton ("HID services").
pub mod button {
    pub const A: u64 = 1 << 0;
    pub const B: u64 = 1 << 1;
    pub const X: u64 = 1 << 2;
    pub const Y: u64 = 1 << 3;
    pub const STICK_L: u64 = 1 << 4;
    pub const STICK_R: u64 = 1 << 5;
    pub const L: u64 = 1 << 6;
    pub const R: u64 = 1 << 7;
    pub const ZL: u64 = 1 << 8;
    pub const ZR: u64 = 1 << 9;
    pub const PLUS: u64 = 1 << 10;
    pub const MINUS: u64 = 1 << 11;
    pub const LEFT: u64 = 1 << 12;
    pub const UP: u64 = 1 << 13;
    pub const RIGHT: u64 = 1 << 14;
    pub const DOWN: u64 = 1 << 15;
    /// StickLLeft, StickLUp, StickLRight, StickLDown, then the same for the
    /// right stick: hid sets them from the stick positions.
    pub const STICK_L_LEFT: u64 = 1 << 16;
    pub const STICK_R_LEFT: u64 = 1 << 20;
    /// The buttons a frontend sets itself (the stick directions are hid's).
    pub const PRESSABLE: u64 = 0xffff;

    /// The buttons by name, as in the settings file and Switchbrew.
    pub const NAMES: [(&str, u64); 16] = [
        ("A", A),
        ("B", B),
        ("X", X),
        ("Y", Y),
        ("StickL", STICK_L),
        ("StickR", STICK_R),
        ("L", L),
        ("R", R),
        ("ZL", ZL),
        ("ZR", ZR),
        ("Plus", PLUS),
        ("Minus", MINUS),
        ("Left", LEFT),
        ("Up", UP),
        ("Right", RIGHT),
        ("Down", DOWN),
    ];
}

/// A stick's full deflection (libnx, JOYSTICK_MAX); -STICK_MAX is the other
/// way. Up is positive Y.
pub const STICK_MAX: i32 = 0x7fff;

/// One controller's state: the buttons pressed (bits of `button`, from
/// `button::PRESSABLE`) and the two sticks as (x, y).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    pub buttons: u64,
    pub left: (i32, i32),
    pub right: (i32, i32),
}

/// The controller kestrel emulates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Controller {
    /// A Pro Controller as player 1 (NpadIdType No1, style FullKey).
    #[default]
    ProController,
    /// The two Joy-Con attached to the console (NpadIdType Handheld, style
    /// Handheld).
    Handheld,
}

/// Where hid gets the controller's state each time it samples it. `sample`
/// is hid's sample count, from 0, so a script can depend on it.
pub trait Source: Send {
    fn controller(&self) -> Controller;
    fn sample(&mut self, sample: u64) -> Pad;
}

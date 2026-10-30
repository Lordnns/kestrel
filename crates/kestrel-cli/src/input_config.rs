//! The input settings file, `user/config/input.cfg`: which controller kestrel
//! emulates and which keys and gamepad buttons drive it (the owner's
//! choice: one controller, mapped from the keyboard and any gamepad). It is
//! written with the defaults below when missing, then the player edits it.

use kestrel_hle::input::{button, Controller, Pad, STICK_MAX};
use std::collections::{HashMap, HashSet};

/// The file written when there is none: every setting, with its defaults
/// and how to change it.
pub const DEFAULT: &str = "\
# kestrel input settings: \"name = value\" lines; \"#\" starts a comment.
# Keys are winit KeyCode names: KeyA to KeyZ, Digit0 to Digit9, ArrowUp,
# ArrowDown, ArrowLeft, ArrowRight, Enter, Backspace, Space, Tab, ShiftLeft,
# ShiftRight, ControlLeft, ControlRight, ...
# Gamepad buttons are gilrs names: South, East, North, West, LeftTrigger,
# LeftTrigger2, RightTrigger, RightTrigger2, Select, Start, LeftThumb,
# RightThumb, DPadUp, DPadDown, DPadLeft, DPadRight.

# The emulated controller: pro (a Pro Controller as player 1) or handheld
# (the Joy-Con attached to the console).
controller = pro

# Buttons: keys (key:) and gamepad buttons (pad:), separated by commas.
# Gamepad buttons go by position, as on a Switch controller: A is the
# right face button (East), B the bottom one (South).
A = key:KeyX, pad:East
B = key:KeyZ, pad:South
X = key:KeyC, pad:North
Y = key:KeyV, pad:West
L = key:KeyQ, pad:LeftTrigger
R = key:KeyE, pad:RightTrigger
ZL = key:Digit1, pad:LeftTrigger2
ZR = key:Digit3, pad:RightTrigger2
Plus = key:Enter, pad:Start
Minus = key:Backspace, pad:Select
StickL = key:KeyF, pad:LeftThumb
StickR = key:KeyH, pad:RightThumb
Up = key:ArrowUp, pad:DPadUp
Down = key:ArrowDown, pad:DPadDown
Left = key:ArrowLeft, pad:DPadLeft
Right = key:ArrowRight, pad:DPadRight

# Sticks: a gamepad stick (pad:LeftStick or pad:RightStick) moves one
# freely; keys push it all the way one way.
LeftStick = pad:LeftStick
LeftStickUp = key:KeyW
LeftStickLeft = key:KeyA
LeftStickDown = key:KeyS
LeftStickRight = key:KeyD
RightStick = pad:RightStick
RightStickUp = key:KeyI
RightStickLeft = key:KeyJ
RightStickDown = key:KeyK
RightStickRight = key:KeyL
";

/// One host input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    Key(String),
    Pad(String),
}

/// The settings, read.
#[derive(Clone, Debug, PartialEq)]
pub struct InputConfig {
    pub controller: Controller,
    /// Each console button and what presses it.
    pub buttons: Vec<(u64, Vec<Binding>)>,
    /// For the left and right stick: the gamepad stick that moves it
    /// ("LeftStick", "RightStick"), and the keys that push it left, up,
    /// right and down.
    pub sticks: [(Option<String>, [Vec<String>; 4]); 2],
}

const DIRECTIONS: [&str; 4] = ["Left", "Up", "Right", "Down"];

fn bindings(value: &str, line: usize) -> Result<Vec<Binding>, String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| match v.split_once(':') {
            Some(("key", k)) if !k.is_empty() => Ok(Binding::Key(k.to_string())),
            Some(("pad", b)) if !b.is_empty() => Ok(Binding::Pad(b.to_string())),
            _ => Err(format!(
                "line {line}: \"{v}\" is not key:<name> or pad:<name>"
            )),
        })
        .collect()
}

impl InputConfig {
    /// Read the settings; a line kestrel cannot read is an error naming it.
    /// Settings left out keep no binding.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut c = InputConfig {
            controller: Controller::ProController,
            buttons: Vec::new(),
            sticks: Default::default(),
        };
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let content = raw.split('#').next().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let Some((name, value)) = content.split_once('=') else {
                return Err(format!("line {line}: expected \"name = value\""));
            };
            let (name, value) = (name.trim(), value.trim());
            if name == "controller" {
                c.controller = match value {
                    "pro" => Controller::ProController,
                    "handheld" => Controller::Handheld,
                    _ => return Err(format!("line {line}: controller is pro or handheld")),
                };
                continue;
            }
            if let Some(&(_, bit)) = button::NAMES.iter().find(|(n, _)| *n == name) {
                c.buttons.push((bit, bindings(value, line)?));
                continue;
            }
            let stick = [("LeftStick", 0), ("RightStick", 1)]
                .into_iter()
                .find_map(|(prefix, i)| name.strip_prefix(prefix).map(|rest| (i, rest)));
            match stick {
                Some((i, "")) => {
                    c.sticks[i].0 = match bindings(value, line)?.as_slice() {
                        [] => None,
                        [Binding::Pad(p)] if p == "LeftStick" || p == "RightStick" => {
                            Some(p.clone())
                        }
                        _ => {
                            return Err(format!(
                                "line {line}: {name} is pad:LeftStick or pad:RightStick"
                            ))
                        }
                    };
                }
                Some((i, rest)) if DIRECTIONS.contains(&rest) => {
                    let d = DIRECTIONS.iter().position(|&x| x == rest).unwrap();
                    let mut keys = Vec::new();
                    for b in bindings(value, line)? {
                        match b {
                            Binding::Key(k) => keys.push(k),
                            Binding::Pad(_) => {
                                return Err(format!("line {line}: {name} takes keys only"))
                            }
                        }
                    }
                    c.sticks[i].1[d] = keys;
                }
                _ => return Err(format!("line {line}: unknown setting \"{name}\"")),
            }
        }
        Ok(c)
    }

    /// The controller's state from what the host has down: keys and gamepad
    /// buttons by name, and gamepad axes by gilrs name ("LeftStickX", ...)
    /// from -1.0 to 1.0, up positive. Keys win over a gamepad stick; a
    /// gamepad stick within 10% of centre counts as centred (kestrel's
    /// choice of dead zone).
    pub fn pad(
        &self,
        keys: &HashSet<String>,
        pad_buttons: &HashSet<String>,
        axes: &HashMap<String, f32>,
    ) -> Pad {
        let down = |b: &Binding| match b {
            Binding::Key(k) => keys.contains(k),
            Binding::Pad(p) => pad_buttons.contains(p),
        };
        let mut buttons = 0;
        for (bit, list) in &self.buttons {
            if list.iter().any(down) {
                buttons |= bit;
            }
        }
        let mut sticks = [(0, 0); 2];
        for (i, (pad_stick, dirs)) in self.sticks.iter().enumerate() {
            let held = |d: usize| dirs[d].iter().any(|k| keys.contains(k)) as i32;
            let (x, y) = (held(2) - held(0), held(1) - held(3));
            sticks[i] = if (x, y) != (0, 0) {
                (x * STICK_MAX, y * STICK_MAX)
            } else if let Some(s) = pad_stick {
                let axis = |a: &str| {
                    let v = axes.get(&format!("{s}{a}")).copied().unwrap_or(0.0);
                    let v = if v.abs() < 0.1 {
                        0.0
                    } else {
                        v.clamp(-1.0, 1.0)
                    };
                    (v * STICK_MAX as f32).round() as i32
                };
                (axis("X"), axis("Y"))
            } else {
                (0, 0)
            };
        }
        Pad {
            buttons,
            left: sticks[0],
            right: sticks[1],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_default_file_reads_back() {
        let c = InputConfig::parse(DEFAULT).unwrap();
        assert_eq!(c.controller, Controller::ProController);
        assert_eq!(c.buttons.len(), 16);
        assert_eq!(
            c.buttons[0],
            (
                button::A,
                vec![Binding::Key("KeyX".into()), Binding::Pad("East".into())]
            )
        );
        assert_eq!(c.sticks[0].0.as_deref(), Some("LeftStick"));
        assert_eq!(c.sticks[1].1[1], ["KeyI"]);
    }

    #[test]
    fn keys_and_gamepads_make_one_controller() {
        let c = InputConfig::parse(DEFAULT).unwrap();
        let axes: HashMap<String, f32> = [
            ("LeftStickX", 0.5),
            ("LeftStickY", -1.0),
            ("RightStickX", 0.05),
        ]
        .into_iter()
        .map(|(a, v)| (a.to_string(), v))
        .collect();
        let pad = c.pad(&set(&["KeyX", "ArrowUp"]), &set(&["RightTrigger2"]), &axes);
        assert_eq!(pad.buttons, button::A | button::UP | button::ZR);
        assert_eq!(pad.left, (16384, -STICK_MAX));
        // Inside the dead zone, the right stick stays centred.
        assert_eq!(pad.right, (0, 0));
        // Keys win over the gamepad stick.
        let pad = c.pad(&set(&["KeyA", "KeyW"]), &set(&[]), &axes);
        assert_eq!(pad.left, (-STICK_MAX, STICK_MAX));
        assert_eq!(pad.buttons, 0);
    }

    #[test]
    fn mistakes_name_their_line() {
        let bad = |text: &str| InputConfig::parse(text).unwrap_err();
        assert_eq!(
            bad("controller = wii"),
            "line 1: controller is pro or handheld"
        );
        assert_eq!(
            bad("\nA = KeyX"),
            "line 2: \"KeyX\" is not key:<name> or pad:<name>"
        );
        assert_eq!(bad("Home = key:KeyH"), "line 1: unknown setting \"Home\"");
        assert_eq!(
            bad("LeftStickUp = pad:DPadUp"),
            "line 1: LeftStickUp takes keys only"
        );
        assert_eq!(
            bad("RightStick = pad:East"),
            "line 1: RightStick is pad:LeftStick or pad:RightStick"
        );
        assert_eq!(bad("A"), "line 1: expected \"name = value\"");
        let c = InputConfig::parse("controller = handheld # the Joy-Con\nB =").unwrap();
        assert_eq!(c.controller, Controller::Handheld);
        assert_eq!(c.buttons, [(button::B, vec![])]);
    }
}

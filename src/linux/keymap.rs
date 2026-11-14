use crate::proto::{KeyState, MAX_KEYS};
use anyhow::{Result, bail};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hid {
    Modifier(u8),
    Key(u16),
    Consumer(u16),
}

const MOD_LEFT_CONTROL: u8 = 0x01;
const MOD_LEFT_SHIFT: u8 = 0x02;
const MOD_LEFT_OPTION: u8 = 0x04;
const MOD_LEFT_COMMAND: u8 = 0x08;
const MOD_RIGHT_CONTROL: u8 = 0x10;
const MOD_RIGHT_SHIFT: u8 = 0x20;
const MOD_RIGHT_OPTION: u8 = 0x40;
const MOD_RIGHT_COMMAND: u8 = 0x80;

pub const KEY_ESC: u32 = 1;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_RIGHTALT: u32 = 100;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;

/// Translates a Linux evdev key code into the HID usage the Mac's virtual keyboard understands.
pub fn to_hid(code: u32, swap_alt_meta: bool) -> Option<Hid> {
    let code = if swap_alt_meta {
        match code {
            KEY_LEFTALT => KEY_LEFTMETA,
            KEY_LEFTMETA => KEY_LEFTALT,
            KEY_RIGHTALT => KEY_RIGHTMETA,
            KEY_RIGHTMETA => KEY_RIGHTALT,
            other => other,
        }
    } else {
        code
    };
    Some(match code {
        KEY_LEFTCTRL => Hid::Modifier(MOD_LEFT_CONTROL),
        KEY_LEFTSHIFT => Hid::Modifier(MOD_LEFT_SHIFT),
        KEY_LEFTALT => Hid::Modifier(MOD_LEFT_OPTION),
        KEY_LEFTMETA => Hid::Modifier(MOD_LEFT_COMMAND),
        KEY_RIGHTCTRL => Hid::Modifier(MOD_RIGHT_CONTROL),
        KEY_RIGHTSHIFT => Hid::Modifier(MOD_RIGHT_SHIFT),
        KEY_RIGHTALT => Hid::Modifier(MOD_RIGHT_OPTION),
        KEY_RIGHTMETA => Hid::Modifier(MOD_RIGHT_COMMAND),
        113 => Hid::Consumer(0xe2),
        114 => Hid::Consumer(0xea),
        115 => Hid::Consumer(0xe9),
        163 => Hid::Consumer(0xb5),
        164 => Hid::Consumer(0xcd),
        165 => Hid::Consumer(0xb6),
        166 => Hid::Consumer(0xb7),
        161 => Hid::Consumer(0xb8),
        168 => Hid::Consumer(0xb4),
        208 => Hid::Consumer(0xb3),
        224 => Hid::Consumer(0x70),
        225 => Hid::Consumer(0x6f),
        other => Hid::Key(keyboard_usage(other)?),
    })
}

fn keyboard_usage(code: u32) -> Option<u16> {
    const LETTERS: [(u32, u16); 26] = [
        (30, 0x04), (48, 0x05), (46, 0x06), (32, 0x07), (18, 0x08), (33, 0x09), (34, 0x0a),
        (35, 0x0b), (23, 0x0c), (36, 0x0d), (37, 0x0e), (38, 0x0f), (50, 0x10), (49, 0x11),
        (24, 0x12), (25, 0x13), (16, 0x14), (19, 0x15), (31, 0x16), (20, 0x17), (22, 0x18),
        (47, 0x19), (17, 0x1a), (45, 0x1b), (21, 0x1c), (44, 0x1d),
    ];
    if let Some((_, usage)) = LETTERS.iter().find(|(evdev, _)| *evdev == code) {
        return Some(*usage);
    }
    Some(match code {
        2..=10 => 0x1e + (code - 2) as u16,
        11 => 0x27,
        28 => 0x28,
        KEY_ESC => 0x29,
        14 => 0x2a,
        15 => 0x2b,
        57 => 0x2c,
        12 => 0x2d,
        13 => 0x2e,
        26 => 0x2f,
        27 => 0x30,
        43 => 0x31,
        39 => 0x33,
        40 => 0x34,
        41 => 0x35,
        51 => 0x36,
        52 => 0x37,
        53 => 0x38,
        58 => 0x39,
        59..=68 => 0x3a + (code - 59) as u16,
        87 => 0x44,
        88 => 0x45,
        99 => 0x46,
        70 => 0x47,
        119 => 0x48,
        110 => 0x49,
        102 => 0x4a,
        104 => 0x4b,
        111 => 0x4c,
        107 => 0x4d,
        109 => 0x4e,
        106 => 0x4f,
        105 => 0x50,
        108 => 0x51,
        103 => 0x52,
        69 => 0x53,
        98 => 0x54,
        55 => 0x55,
        74 => 0x56,
        78 => 0x57,
        96 => 0x58,
        79 => 0x59,
        80 => 0x5a,
        81 => 0x5b,
        75 => 0x5c,
        76 => 0x5d,
        77 => 0x5e,
        71 => 0x5f,
        72 => 0x60,
        73 => 0x61,
        82 => 0x62,
        83 => 0x63,
        86 => 0x64,
        127 => 0x65,
        116 => 0x66,
        117 => 0x67,
        183..=194 => 0x68 + (code - 183) as u16,
        138 => 0x75,
        139 => 0x76,
        128 => 0x78,
        129 => 0x79,
        131 => 0x7a,
        137 => 0x7b,
        133 => 0x7c,
        135 => 0x7d,
        136 => 0x7e,
        121 => 0x85,
        89 => 0x87,
        93 => 0x88,
        124 => 0x89,
        92 => 0x8a,
        94 => 0x8b,
        95 => 0x8c,
        122 => 0x90,
        123 => 0x91,
        90 => 0x92,
        91 => 0x93,
        85 => 0x94,
        _ => return None,
    })
}

/// Builds the full HID keyboard state from the set of evdev codes currently held.
pub fn key_state(pressed: &BTreeSet<u32>, swap_alt_meta: bool) -> KeyState {
    let mut state = KeyState::default();
    for code in pressed {
        match to_hid(*code, swap_alt_meta) {
            Some(Hid::Modifier(bit)) => state.modifiers |= bit,
            Some(Hid::Key(usage)) if state.keys.len() < MAX_KEYS => state.keys.push(usage),
            Some(Hid::Consumer(usage)) if state.consumer.len() < MAX_KEYS => state.consumer.push(usage),
            _ => {}
        }
    }
    state
}

/// Maps an evdev pointer button code to the HID button number macOS expects.
pub fn button_bit(code: u32) -> Option<u32> {
    let button = match code {
        0x110 => 1,
        0x111 => 2,
        0x112 => 3,
        0x113 | 0x116 => 4,
        0x114 | 0x115 => 5,
        0x117 => 6,
        0x118..=0x11f => 7 + (code - 0x118),
        _ => return None,
    };
    Some(1 << (button - 1))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Modifiers {
    meta: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl Modifiers {
    fn held(pressed: &BTreeSet<u32>) -> Self {
        Self {
            meta: pressed.contains(&KEY_LEFTMETA) || pressed.contains(&KEY_RIGHTMETA),
            ctrl: pressed.contains(&KEY_LEFTCTRL) || pressed.contains(&KEY_RIGHTCTRL),
            alt: pressed.contains(&KEY_LEFTALT) || pressed.contains(&KEY_RIGHTALT),
            shift: pressed.contains(&KEY_LEFTSHIFT) || pressed.contains(&KEY_RIGHTSHIFT),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    modifiers: Modifiers,
    key: u32,
    qt_name: String,
}

impl Hotkey {
    pub fn parse(text: &str) -> Result<Self> {
        let mut modifiers = Modifiers::default();
        let mut key = None;
        let mut qt_parts = Vec::new();
        for part in text.split('+').map(str::trim).filter(|part| !part.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "meta" | "super" | "win" | "cmd" => {
                    modifiers.meta = true;
                    qt_parts.push("Meta".to_string());
                }
                "ctrl" | "control" => {
                    modifiers.ctrl = true;
                    qt_parts.push("Ctrl".to_string());
                }
                "alt" | "option" => {
                    modifiers.alt = true;
                    qt_parts.push("Alt".to_string());
                }
                "shift" => {
                    modifiers.shift = true;
                    qt_parts.push("Shift".to_string());
                }
                name => {
                    if key.is_some() {
                        bail!("hotkey {text:?} names more than one key");
                    }
                    let (code, qt_name) = key_by_name(name)
                        .ok_or_else(|| anyhow::anyhow!("unknown key {part:?} in hotkey {text:?}"))?;
                    key = Some(code);
                    qt_parts.push(qt_name);
                }
            }
        }
        let Some(key) = key else { bail!("hotkey {text:?} names no key") };
        if modifiers == Modifiers::default() {
            bail!("hotkey {text:?} needs at least one modifier");
        }
        Ok(Self { modifiers, key, qt_name: qt_parts.join("+") })
    }

    /// True when `code` was just pressed and exactly the hotkey's modifiers are held.
    pub fn matches(&self, code: u32, pressed: &BTreeSet<u32>) -> bool {
        code == self.key && Modifiers::held(pressed) == self.modifiers
    }

    pub fn qt_name(&self) -> &str {
        &self.qt_name
    }
}

fn key_by_name(name: &str) -> Option<(u32, String)> {
    const NAMED: &[(&str, u32, &str)] = &[
        ("escape", KEY_ESC, "Esc"),
        ("esc", KEY_ESC, "Esc"),
        ("space", 57, "Space"),
        ("tab", 15, "Tab"),
        ("return", 28, "Return"),
        ("enter", 28, "Return"),
        ("backspace", 14, "Backspace"),
        ("grave", 41, "`"),
        ("`", 41, "`"),
        ("backslash", 43, "\\"),
        ("\\", 43, "\\"),
        ("minus", 12, "-"),
        ("equal", 13, "="),
        ("left", 105, "Left"),
        ("right", 106, "Right"),
        ("up", 103, "Up"),
        ("down", 108, "Down"),
        ("home", 102, "Home"),
        ("end", 107, "End"),
        ("pageup", 104, "PgUp"),
        ("pagedown", 109, "PgDown"),
        ("insert", 110, "Ins"),
        ("delete", 111, "Del"),
        ("pause", 119, "Pause"),
        ("scrolllock", 70, "ScrollLock"),
        ("print", 99, "Print"),
    ];
    if let Some((_, code, qt)) = NAMED.iter().find(|(named, _, _)| *named == name) {
        return Some((*code, (*qt).to_string()));
    }
    if let Some(number) = name.strip_prefix('f').and_then(|rest| rest.parse::<u32>().ok()) {
        let code = match number {
            1..=10 => 58 + number,
            11 => 87,
            12 => 88,
            13..=24 => 170 + number,
            _ => return None,
        };
        return Some((code, format!("F{number}")));
    }
    let mut chars = name.chars();
    let (Some(single), None) = (chars.next(), chars.next()) else { return None };
    const ROWS: [(&str, u32); 4] = [("1234567890", 2), ("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)];
    ROWS.iter().find_map(|(row, first)| {
        row.find(single).map(|offset| (first + offset as u32, single.to_ascii_uppercase().to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_map_to_hid() {
        assert_eq!(to_hid(30, false), Some(Hid::Key(0x04)));
        assert_eq!(to_hid(44, false), Some(Hid::Key(0x1d)));
        assert_eq!(to_hid(2, false), Some(Hid::Key(0x1e)));
        assert_eq!(to_hid(11, false), Some(Hid::Key(0x27)));
        assert_eq!(to_hid(88, false), Some(Hid::Key(0x45)));
        assert_eq!(to_hid(194, false), Some(Hid::Key(0x73)));
    }

    #[test]
    fn letters_cover_qwerty() {
        let evdev = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
        for (index, code) in evdev.iter().enumerate() {
            assert_eq!(to_hid(*code, false), Some(Hid::Key(0x04 + index as u16)));
        }
    }

    #[test]
    fn modifiers_and_swap() {
        assert_eq!(to_hid(KEY_LEFTMETA, false), Some(Hid::Modifier(MOD_LEFT_COMMAND)));
        assert_eq!(to_hid(KEY_LEFTALT, false), Some(Hid::Modifier(MOD_LEFT_OPTION)));
        assert_eq!(to_hid(KEY_LEFTMETA, true), Some(Hid::Modifier(MOD_LEFT_OPTION)));
        assert_eq!(to_hid(KEY_RIGHTALT, true), Some(Hid::Modifier(MOD_RIGHT_COMMAND)));
    }

    #[test]
    fn media_keys_use_consumer_page() {
        assert_eq!(to_hid(115, false), Some(Hid::Consumer(0xe9)));
        assert_eq!(to_hid(164, false), Some(Hid::Consumer(0xcd)));
    }

    #[test]
    fn key_state_splits_modifiers() {
        let pressed: BTreeSet<u32> = [KEY_LEFTMETA, 46, 115].into_iter().collect();
        let state = key_state(&pressed, false);
        assert_eq!(state.modifiers, MOD_LEFT_COMMAND);
        assert_eq!(state.keys, vec![0x06]);
        assert_eq!(state.consumer, vec![0xe9]);
    }

    #[test]
    fn buttons() {
        assert_eq!(button_bit(0x110), Some(1));
        assert_eq!(button_bit(0x111), Some(2));
        assert_eq!(button_bit(0x112), Some(4));
        assert_eq!(button_bit(0x113), Some(8));
        assert_eq!(button_bit(0x114), Some(16));
        assert_eq!(button_bit(0x100), None);
    }

    #[test]
    fn hotkey_matches_exact_modifiers() {
        let hotkey = Hotkey::parse("Meta+Escape").unwrap();
        assert_eq!(hotkey.qt_name(), "Meta+Esc");
        let meta: BTreeSet<u32> = [KEY_LEFTMETA, KEY_ESC].into_iter().collect();
        assert!(hotkey.matches(KEY_ESC, &meta));
        let meta_alt: BTreeSet<u32> = [KEY_LEFTMETA, KEY_LEFTALT, KEY_ESC].into_iter().collect();
        assert!(!hotkey.matches(KEY_ESC, &meta_alt));
        assert!(!hotkey.matches(30, &meta));
    }

    #[test]
    fn hotkey_parsing() {
        assert_eq!(Hotkey::parse("ctrl+alt+f12").unwrap().qt_name(), "Ctrl+Alt+F12");
        assert_eq!(Hotkey::parse("Meta+m").unwrap().key, 50);
        assert!(Hotkey::parse("Escape").is_err());
        assert!(Hotkey::parse("Meta+Bogus").is_err());
        assert!(Hotkey::parse("Meta+a+b").is_err());
    }
}

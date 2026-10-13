//! What the viewer does with each physical key while it controls a host: forward it,
//! leave it to Windows, or treat it as a viewer shortcut. Platform-independent, so it is
//! tested everywhere; `hook.rs` feeds it from a Windows low-level keyboard hook.
//!
//! Keys are identified by position (scan code), never by character, and forwarded as USB
//! HID keyboard usages. Ctrl, Shift and Alt forward as themselves (Control, Shift and
//! Option on a Mac). Command is whichever key the user picked; nothing is guessed.
//!
//! Windowed, the modifiers also reach Windows, so Alt+Tab, Alt+Esc, Ctrl+Esc and Alt+F4
//! still work locally and are not forwarded. Fullscreen, every mapped key goes to the
//! host. Keys this table doesn't know (media keys, for example) always stay local.
//! Ctrl+Alt+Del and Win+L are handled by Windows before any hook and can't be forwarded.

use nocomputer_proto::KeyState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CommandKey {
    /// Both Windows keys send Command. Windows then never sees them, so the Start menu
    /// doesn't open.
    #[default]
    Win,
    /// Right Alt sends Command; the Windows keys stay with Windows.
    RightAlt,
}

/// Viewer controls, on Ctrl+Alt+Shift plus a key, which is not forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shortcut {
    Fullscreen,
    Native,
    Status,
    Reset,
    PanLeft,
    PanRight,
    PanUp,
    PanDown,
}

/// One key event as the hook reports it.
#[derive(Debug, Clone, Copy)]
pub struct RawKey {
    pub scan: u32,
    pub extended: bool,
    pub vk: u32,
    pub down: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Decision {
    /// Keep the key from Windows and the page.
    pub swallow: bool,
    pub send: Option<(u16, KeyState)>,
    pub shortcut: Option<Shortcut>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// Sent to the host as this usage; also reached Windows if `passed`.
    Forwarded { usage: u16, passed: bool },
    /// Left to Windows only.
    Local,
    /// A viewer shortcut.
    Consumed,
}

const LEFT_CTRL: u16 = 0xE0;
const LEFT_SHIFT: u16 = 0xE1;
const LEFT_ALT: u16 = 0xE2;
const LEFT_GUI: u16 = 0xE3;
const RIGHT_CTRL: u16 = 0xE4;
const RIGHT_SHIFT: u16 = 0xE5;
const RIGHT_ALT: u16 = 0xE6;
const RIGHT_GUI: u16 = 0xE7;
const TAB: u16 = 0x2B;
const ESCAPE: u16 = 0x29;
const F4: u16 = 0x3D;

#[derive(Default)]
pub struct Keyboard {
    pub command: CommandKey,
    pub fullscreen: bool,
    /// By physical usage.
    held: BTreeMap<u16, Held>,
    /// A forwarded key went down while an Alt that Windows saw was held. Releasing that
    /// Alt alone would open the window menu, so the hook masks the release.
    alt_used: bool,
}

/// Physical position to HID usage, from the scan code (set 1) and extended flag. The
/// virtual key decides only where scan codes are ambiguous.
pub fn physical_usage(key: &RawKey) -> Option<u16> {
    match key.vk {
        0x13 => return Some(0x48), // Pause
        0x90 => return Some(0x53), // Num Lock
        0x2C => return Some(0x46), // Print Screen
        _ => {}
    }
    if key.scan > 0xFF {
        return None;
    }
    let usage = if key.extended {
        match key.scan {
            0x1C => 0x58, // keypad Enter
            0x1D => RIGHT_CTRL,
            0x35 => 0x54, // keypad /
            0x38 => RIGHT_ALT,
            0x47 => 0x4A, // Home
            0x48 => 0x52, // Up
            0x49 => 0x4B, // Page Up
            0x4B => 0x50, // Left
            0x4D => 0x4F, // Right
            0x4F => 0x4D, // End
            0x50 => 0x51, // Down
            0x51 => 0x4E, // Page Down
            0x52 => 0x49, // Insert
            0x53 => 0x4C, // Delete
            0x5B => LEFT_GUI,
            0x5C => RIGHT_GUI,
            0x5D => 0x65, // Menu
            _ => return None,
        }
    } else {
        const MAIN: [u16; 0x59] = [
            0, 0x29, 0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x2D, 0x2E, 0x2A, 0x2B, // 00-0F: Esc, 1-0, - =, Backspace, Tab
            0x14, 0x1A, 0x08, 0x15, 0x17, 0x1C, 0x18, 0x0C, 0x12, 0x13, 0x2F, 0x30, 0x28, 0xE0, 0x04, 0x16, // 10-1F: Q-P [ ] Enter LCtrl A S
            0x07, 0x09, 0x0A, 0x0B, 0x0D, 0x0E, 0x0F, 0x33, 0x34, 0x35, 0xE1, 0x31, 0x1D, 0x1B, 0x06, 0x19, // 20-2F: D-L ; ' ` LShift \ Z X C V
            0x05, 0x11, 0x10, 0x36, 0x37, 0x38, 0xE5, 0x55, 0xE2, 0x2C, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, // 30-3F: B N M , . / RShift KP* LAlt Space Caps F1-F5
            0x3F, 0x40, 0x41, 0x42, 0x43, 0, 0x47, 0x5F, 0x60, 0x61, 0x56, 0x5C, 0x5D, 0x5E, 0x57, 0x59, // 40-4F: F6-F10, (Num Lock by vk), Scroll Lock, keypad 7 8 9 - 4 5 6 + 1
            0x5A, 0x5B, 0x62, 0x63, 0, 0, 0x64, 0x44, 0x45, // 50-58: keypad 2 3 0 ., -, -, non-US \, F11, F12
        ];
        match key.scan {
            s @ 0..=0x58 => MAIN[s as usize],
            s @ 0x64..=0x6E => 0x68 + (s - 0x64) as u16, // F13-F23
            0x76 => 0x73,                                // F24
            0x70 => 0x88,                                // International 2 (Katakana/Hiragana)
            0x73 => 0x87,                                // International 1 (Ro)
            0x79 => 0x8A,                                // International 4 (Henkan)
            0x7B => 0x8B,                                // International 5 (Muhenkan)
            0x7D => 0x89,                                // International 3 (Yen)
            _ => 0,
        }
    };
    (usage != 0).then_some(usage)
}

fn shortcut(usage: u16) -> Option<Shortcut> {
    Some(match usage {
        0x09 => Shortcut::Fullscreen, // F
        0x11 => Shortcut::Native,     // N
        0x0B => Shortcut::Status,     // H
        0x15 => Shortcut::Reset,      // R
        0x50 => Shortcut::PanLeft,
        0x4F => Shortcut::PanRight,
        0x52 => Shortcut::PanUp,
        0x51 => Shortcut::PanDown,
        _ => return None,
    })
}

impl Keyboard {
    pub const fn new() -> Self {
        Keyboard { command: CommandKey::Win, fullscreen: false, held: BTreeMap::new(), alt_used: false }
    }

    /// What the host receives for a physical key; `None` leaves it to Windows.
    fn remap(&self, usage: u16) -> Option<u16> {
        match (usage, self.command) {
            (LEFT_GUI | RIGHT_GUI, CommandKey::RightAlt) => None,
            (RIGHT_ALT, CommandKey::RightAlt) => Some(RIGHT_GUI),
            _ => Some(usage),
        }
    }

    /// Ctrl, Shift or Alt as the host sees them; these reach Windows too when windowed.
    fn is_shared_modifier(&self, usage: u16) -> bool {
        match usage {
            LEFT_CTRL | RIGHT_CTRL | LEFT_SHIFT | RIGHT_SHIFT | LEFT_ALT => true,
            RIGHT_ALT => self.command != CommandKey::RightAlt,
            _ => false,
        }
    }

    fn holding(&self, usages: &[u16]) -> bool {
        usages.iter().any(|u| matches!(self.held.get(u), Some(Held::Forwarded { .. } | Held::Local)))
    }

    fn alt(&self) -> bool {
        self.holding(&[LEFT_ALT]) || (self.command != CommandKey::RightAlt && self.holding(&[RIGHT_ALT]))
    }

    fn ctrl(&self) -> bool {
        self.holding(&[LEFT_CTRL, RIGHT_CTRL])
    }

    fn shift(&self) -> bool {
        self.holding(&[LEFT_SHIFT, RIGHT_SHIFT])
    }

    pub fn on_key(&mut self, key: RawKey) -> Decision {
        let Some(usage) = physical_usage(&key) else {
            // AltGr's synthetic Left Ctrl (scan 0x21D) belongs to Right Alt: drop it.
            return Decision { swallow: key.scan & 0x200 != 0 && key.scan & 0xFF == 0x1D, ..Default::default() };
        };
        if key.down {
            self.down(usage)
        } else {
            self.up(usage)
        }
    }

    fn down(&mut self, usage: u16) -> Decision {
        match self.held.get(&usage) {
            Some(Held::Forwarded { usage: sent, passed }) => {
                // Modifiers never repeat on the host; Windows still sees them repeat.
                let send = (!(LEFT_CTRL..=RIGHT_GUI).contains(sent)).then_some((*sent, KeyState::Repeat));
                return Decision { swallow: !passed, send, shortcut: None };
            }
            Some(Held::Local) => return Decision::default(),
            Some(Held::Consumed) => return Decision { swallow: true, ..Default::default() },
            None => {}
        }
        if self.ctrl() && self.alt() && self.shift() {
            if let Some(s) = shortcut(usage) {
                self.held.insert(usage, Held::Consumed);
                return Decision { swallow: true, send: None, shortcut: Some(s) };
            }
        }
        let local_combo = !self.fullscreen && ((usage == TAB || usage == ESCAPE || usage == F4) && self.alt() || usage == ESCAPE && self.ctrl());
        let Some(sent) = self.remap(usage).filter(|_| !local_combo) else {
            self.held.insert(usage, Held::Local);
            return Decision::default();
        };
        let passed = !self.fullscreen && self.is_shared_modifier(usage);
        if !passed && self.held.iter().any(|(u, h)| matches!(*u, LEFT_ALT | RIGHT_ALT) && matches!(h, Held::Forwarded { passed: true, .. })) {
            self.alt_used = true;
        }
        self.held.insert(usage, Held::Forwarded { usage: sent, passed });
        Decision { swallow: !passed, send: Some((sent, KeyState::Down)), shortcut: None }
    }

    fn up(&mut self, usage: u16) -> Decision {
        match self.held.remove(&usage) {
            Some(Held::Forwarded { usage: sent, passed }) => Decision { swallow: !passed, send: Some((sent, KeyState::Up)), shortcut: None },
            Some(Held::Consumed) => Decision { swallow: true, ..Default::default() },
            // Local, or pressed before the hook was installed.
            Some(Held::Local) | None => Decision::default(),
        }
    }

    /// Whether this Alt release must be masked so Windows doesn't open the window menu.
    /// Call after [`Keyboard::on_key`] for an Alt key-up that is not swallowed.
    pub fn take_alt_mask(&mut self) -> bool {
        !self.alt() && std::mem::take(&mut self.alt_used)
    }

    /// Forgets everything held, after focus moves away (the host is told separately).
    pub fn reset(&mut self) {
        self.held.clear();
        self.alt_used = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scan: u32, extended: bool, down: bool) -> RawKey {
        RawKey { scan, extended, vk: 0, down }
    }
    const A: u32 = 0x1E;
    const LALT: u32 = 0x38;
    const LCTRL: u32 = 0x1D;
    const LSHIFT: u32 = 0x2A;
    const LWIN: u32 = 0x5B;

    #[test]
    fn scan_codes_map_to_positions() {
        assert_eq!(physical_usage(&key(A, false, true)), Some(0x04));
        assert_eq!(physical_usage(&key(0x2C, false, true)), Some(0x1D), "Z");
        assert_eq!(physical_usage(&key(0x1C, false, true)), Some(0x28), "Enter");
        assert_eq!(physical_usage(&key(0x1C, true, true)), Some(0x58), "keypad Enter");
        assert_eq!(physical_usage(&key(0x48, true, true)), Some(0x52), "Up");
        assert_eq!(physical_usage(&key(0x48, false, true)), Some(0x60), "keypad 8");
        assert_eq!(physical_usage(&key(0x58, false, true)), Some(0x45), "F12");
        assert_eq!(physical_usage(&key(0x64, false, true)), Some(0x68), "F13");
        assert_eq!(physical_usage(&RawKey { scan: 0x45, extended: true, vk: 0x90, down: true }), Some(0x53), "Num Lock");
        assert_eq!(physical_usage(&RawKey { scan: 0x45, extended: false, vk: 0x13, down: true }), Some(0x48), "Pause");
        assert_eq!(physical_usage(&key(0x20, true, true)), None, "Mute stays local");
        assert_eq!(physical_usage(&key(0x21D, false, true)), None);
    }

    #[test]
    fn the_main_block_maps_to_distinct_usages() {
        let mut seen = std::collections::HashSet::new();
        for scan in 1..=0x58 {
            if let Some(u) = physical_usage(&key(scan, false, true)) {
                assert!(seen.insert(u), "scan {scan:#x} repeats usage {u:#x}");
            }
        }
        assert!(seen.len() >= 83, "{}", seen.len());
    }

    #[test]
    fn typing_is_forwarded_and_kept_from_windows() {
        let mut k = Keyboard::default();
        assert_eq!(k.on_key(key(A, false, true)), Decision { swallow: true, send: Some((0x04, KeyState::Down)), shortcut: None });
        assert_eq!(k.on_key(key(A, false, true)).send, Some((0x04, KeyState::Repeat)));
        assert_eq!(k.on_key(key(A, false, false)), Decision { swallow: true, send: Some((0x04, KeyState::Up)), shortcut: None });
        assert_eq!(k.on_key(key(A, false, false)), Decision::default(), "an up we never saw go down stays local");
    }

    #[test]
    fn command_is_the_users_choice() {
        let mut k = Keyboard::default();
        assert_eq!(k.on_key(key(LWIN, true, true)), Decision { swallow: true, send: Some((LEFT_GUI, KeyState::Down)), shortcut: None });
        assert_eq!(k.on_key(key(LWIN, true, true)), Decision { swallow: true, send: None, shortcut: None }, "Command doesn't repeat");
        assert_eq!(k.on_key(key(LWIN, true, false)).send, Some((LEFT_GUI, KeyState::Up)));
        assert_eq!(k.on_key(key(LALT, true, true)).send, Some((RIGHT_ALT, KeyState::Down)), "Right Alt is Option");

        let mut k = Keyboard { command: CommandKey::RightAlt, ..Default::default() };
        assert_eq!(k.on_key(key(LWIN, true, true)), Decision::default(), "the Windows key stays with Windows");
        assert_eq!(k.on_key(key(LALT, true, true)), Decision { swallow: true, send: Some((RIGHT_GUI, KeyState::Down)), shortcut: None });
        assert_eq!(k.on_key(key(LALT, true, false)), Decision { swallow: true, send: Some((RIGHT_GUI, KeyState::Up)), shortcut: None });
    }

    #[test]
    fn windowed_modifiers_reach_both_and_local_combos_stay_local() {
        let mut k = Keyboard::default();
        let alt = k.on_key(key(LALT, false, true));
        assert_eq!(alt, Decision { swallow: false, send: Some((LEFT_ALT, KeyState::Down)), shortcut: None });
        assert_eq!(k.on_key(key(0x0F, false, true)), Decision::default(), "Alt+Tab switches windows locally");
        assert_eq!(k.on_key(key(0x0F, false, false)), Decision::default());
        assert_eq!(k.on_key(key(LALT, false, false)).send, Some((LEFT_ALT, KeyState::Up)));
        assert!(!k.take_alt_mask(), "Alt+Tab released Alt with nothing forwarded");

        // Option+A: Windows saw Alt go down and up with nothing between, so mask it.
        k.on_key(key(LALT, false, true));
        k.on_key(key(A, false, true));
        k.on_key(key(A, false, false));
        k.on_key(key(LALT, false, false));
        assert!(k.take_alt_mask());
        assert!(!k.take_alt_mask());

        k.on_key(key(LCTRL, false, true));
        assert_eq!(k.on_key(key(0x01, false, true)), Decision::default(), "Ctrl+Esc opens Start locally");
    }

    #[test]
    fn fullscreen_forwards_everything_mapped() {
        let mut k = Keyboard { fullscreen: true, ..Default::default() };
        assert_eq!(k.on_key(key(LALT, false, true)), Decision { swallow: true, send: Some((LEFT_ALT, KeyState::Down)), shortcut: None });
        assert_eq!(k.on_key(key(0x0F, false, true)).send, Some((TAB, KeyState::Down)));
        assert_eq!(k.on_key(key(0x3E, false, true)).send, Some((F4, KeyState::Down)));
        assert_eq!(k.on_key(key(0x20, true, true)), Decision::default(), "media keys stay local");
    }

    #[test]
    fn ctrl_alt_shift_letters_are_viewer_shortcuts() {
        let mut k = Keyboard::default();
        for scan in [LCTRL, LALT, LSHIFT] {
            k.on_key(key(scan, false, true));
        }
        assert_eq!(k.on_key(key(0x21, false, true)), Decision { swallow: true, send: None, shortcut: Some(Shortcut::Fullscreen) });
        assert_eq!(k.on_key(key(0x21, false, true)), Decision { swallow: true, send: None, shortcut: None }, "no repeat");
        assert_eq!(k.on_key(key(0x21, false, false)), Decision { swallow: true, send: None, shortcut: None });
        assert_eq!(k.on_key(key(0x1F, false, true)).send, Some((0x16, KeyState::Down)), "other letters are forwarded");
    }

    #[test]
    fn altgr_synthetic_ctrl_is_dropped() {
        let mut k = Keyboard::default();
        assert_eq!(k.on_key(key(0x21D, false, true)), Decision { swallow: true, send: None, shortcut: None });
    }
}

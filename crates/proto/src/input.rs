//! Remote input from the controlling viewer, and notices from the host about it.
//!
//! Input travels on the control stream (see `wire.rs`) as `{"type":"input","kind":...}`.
//! Keys are physical: USB HID usages from the keyboard page (0x07), never characters,
//! so the keyboard layout stays the host's concern. Any modifier remapping (which key
//! sends Command, for example) happens on the viewer before the usage is sent. Neither
//! side logs input contents.

use serde::{Deserialize, Serialize};

/// Largest scroll the host passes on per message, in each axis.
pub const MAX_SCROLL_PIXELS: i32 = 10_000;
pub const MAX_SCROLL_LINES: i32 = 100;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    /// Absolute position in the video picture: 0 is the left (top) edge and 65535 the
    /// right (bottom) edge. The viewer sends no positions outside the picture.
    PointerMove { x: u16, y: u16 },
    /// At the last position.
    PointerButton { button: Button, down: bool },
    /// DOM sign convention: positive `dy` scrolls toward the end of the document.
    Scroll { dx: i32, dy: i32, unit: ScrollUnit },
    Key { usage: u16, state: KeyState },
    /// Release every key and button this viewer holds down.
    ReleaseAll,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    /// From a newer viewer; dropped by the host.
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScrollUnit {
    Pixel,
    Line,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    Up,
    Down,
    /// The viewer's own autorepeat of a key already down.
    Repeat,
}

impl Input {
    /// What the host passes on: unknown buttons dropped, scrolls clamped.
    pub fn sanitized(self) -> Option<Input> {
        match self {
            Input::PointerButton { button: Button::Unknown, .. } => None,
            Input::Scroll { dx, dy, unit } => {
                let max = match unit {
                    ScrollUnit::Pixel => MAX_SCROLL_PIXELS,
                    ScrollUnit::Line => MAX_SCROLL_LINES,
                };
                let (dx, dy) = (dx.clamp(-max, max), dy.clamp(-max, max));
                (dx != 0 || dy != 0).then_some(Input::Scroll { dx, dy, unit })
            }
            other => Some(other),
        }
    }
}

/// Something the viewer should show about the host's state, from the host's capture
/// side. `message` is for display only.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// The host cannot inject input (for example, macOS Accessibility is not granted).
    InputUnavailable,
    /// The host stopped capturing; video will not resume until it restarts.
    CaptureStopped,
    #[serde(other)]
    Unknown,
}

/// Longest notice message the host sends, in bytes.
pub const MAX_NOTICE: usize = 1024;

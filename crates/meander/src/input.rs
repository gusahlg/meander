//! Pointer input types.
//!
//! Keyboard input is intentionally absent from this first cut. With gharial
//! managing global key bindings via `river-xkb-bindings-v1`, individual
//! meander surfaces rarely want raw keys; a launcher that does will land in a
//! follow-up.

use crate::surface::SurfaceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
    /// evdev button code for buttons outside the named set.
    Other(u32),
}

impl PointerButton {
    pub(crate) fn from_code(code: u32) -> Self {
        // values from <linux/input-event-codes.h>
        match code {
            0x110 => Self::Left,
            0x111 => Self::Right,
            0x112 => Self::Middle,
            0x113 => Self::Back,
            0x114 => Self::Forward,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Axis {
    /// Positive = down, negative = up. Units: pointer-axis "discrete steps"
    /// when available, otherwise pixels (compositor-defined).
    Vertical(f64),
    /// Positive = right, negative = left.
    Horizontal(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PointerEventKind {
    Enter,
    Leave,
    Motion,
    Press(PointerButton),
    Release(PointerButton),
    Scroll(Axis),
}

/// Coalesced pointer event for one surface.
///
/// Wayland delivers pointer state across multiple events terminated by a
/// `frame` marker; meander batches them into one `PointerEvent` per frame so
/// you don't have to track partial state yourself.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerEvent {
    pub surface: SurfaceId,
    pub kind: PointerEventKind,
    /// Position in physical pixels relative to the surface origin.
    pub x: f64,
    pub y: f64,
}

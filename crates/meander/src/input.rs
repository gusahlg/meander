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

/// Which axis a scroll event moved along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAxis {
    /// Positive = down, negative = up.
    Vertical,
    /// Positive = right, negative = left.
    Horizontal,
}

/// The physical origin of a scroll, as advertised by `wl_pointer.axis_source`
/// (available on `wl_pointer` v5+). Distinguishes a notched wheel from
/// continuous touchpad scrolling so callers can apply the right acceleration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSource {
    /// A physical, usually-notched scroll wheel.
    Wheel,
    /// A touchpad finger drag (kinetic; ends with a stop).
    Finger,
    /// A continuous-motion device with no notches.
    Continuous,
    /// Sideways wheel tilt.
    WheelTilt,
    /// The compositor did not advertise a source (pre-v5 or unknown value).
    Unknown,
}

/// One coalesced scroll event.
///
/// Wayland reports a continuous `value` plus, for notched wheels, a discrete
/// step count. Meander keeps them separate so a caller never mistakes a
/// continuous touchpad delta for a wheel notch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scroll {
    pub axis: ScrollAxis,
    /// Continuous scroll distance in surface-local pixels
    /// (compositor-defined). Zero for an `axis_stop`.
    pub value: f64,
    /// Discrete step count for notched wheels: from `axis_value120` (÷120) on
    /// `wl_pointer` v8+, or `axis_discrete` on v5–7. `None` for touchpad /
    /// continuous sources and on pre-v5 pointers.
    pub discrete: Option<f64>,
    /// Where the scroll originated, when advertised (v5+); otherwise
    /// [`ScrollSource::Unknown`].
    pub source: ScrollSource,
    /// True when this event marks the end of a kinetic scroll
    /// (`wl_pointer.axis_stop`, v5+). `value` is 0 in that case.
    pub stop: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PointerEventKind {
    Enter,
    Leave,
    Motion,
    Press(PointerButton),
    Release(PointerButton),
    Scroll(Scroll),
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

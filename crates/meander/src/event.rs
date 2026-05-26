//! High-level events surfaced by the [`App`](crate::App) event loop.

use crate::input::PointerEvent;
use crate::output::OutputId;
use crate::surface::SurfaceId;

/// Why a surface was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The compositor told us to go away (`layer_surface::closed`).
    Compositor,
}

/// Everything the runtime can hand back to your event loop.
///
/// Drain this in a `while let Some(ev) = app.next_event()` loop after each
/// `dispatch`. Variants intentionally stay flat — meander's job is to turn the
/// Wayland wire into a stream of plain data, not to wrap it in builders.
#[derive(Debug, Clone)]
pub enum Event {
    /// The compositor has finalised a size for a surface. Meander has already
    /// acked it; you should draw the surface using these dimensions before the
    /// next dispatch returns.
    Configure {
        surface: SurfaceId,
        /// Logical size, before scale.
        width: u32,
        height: u32,
        /// Scale of the output(s) the surface is on (1, 2, 3, ...). Buffer
        /// pixel dimensions are `width * scale` by `height * scale`.
        scale: i32,
    },

    /// A frame callback we previously requested has fired. Time is the
    /// compositor's millisecond clock — use deltas, never the absolute value.
    Frame { surface: SurfaceId, time_ms: u32 },

    /// The compositor closed the surface. After this you should `destroy` it
    /// and stop drawing.
    Closed {
        surface: SurfaceId,
        reason: CloseReason,
    },

    /// A monitor appeared.
    OutputAdded(OutputId),
    /// A monitor's parameters changed (mode, scale, name, ...).
    OutputChanged(OutputId),
    /// A monitor was unplugged.
    OutputRemoved(OutputId),

    /// Pointer event delivered to one of our surfaces.
    Pointer(PointerEvent),
}

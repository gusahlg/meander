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
    /// A monitor was unplugged. Meander has already purged it from every
    /// surface and re-emitted a [`Configure`](Event::Configure) for any surface
    /// whose scale changed as a result, *before* this event.
    OutputRemoved(OutputId),

    /// A compositor global meander depends on (`wl_compositor`, `wl_shm`, or
    /// `zwlr_layer_shell_v1`) was removed at runtime. The connection can no
    /// longer service surfaces; you should tear down and exit. Named by
    /// interface for logging.
    GlobalLost(&'static str),

    /// Pointer event delivered to one of our surfaces.
    Pointer(PointerEvent),
}

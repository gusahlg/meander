//! The wayland runtime: connection, dispatch impls, event-loop helpers.
//!
//! `mod.rs` holds the public API (`App`, `SurfaceHandle`, the layer-surface
//! builder, and the shared `AppState`). The noisier protocol bindings live in
//! the sibling [`dispatch`] module so callers only see meander's domain layer.

mod dispatch;

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use tiny_skia::PixmapMut;
use wayland_client::{
    protocol::{wl_compositor, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::buffer_layout::BufferLayout;
use crate::canvas::Canvas;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::input::PointerEvent;
use crate::output::OutputEntry;
use crate::shm::ShmPool;
use crate::shm_format::PixelFormat;
use crate::surface::{Anchor, KeyboardInteractivity, Layer, LayerSurfaceBuilder, SurfaceId};

pub(crate) use caps::{Capabilities, PointerCaps};

mod caps;

/// Top-level handle to the Wayland connection and all the surfaces meander is
/// managing for you. Owns the event queue, the shm globals, the seats, and
/// the output table.
pub struct App {
    conn: Connection,
    queue: EventQueue<AppState>,
    state: AppState,
}

pub(crate) struct AppState {
    pub(crate) qh: QueueHandle<AppState>,

    pub(crate) compositor: Option<wl_compositor::WlCompositor>,
    pub(crate) shm: Option<wl_shm::WlShm>,
    pub(crate) layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,

    // Registry names of the singleton globals, retained so `global_remove` can
    // detect the loss of a required global.
    pub(crate) compositor_registry_name: Option<u32>,
    pub(crate) shm_registry_name: Option<u32>,
    pub(crate) layer_shell_registry_name: Option<u32>,

    /// Negotiated protocol versions and advertised optional features.
    pub(crate) caps: Capabilities,

    pub(crate) seats: Vec<SeatEntry>,
    pub(crate) outputs: Vec<OutputEntry>,
    pub(crate) surfaces: Vec<SurfaceEntry>,

    pub(crate) next_surface_id: u32,
    pub(crate) next_output_id: u32,
    pub(crate) next_seat_id: u32,

    pub(crate) pending_events: VecDeque<Event>,
    pub(crate) exit_requested: bool,
}

pub(crate) struct SeatEntry {
    /// Stable identity for this seat, independent of its position in the `Vec`.
    /// Used as the dispatch user-data so a seat removed mid-run never shifts the
    /// index another seat's events resolve through.
    pub(crate) id: u32,
    /// wl_registry `name`, retained so `global_remove` can find this seat.
    pub(crate) registry_name: u32,
    // Held to keep the seat object alive for the duration of the App; the
    // pointer below is the proxy we actually receive events on.
    pub(crate) seat: wl_seat::WlSeat,
    /// Negotiated `wl_seat` version.
    pub(crate) seat_version: u32,
    pub(crate) pointer: Option<wl_pointer::WlPointer>,
    /// Version-derived pointer capabilities, once a pointer is bound.
    pub(crate) pointer_caps: Option<PointerCaps>,
    /// Surface the pointer is currently over (raw id).
    pub(crate) focus: Option<u32>,
    /// Latest local pointer position (physical pixels).
    pub(crate) pos: (f64, f64),
    /// Events buffered between `enter`/.../`frame` markers (v5+ pointers only).
    pub(crate) frame_buffer: Vec<PointerEvent>,
    /// Scroll state accumulated across one `axis*`/`frame` batch.
    pub(crate) pending_axis: dispatch::PendingAxis,
}

impl SeatEntry {
    /// Release the pointer proxy if the version supports the destructor, then
    /// forget it. Called on capability loss and seat removal.
    pub(crate) fn drop_pointer(&mut self) {
        if let Some(p) = self.pointer.take() {
            if self.pointer_caps.is_some_and(|c| c.has_release()) {
                p.release();
            }
        }
        self.pointer_caps = None;
        self.focus = None;
        self.frame_buffer.clear();
        self.pending_axis = dispatch::PendingAxis::default();
    }
}

pub(crate) struct SurfaceEntry {
    pub(crate) id: SurfaceId,
    pub(crate) wl_surface: wl_surface::WlSurface,
    pub(crate) layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    pub(crate) requested: (u32, u32),
    pub(crate) configured: (u32, u32),
    pub(crate) scale: i32,
    pub(crate) entered: Vec<wl_output::WlOutput>,
    pub(crate) pool: Option<ShmPool>,
    pub(crate) ready: bool,
}

impl AppState {
    /// The best pixel format for the negotiated shm capabilities: the swap-free
    /// `Abgr8888` when advertised, else the mandatory `Argb8888`.
    pub(crate) fn pixel_format(&self) -> PixelFormat {
        PixelFormat::best(self.caps.abgr_supported)
    }

    pub(crate) fn surface_idx(&self, sid: u32) -> Option<usize> {
        self.surfaces.iter().position(|s| s.id.0 == sid)
    }

    /// Find a surface by its raw `wl_surface` object — used by pointer events
    /// where the compositor identifies the target by proxy identity.
    pub(crate) fn surface_idx_for_wl(&self, wl: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| &s.wl_surface == wl)
    }

    pub(crate) fn output_idx_for_object(&self, output: &wl_output::WlOutput) -> Option<usize> {
        self.outputs.iter().position(|o| &o.output == output)
    }

    pub(crate) fn refresh_surface_scale(&mut self, sidx: usize) {
        let mut scale = 1;
        for entered in &self.surfaces[sidx].entered {
            if let Some(oidx) = self.output_idx_for_object(entered) {
                scale = scale.max(self.outputs[oidx].info.scale);
            }
        }
        if self.surfaces[sidx].scale != scale {
            self.surfaces[sidx].scale = scale;
            if self.surfaces[sidx].ready {
                let (w, h) = self.surfaces[sidx].configured;
                let id = self.surfaces[sidx].id;
                self.pending_events.push_back(Event::Configure {
                    surface: id,
                    width: w,
                    height: h,
                    scale,
                });
            }
        }
    }

    /// Find a seat by its stable id.
    pub(crate) fn seat_idx(&self, seat_id: u32) -> Option<usize> {
        self.seats.iter().position(|s| s.id == seat_id)
    }

    /// Remove an output global: purge its proxy from every surface, recompute
    /// each affected surface's scale (which may enqueue a `Configure`), and only
    /// then emit the final `OutputRemoved`. Enqueues nothing if the output is
    /// unknown.
    pub(crate) fn remove_output(&mut self, registry_name: u32) {
        let Some(oidx) = self
            .outputs
            .iter()
            .position(|o| o.registry_name == registry_name)
        else {
            return;
        };
        let id = self.outputs[oidx].info.id;
        let proxy = self.outputs[oidx].output.clone();

        // Drop the output from the table first so scale recomputation can't
        // count the monitor that is going away.
        self.outputs.remove(oidx);

        // Purge the dangling proxy from every surface and refresh its scale.
        for sidx in 0..self.surfaces.len() {
            let before = self.surfaces[sidx].entered.len();
            self.surfaces[sidx].entered.retain(|o| o != &proxy);
            if self.surfaces[sidx].entered.len() != before {
                self.refresh_surface_scale(sidx);
            }
        }

        self.pending_events.push_back(Event::OutputRemoved(id));
    }

    /// Drop all queued state referring to a surface that is being destroyed:
    /// pending high-level events, and any seat that had focus or buffered
    /// pointer events for it.
    pub(crate) fn purge_surface(&mut self, sid: SurfaceId) {
        let raw = sid.0;
        self.pending_events
            .retain(|ev| !event_targets_surface(ev, sid));
        for seat in &mut self.seats {
            if seat.focus == Some(raw) {
                seat.focus = None;
            }
            seat.frame_buffer.retain(|pe| pe.surface != sid);
        }
    }

    /// Deliver one pointer event: buffer it for the next `frame` on v5+
    /// pointers, or emit it immediately on pre-frame pointers so nothing is
    /// stranded waiting for a `frame` that never comes.
    pub(crate) fn deliver_pointer(&mut self, seat_idx: usize, ev: PointerEvent) {
        let has_frame = self.seats[seat_idx]
            .pointer_caps
            .is_some_and(|c| c.has_frame());
        if has_frame {
            self.seats[seat_idx].frame_buffer.push(ev);
        } else {
            self.pending_events.push_back(Event::Pointer(ev));
        }
    }
}

/// Whether a high-level event is addressed to `sid` (used when purging a
/// destroyed surface's queued events).
fn event_targets_surface(ev: &Event, sid: SurfaceId) -> bool {
    match ev {
        Event::Configure { surface, .. }
        | Event::Frame { surface, .. }
        | Event::Closed { surface, .. } => *surface == sid,
        Event::Pointer(pe) => pe.surface == sid,
        _ => false,
    }
}

impl App {
    /// Connect to `$WAYLAND_DISPLAY`, bind required globals, and do two
    /// roundtrips so that initial output / seat events have all arrived.
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()?;
        let mut queue: EventQueue<AppState> = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());

        let mut state = AppState {
            qh: qh.clone(),
            compositor: None,
            shm: None,
            layer_shell: None,
            compositor_registry_name: None,
            shm_registry_name: None,
            layer_shell_registry_name: None,
            caps: Capabilities::default(),
            seats: Vec::new(),
            outputs: Vec::new(),
            surfaces: Vec::new(),
            next_surface_id: 1,
            next_output_id: 1,
            next_seat_id: 1,
            pending_events: VecDeque::new(),
            exit_requested: false,
        };

        queue.roundtrip(&mut state)?;
        // Second roundtrip so output `done` and seat capability events land.
        queue.roundtrip(&mut state)?;

        if state.compositor.is_none() {
            return Err(Error::MissingGlobal("wl_compositor"));
        }
        if state.shm.is_none() {
            return Err(Error::MissingGlobal("wl_shm"));
        }
        if state.layer_shell.is_none() {
            return Err(Error::MissingGlobal("zwlr_layer_shell_v1"));
        }
        // Two roundtrips have populated state.outputs and queued one
        // `OutputAdded` plus one `OutputChanged` per initial monitor; we leave
        // them in place so a user's event loop sees the same shape regardless
        // of whether an output existed at connect time or appeared later.
        Ok(Self { conn, queue, state })
    }

    pub fn outputs(&self) -> Vec<crate::OutputInfo> {
        self.state.outputs.iter().map(|o| o.info.clone()).collect()
    }

    /// Begin configuring a new layer-shell surface. Call `.build()` to commit.
    pub fn layer_surface(&mut self) -> LayerSurfaceBuilder<'_> {
        LayerSurfaceBuilder {
            app: self,
            namespace: "meander".into(),
            layer: Layer::Top,
            anchor: Anchor::empty(),
            width: 0,
            height: 0,
            exclusive_zone: 0,
            margin_top: 0,
            margin_right: 0,
            margin_bottom: 0,
            margin_left: 0,
            interactivity: KeyboardInteractivity::None,
            output: None,
        }
    }

    /// Get a mutable handle to a surface by id. Errors only if the id was
    /// destroyed.
    pub fn surface(&mut self, id: SurfaceId) -> SurfaceHandle<'_> {
        SurfaceHandle { app: self, id }
    }

    /// Flush queued protocol messages to the compositor.
    pub fn flush(&mut self) -> Result<()> {
        self.conn.flush()?;
        Ok(())
    }

    /// Drain protocol events already buffered locally. Non-blocking.
    pub fn dispatch_pending(&mut self) -> Result<usize> {
        Ok(self.queue.dispatch_pending(&mut self.state)?)
    }

    /// Pop the next high-level [`Event`] meander has surfaced, or `None` if
    /// the queue is empty.
    pub fn next_event(&mut self) -> Option<Event> {
        self.state.pending_events.pop_front()
    }

    /// True once a surface emitted [`Event::Closed`] and you called
    /// [`quit`](Self::quit), or you called `quit` directly.
    pub fn is_quit_requested(&self) -> bool {
        self.state.exit_requested
    }

    /// Mark the App as wanting to exit. Doesn't block; the next iteration of
    /// your own loop should observe `is_quit_requested` and break.
    pub fn quit(&mut self) {
        self.state.exit_requested = true;
    }

    /// Raw connection fd, for callers who want to integrate the Wayland
    /// socket into their own poll / select / epoll / calloop loop. The fd is
    /// owned by the Wayland connection; do not close it.
    pub fn connection_fd(&self) -> RawFd {
        self.conn.backend().poll_fd().as_raw_fd()
    }

    /// Flush, then block until either a Wayland event arrives or `timeout`
    /// elapses. After it returns, drain `next_event` to see what happened.
    ///
    /// Pass `None` for an indefinite wait.
    pub fn dispatch(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.queue.flush()?;
        // Drain anything already buffered first so we don't sleep on data we
        // already have.
        let drained = self.queue.dispatch_pending(&mut self.state)?;
        if drained > 0 || !self.state.pending_events.is_empty() {
            return Ok(());
        }
        // Wait on the socket.
        let read_guard = loop {
            match self.conn.prepare_read() {
                Some(g) => break g,
                None => {
                    self.queue.dispatch_pending(&mut self.state)?;
                }
            }
        };
        let fd = read_guard.connection_fd();
        let mut pfds = [PollFd::new(&fd, PollFlags::IN)];
        let ts = timeout.map(|d| Timespec {
            tv_sec: d.as_secs() as _,
            tv_nsec: d.subsec_nanos() as _,
        });
        match poll(&mut pfds, ts.as_ref()) {
            Ok(_) => {}
            // A signal interrupted the wait: treat it as a spurious wakeup.
            // Release the read intent (prepare_read pairs with exactly one
            // read/drop) and let the caller's loop come back around.
            Err(rustix::io::Errno::INTR) => {
                drop(read_guard);
                self.queue.dispatch_pending(&mut self.state)?;
                return Ok(());
            }
            Err(e) => {
                drop(read_guard);
                return Err(e.into());
            }
        }
        let revents = pfds[0].revents();
        if revents.intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP) {
            // read() honours the prepare_read contract and surfaces a hangup or
            // socket error as a WaylandError, which we propagate. ERR/HUP with
            // no IN still needs the read() so the disconnect is reported rather
            // than silently swallowed.
            read_guard.read()?;
        } else {
            // Timed out with no activity; release the read intent.
            drop(read_guard);
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Surface handle
// ---------------------------------------------------------------------------

/// Short-lived borrow of one surface registered with an [`App`].
pub struct SurfaceHandle<'a> {
    app: &'a mut App,
    id: SurfaceId,
}

impl<'a> SurfaceHandle<'a> {
    pub fn id(&self) -> SurfaceId {
        self.id
    }

    /// Logical size last configured by the compositor.
    pub fn size(&self) -> Result<(u32, u32)> {
        let s = self.entry()?;
        Ok(s.configured)
    }

    pub fn scale(&self) -> Result<i32> {
        Ok(self.entry()?.scale)
    }

    /// Draw the next frame into the back buffer, then attach and commit.
    ///
    /// The closure receives a `Canvas` sized to the surface's physical pixel
    /// dimensions (logical size × scale). When it returns, meander swaps R/B
    /// to ARGB byte order, attaches the buffer, damages it fully, and commits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotConfigured`] if the surface hasn't received an
    /// initial [`Event::Configure`](crate::Event::Configure) yet — wait for
    /// it before drawing.
    ///
    /// Returns [`Error::BuffersBusy`] when both of the surface's shm buffers
    /// are still held by the compositor (no client release has arrived for
    /// either since you last attached them). The recommended pattern is to
    /// leave your `needs_draw` flag set and try again on the next
    /// [`Event::Frame`](crate::Event::Frame) or after a short timer; meander
    /// will not silently overwrite a buffer the compositor may still be
    /// reading.
    pub fn draw<F: FnOnce(&mut Canvas<'_>)>(&mut self, f: F) -> Result<()> {
        let state = &mut self.app.state;
        let shm = state.shm.as_ref().ok_or(Error::MissingGlobal("wl_shm"))?;
        let qh = state.qh.clone();
        let idx = state
            .surfaces
            .iter()
            .position(|s| s.id == self.id)
            .ok_or(Error::NoSuchSurface(self.id))?;
        if !state.surfaces[idx].ready {
            return Err(Error::NotConfigured);
        }
        let (logical_w, logical_h) = state.surfaces[idx].configured;
        let scale = state.surfaces[idx].scale.max(1);
        // Validate the geometry *before* any allocation or protocol request:
        // arbitrary compositor-supplied dimensions cannot reach memfd/mmap or a
        // lossy i32 cast without passing this one checked constructor.
        let layout = BufferLayout::new(logical_w, logical_h, scale, crate::shm::BUFFER_COUNT)?;
        let format = state.pixel_format();

        // Replace the pool if geometry or format changed.
        let needs_new_pool = match &state.surfaces[idx].pool {
            None => true,
            Some(p) => p.layout != layout || p.format != format,
        };
        if needs_new_pool {
            state.surfaces[idx].pool = Some(ShmPool::new(shm, layout, format, &qh)?);
        }

        // Clone the wl_surface (Arc-backed) so we can keep a handle to it
        // independent of the mutable borrow we're about to take on the pool.
        let surface = state.surfaces[idx].wl_surface.clone();
        let can_set_buffer_scale = state.caps.supports_set_buffer_scale();
        let can_damage_buffer = state.caps.supports_damage_buffer();
        let pool = state.surfaces[idx]
            .pool
            .as_mut()
            .expect("pool just ensured above");
        // If the compositor still owns both buffers we must not scribble over
        // either one — that's a racy write to memory the server is reading.
        // Surface the busy state to the caller; the next Frame / buffer Release
        // event will free a slot.
        let buf_idx = pool.pick_free().ok_or(Error::BuffersBusy)?;
        let pixels = pool.pixels_mut(buf_idx);

        {
            // The buffer is reused and may hold the previous frame. Rather than
            // unconditionally clearing it (and then usually overwriting it with
            // a full fill), the canvas clears lazily: a leading `fill` replaces
            // the clear, and any partial draw — or an empty frame — falls back
            // to a transparent clear via `ensure_cleared`.
            let pixmap = PixmapMut::from_bytes(pixels, layout.width, layout.height)
                .expect("pixmap dimensions are non-zero and fit by construction");
            let mut canvas = Canvas::new_surface(pixmap, scale);
            f(&mut canvas);
            canvas.ensure_cleared();
        }

        // Only swap R/B when the negotiated format is BGRA byte order.
        if pool.format.needs_rb_swap() {
            ShmPool::rgba_to_bgra(pool.pixels_mut(buf_idx));
        }

        // `set_buffer_scale` and `damage_buffer` are wl_compositor v3/v4; fall
        // back to surface-local `damage` (logical coordinates) on older servers.
        if can_set_buffer_scale {
            surface.set_buffer_scale(scale);
        }
        surface.attach(Some(&pool.buffers[buf_idx].wl), 0, 0);
        if can_damage_buffer {
            surface.damage_buffer(0, 0, layout.width as i32, layout.height as i32);
        } else {
            surface.damage(0, 0, logical_w as i32, logical_h as i32);
        }
        surface.commit();
        pool.buffers[buf_idx].in_use = true;
        self.app.conn.flush()?;
        Ok(())
    }

    /// Ask the compositor to call us back when the next frame is presented.
    /// Drives Event::Frame.
    pub fn request_frame(&mut self) -> Result<()> {
        let state = &mut self.app.state;
        let qh = state.qh.clone();
        let entry = state
            .surfaces
            .iter()
            .find(|s| s.id == self.id)
            .ok_or(Error::NoSuchSurface(self.id))?;
        let raw = self.id.0;
        let _cb = entry.wl_surface.frame(&qh, raw);
        entry.wl_surface.commit();
        Ok(())
    }

    /// Destroy the surface and its associated layer-shell object.
    pub fn destroy(self) -> Result<()> {
        let state = &mut self.app.state;
        let idx = state
            .surfaces
            .iter()
            .position(|s| s.id == self.id)
            .ok_or(Error::NoSuchSurface(self.id))?;
        let entry = state.surfaces.remove(idx);
        // Drop any queued events / seat focus that still reference this surface
        // so the caller never sees an event for an id it just destroyed.
        state.purge_surface(self.id);
        // Buffers/pool drop first so destroys go out in order.
        drop(entry.pool);
        entry.layer.destroy();
        entry.wl_surface.destroy();
        self.app.conn.flush()?;
        Ok(())
    }

    fn entry(&self) -> Result<&SurfaceEntry> {
        self.app
            .state
            .surfaces
            .iter()
            .find(|s| s.id == self.id)
            .ok_or(Error::NoSuchSurface(self.id))
    }
}

// ---------------------------------------------------------------------------
// Surface builder bridge
// ---------------------------------------------------------------------------

/// Validate a layer-surface request against the layer-shell protocol rules and
/// the negotiated capabilities. Pure — no protocol objects are touched, so an
/// invalid request produces a typed error before anything is allocated.
pub(crate) fn validate_layer_request(
    width: u32,
    height: u32,
    anchor: Anchor,
    exclusive_zone: i32,
    interactivity: KeyboardInteractivity,
    caps: &Capabilities,
) -> Result<()> {
    // Zero in a dimension asks the compositor to derive it from the anchors,
    // which is only well-defined when the surface is anchored to both opposite
    // edges. The compositor would otherwise raise an `invalid_size` protocol
    // error and kill the connection; reject locally instead.
    if width == 0 && !(anchor.contains(Anchor::LEFT) && anchor.contains(Anchor::RIGHT)) {
        return Err(Error::InvalidArgument(
            "width 0 requires the surface to be anchored to both LEFT and RIGHT",
        ));
    }
    if height == 0 && !(anchor.contains(Anchor::TOP) && anchor.contains(Anchor::BOTTOM)) {
        return Err(Error::InvalidArgument(
            "height 0 requires the surface to be anchored to both TOP and BOTTOM",
        ));
    }
    // `set_exclusive_zone` accepts -1 (don't reserve, allow overlap) or any
    // non-negative reservation. Anything below -1 is invalid.
    if exclusive_zone < -1 {
        return Err(Error::InvalidArgument(
            "exclusive zone must be -1 or a non-negative pixel count",
        ));
    }
    // `on_demand` keyboard interactivity is a layer-shell v4 addition.
    if !caps.supports_keyboard_mode(interactivity) {
        return Err(Error::UnsupportedProtocolVersion {
            interface: "zwlr_layer_shell_v1",
            advertised: caps.layer_shell_version,
            required: 4,
        });
    }
    Ok(())
}

pub(crate) fn build_layer_surface(b: LayerSurfaceBuilder<'_>) -> Result<SurfaceId> {
    let app = b.app;
    let state = &mut app.state;

    // --- Validate everything up front, before touching a protocol object. ---
    validate_layer_request(
        b.width,
        b.height,
        b.anchor,
        b.exclusive_zone,
        b.interactivity,
        &state.caps,
    )?;

    let compositor = state
        .compositor
        .as_ref()
        .ok_or(Error::MissingGlobal("wl_compositor"))?
        .clone();
    let layer_shell = state
        .layer_shell
        .as_ref()
        .ok_or(Error::MissingGlobal("zwlr_layer_shell_v1"))?
        .clone();
    // Resolve the requested output *before* creating any objects, so an unknown
    // output cannot leave a half-created surface behind.
    let output_ref = match b.output {
        None => None,
        Some(oid) => Some(
            state
                .outputs
                .iter()
                .find(|o| o.info.id == oid)
                .ok_or(Error::UnknownOutput(oid))?
                .output
                .clone(),
        ),
    };

    // --- All validation passed: reserve an id and create the objects. ---
    let id_u32 = state.next_surface_id;
    let id = SurfaceId(id_u32);

    let wl_surface = compositor.create_surface(&state.qh, id_u32);
    let layer_surface = layer_shell.get_layer_surface(
        &wl_surface,
        output_ref.as_ref(),
        dispatch::map_layer(b.layer),
        b.namespace.clone(),
        &state.qh,
        id_u32,
    );
    layer_surface.set_anchor(dispatch::map_anchor(b.anchor));
    layer_surface.set_exclusive_zone(b.exclusive_zone);
    layer_surface.set_margin(b.margin_top, b.margin_right, b.margin_bottom, b.margin_left);
    layer_surface.set_keyboard_interactivity(dispatch::map_keyboard(b.interactivity));
    layer_surface.set_size(b.width, b.height);
    wl_surface.commit();

    // Flush before recording state so a transport failure rolls the objects
    // back instead of leaving a state entry that references dead proxies.
    if let Err(e) = app.conn.flush() {
        layer_surface.destroy();
        wl_surface.destroy();
        // Best-effort: try to push the destructors out; ignore a second error.
        let _ = app.conn.flush();
        return Err(e.into());
    }

    // Commit the id bump and state entry only now that the objects are live.
    state.next_surface_id += 1;
    state.surfaces.push(SurfaceEntry {
        id,
        wl_surface,
        layer: layer_surface,
        requested: (b.width, b.height),
        configured: (b.width, b.height),
        scale: 1,
        entered: Vec::new(),
        pool: None,
        ready: false,
    });
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_v4() -> Capabilities {
        Capabilities {
            compositor_version: 4,
            shm_version: 1,
            layer_shell_version: 4,
            abgr_supported: false,
        }
    }

    fn validate(
        w: u32,
        h: u32,
        anchor: Anchor,
        zone: i32,
        kb: KeyboardInteractivity,
        caps: &Capabilities,
    ) -> Result<()> {
        validate_layer_request(w, h, anchor, zone, kb, caps)
    }

    #[test]
    fn top_strip_bar_is_valid() {
        // width 0 anchored LEFT|RIGHT, fixed height — the canonical bar.
        assert!(validate(
            0,
            28,
            Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            28,
            KeyboardInteractivity::None,
            &caps_v4(),
        )
        .is_ok());
    }

    #[test]
    fn zero_width_without_both_horizontal_anchors_is_rejected() {
        let r = validate(
            0,
            28,
            Anchor::TOP | Anchor::LEFT,
            0,
            KeyboardInteractivity::None,
            &caps_v4(),
        );
        assert!(matches!(r, Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn zero_height_without_both_vertical_anchors_is_rejected() {
        let r = validate(
            200,
            0,
            Anchor::LEFT | Anchor::TOP,
            0,
            KeyboardInteractivity::None,
            &caps_v4(),
        );
        assert!(matches!(r, Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn full_screen_overlay_with_zero_size_and_all_anchors_is_valid() {
        assert!(validate(
            0,
            0,
            Anchor::ALL,
            -1,
            KeyboardInteractivity::None,
            &caps_v4(),
        )
        .is_ok());
    }

    #[test]
    fn exclusive_zone_minus_one_is_valid() {
        assert!(validate(
            100,
            100,
            Anchor::empty(),
            -1,
            KeyboardInteractivity::None,
            &caps_v4(),
        )
        .is_ok());
    }

    #[test]
    fn exclusive_zone_below_minus_one_is_rejected() {
        let r = validate(
            100,
            100,
            Anchor::empty(),
            -2,
            KeyboardInteractivity::None,
            &caps_v4(),
        );
        assert!(matches!(r, Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn on_demand_keyboard_rejected_on_layer_shell_v3() {
        let caps = Capabilities {
            layer_shell_version: 3,
            ..caps_v4()
        };
        let r = validate(
            100,
            100,
            Anchor::empty(),
            0,
            KeyboardInteractivity::OnDemand,
            &caps,
        );
        assert!(matches!(
            r,
            Err(Error::UnsupportedProtocolVersion {
                interface: "zwlr_layer_shell_v1",
                required: 4,
                ..
            })
        ));
    }

    #[test]
    fn on_demand_keyboard_accepted_on_layer_shell_v4() {
        assert!(validate(
            100,
            100,
            Anchor::empty(),
            0,
            KeyboardInteractivity::OnDemand,
            &caps_v4(),
        )
        .is_ok());
    }

    #[test]
    fn non_zero_size_needs_no_anchors() {
        assert!(validate(
            200,
            50,
            Anchor::empty(),
            0,
            KeyboardInteractivity::None,
            &caps_v4(),
        )
        .is_ok());
    }
}

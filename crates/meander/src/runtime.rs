//! The wayland runtime: connection, dispatch impls, event-loop helpers.
//!
//! Everything that touches the wire is concentrated here so the rest of the
//! crate can stay agnostic about Wayland object lifetimes.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use tiny_skia::PixmapMut;
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::canvas::Canvas;
use crate::error::{Error, Result};
use crate::event::{CloseReason, Event};
use crate::input::{Axis, PointerButton, PointerEvent, PointerEventKind};
use crate::output::{map_subpixel, map_transform, OutputEntry, OutputId};
use crate::shm::ShmPool;
use crate::surface::{
    Anchor, KeyboardInteractivity, Layer, LayerSurfaceBuilder, SurfaceId,
};

/// Top-level handle to the Wayland connection and all the surfaces meander is
/// managing for you. Owns the event queue, the shm globals, the seats, and
/// the output table.
pub struct App {
    conn: Connection,
    queue: EventQueue<AppState>,
    state: AppState,
}

pub(crate) struct AppState {
    qh: QueueHandle<AppState>,

    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,

    seats: Vec<SeatEntry>,
    outputs: Vec<OutputEntry>,
    surfaces: Vec<SurfaceEntry>,

    next_surface_id: u32,
    next_output_id: u32,

    pending_events: VecDeque<Event>,
    exit_requested: bool,
}

struct SeatEntry {
    // Held to keep the seat object alive for the duration of the App; the
    // pointer below is the proxy we actually receive events on.
    #[allow(dead_code)]
    seat: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    /// Surface the pointer is currently over (raw id).
    focus: Option<u32>,
    /// Latest local pointer position (physical pixels).
    pos: (f64, f64),
    /// Events buffered between `enter`/.../`frame` markers.
    frame_buffer: Vec<PointerEvent>,
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
            seats: Vec::new(),
            outputs: Vec::new(),
            surfaces: Vec::new(),
            next_surface_id: 1,
            next_output_id: 1,
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
        let _n = poll(&mut pfds, ts.as_ref())?;
        if pfds[0].revents().contains(PollFlags::IN) {
            read_guard.read()?;
        } else {
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
        let buf_w = logical_w * scale as u32;
        let buf_h = logical_h * scale as u32;

        // Replace the pool if size mismatches.
        let needs_new_pool = match &state.surfaces[idx].pool {
            None => true,
            Some(p) => p.width != buf_w || p.height != buf_h,
        };
        if needs_new_pool {
            state.surfaces[idx].pool = Some(ShmPool::new(shm, buf_w, buf_h, &qh)?);
        }

        // Clone the wl_surface (Arc-backed) so we can keep a handle to it
        // independent of the mutable borrow we're about to take on the pool.
        let surface = state.surfaces[idx].wl_surface.clone();
        let pool = state.surfaces[idx].pool.as_mut().unwrap();
        let buf_idx = pool.pick_free().unwrap_or(0);
        let pixels = pool.pixels_mut(buf_idx);
        pixels.fill(0);

        {
            let pixmap = PixmapMut::from_bytes(pixels, buf_w, buf_h)
                .expect("pixmap dimensions are non-zero by construction");
            let mut canvas = Canvas { pixmap, scale };
            f(&mut canvas);
        }

        ShmPool::rgba_to_bgra(pool.pixels_mut(buf_idx));

        surface.set_buffer_scale(scale);
        surface.attach(Some(&pool.buffers[buf_idx].wl), 0, 0);
        surface.damage_buffer(0, 0, buf_w as i32, buf_h as i32);
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

pub(crate) fn build_layer_surface(b: LayerSurfaceBuilder<'_>) -> Result<SurfaceId> {
    let app = b.app;
    let state = &mut app.state;
    let compositor = state
        .compositor
        .as_ref()
        .ok_or(Error::MissingGlobal("wl_compositor"))?;
    let layer_shell = state
        .layer_shell
        .as_ref()
        .ok_or(Error::MissingGlobal("zwlr_layer_shell_v1"))?;

    let id_u32 = state.next_surface_id;
    state.next_surface_id += 1;
    let id = SurfaceId(id_u32);

    let wl_surface = compositor.create_surface(&state.qh, id_u32);
    let output_ref = b
        .output
        .and_then(|oid| state.outputs.iter().find(|o| o.info.id == oid))
        .map(|o| o.output.clone());

    let layer_surface = layer_shell.get_layer_surface(
        &wl_surface,
        output_ref.as_ref(),
        map_layer(b.layer),
        b.namespace.clone(),
        &state.qh,
        id_u32,
    );
    layer_surface.set_anchor(map_anchor(b.anchor));
    layer_surface.set_exclusive_zone(b.exclusive_zone);
    layer_surface.set_margin(b.margin_top, b.margin_right, b.margin_bottom, b.margin_left);
    layer_surface.set_keyboard_interactivity(map_keyboard(b.interactivity));
    layer_surface.set_size(b.width, b.height);
    wl_surface.commit();

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
    app.conn.flush()?;
    Ok(id)
}

fn map_layer(l: Layer) -> zwlr_layer_shell_v1::Layer {
    match l {
        Layer::Background => zwlr_layer_shell_v1::Layer::Background,
        Layer::Bottom => zwlr_layer_shell_v1::Layer::Bottom,
        Layer::Top => zwlr_layer_shell_v1::Layer::Top,
        Layer::Overlay => zwlr_layer_shell_v1::Layer::Overlay,
    }
}

fn map_anchor(a: Anchor) -> zwlr_layer_surface_v1::Anchor {
    let mut out = zwlr_layer_surface_v1::Anchor::empty();
    if a.contains(Anchor::TOP) {
        out |= zwlr_layer_surface_v1::Anchor::Top;
    }
    if a.contains(Anchor::BOTTOM) {
        out |= zwlr_layer_surface_v1::Anchor::Bottom;
    }
    if a.contains(Anchor::LEFT) {
        out |= zwlr_layer_surface_v1::Anchor::Left;
    }
    if a.contains(Anchor::RIGHT) {
        out |= zwlr_layer_surface_v1::Anchor::Right;
    }
    out
}

fn map_keyboard(k: KeyboardInteractivity) -> zwlr_layer_surface_v1::KeyboardInteractivity {
    match k {
        KeyboardInteractivity::None => zwlr_layer_surface_v1::KeyboardInteractivity::None,
        KeyboardInteractivity::Exclusive => zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
        KeyboardInteractivity::OnDemand => zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    }
}

// ---------------------------------------------------------------------------
// Dispatch impls
// ---------------------------------------------------------------------------

impl AppState {
    fn surface_idx(&self, sid: u32) -> Option<usize> {
        self.surfaces.iter().position(|s| s.id.0 == sid)
    }

    fn output_idx_for_object(&self, output: &wl_output::WlOutput) -> Option<usize> {
        self.outputs.iter().position(|o| &o.output == output)
    }

    fn refresh_surface_scale(&mut self, sidx: usize) {
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
                self.pending_events
                    .push_back(Event::Configure { surface: id, width: w, height: h, scale });
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                match interface.as_str() {
                    "wl_compositor" => {
                        let v = version.min(4);
                        state.compositor = Some(
                            registry.bind::<wl_compositor::WlCompositor, _, _>(name, v, qh, ()),
                        );
                    }
                    "wl_shm" => {
                        let v = version.min(1);
                        state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, v, qh, ()));
                    }
                    "zwlr_layer_shell_v1" => {
                        let v = version.min(4);
                        state.layer_shell = Some(registry.bind::<
                            zwlr_layer_shell_v1::ZwlrLayerShellV1,
                            _,
                            _,
                        >(name, v, qh, ()));
                    }
                    "wl_seat" => {
                        let v = version.min(7);
                        let idx = state.seats.len() as u32;
                        let seat =
                            registry.bind::<wl_seat::WlSeat, _, _>(name, v, qh, idx);
                        state.seats.push(SeatEntry {
                            seat,
                            pointer: None,
                            focus: None,
                            pos: (0.0, 0.0),
                            frame_buffer: Vec::new(),
                        });
                    }
                    "wl_output" => {
                        let v = version.min(4);
                        let id_u32 = state.next_output_id;
                        state.next_output_id += 1;
                        let id = OutputId(id_u32);
                        let output =
                            registry.bind::<wl_output::WlOutput, _, _>(name, v, qh, id_u32);
                        state.outputs.push(OutputEntry::new(output, id, name));
                        state.pending_events.push_back(Event::OutputAdded(id));
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(idx) =
                    state.outputs.iter().position(|o| o.registry_name == name)
                {
                    let id = state.outputs[idx].info.id;
                    state.outputs.remove(idx);
                    state.pending_events.push_back(Event::OutputRemoved(id));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for AppState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _id_u32: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(idx) = state.output_idx_for_object(output) else { return };
        let mut changed = false;
        match event {
            wl_output::Event::Geometry {
                x,
                y,
                physical_width,
                physical_height,
                subpixel,
                transform,
                ..
            } => {
                let info = &mut state.outputs[idx].info;
                info.position = (x, y);
                info.physical_size_mm = (physical_width, physical_height);
                if let WEnum::Value(s) = subpixel {
                    info.subpixel = map_subpixel(s);
                }
                if let WEnum::Value(t) = transform {
                    info.transform = map_transform(t);
                }
                changed = true;
            }
            wl_output::Event::Mode { flags, width, height, refresh, .. } => {
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                    state.outputs[idx].info.mode = Some((width, height, refresh));
                    changed = true;
                }
            }
            wl_output::Event::Scale { factor } => {
                state.outputs[idx].info.scale = factor;
                changed = true;
            }
            wl_output::Event::Name { name } => {
                state.outputs[idx].info.name = Some(name);
                changed = true;
            }
            wl_output::Event::Description { description } => {
                state.outputs[idx].info.description = Some(description);
                changed = true;
            }
            wl_output::Event::Done => {
                let id = state.outputs[idx].info.id;
                state.pending_events.push_back(Event::OutputChanged(id));
                // Output scale may have changed; refresh all surfaces that
                // sit on this output.
                let surface_count = state.surfaces.len();
                for sidx in 0..surface_count {
                    let entered = state.surfaces[sidx]
                        .entered
                        .iter()
                        .any(|e| Some(idx) == state.output_idx_for_object(e));
                    if entered {
                        state.refresh_surface_scale(sidx);
                    }
                }
                return;
            }
            _ => {}
        }
        if changed {
            // Defer emitting OutputChanged until Done.
        }
    }
}

impl Dispatch<wl_surface::WlSurface, u32> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        sid_u32: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(sidx) = state.surface_idx(*sid_u32) else { return };
        match event {
            wl_surface::Event::Enter { output } => {
                state.surfaces[sidx].entered.push(output);
                state.refresh_surface_scale(sidx);
            }
            wl_surface::Event::Leave { output } => {
                state.surfaces[sidx].entered.retain(|o| o != &output);
                state.refresh_surface_scale(sidx);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u32> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        sid_u32: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { callback_data } = event {
            if let Some(sidx) = state.surface_idx(*sid_u32) {
                let id = state.surfaces[sidx].id;
                state.pending_events.push_back(Event::Frame {
                    surface: id,
                    time_ms: callback_data,
                });
            }
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(
        state: &mut Self,
        buf: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            for s in &mut state.surfaces {
                if let Some(pool) = s.pool.as_mut() {
                    if pool.release(buf) {
                        return;
                    }
                }
            }
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for AppState {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        sid_u32: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(sidx) = state.surface_idx(*sid_u32) else { return };
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                let surface = &mut state.surfaces[sidx];
                // Width or height of 0 means "client picks". If the user
                // requested a non-zero dimension, honour it; otherwise fall
                // back to the compositor-supplied dimension.
                let final_w = if width == 0 { surface.requested.0 } else { width };
                let final_h = if height == 0 { surface.requested.1 } else { height };
                let final_w = if final_w == 0 { 1 } else { final_w };
                let final_h = if final_h == 0 { 1 } else { final_h };
                surface.configured = (final_w, final_h);
                surface.ready = true;
                let id = surface.id;
                let scale = surface.scale;
                state.pending_events.push_back(Event::Configure {
                    surface: id,
                    width: final_w,
                    height: final_h,
                    scale,
                });
            }
            zwlr_layer_surface_v1::Event::Closed => {
                let id = state.surfaces[sidx].id;
                state.pending_events.push_back(Event::Closed {
                    surface: id,
                    reason: CloseReason::Compositor,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for AppState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        seat_idx: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let idx = *seat_idx as usize;
        if idx >= state.seats.len() {
            return;
        }
        match event {
            wl_seat::Event::Capabilities { capabilities } => {
                let caps = match capabilities {
                    WEnum::Value(v) => v,
                    _ => return,
                };
                let want_pointer = caps.contains(wl_seat::Capability::Pointer);
                let has_pointer = state.seats[idx].pointer.is_some();
                if want_pointer && !has_pointer {
                    let p = seat.get_pointer(qh, *seat_idx);
                    state.seats[idx].pointer = Some(p);
                }
                if !want_pointer {
                    if let Some(p) = state.seats[idx].pointer.take() {
                        p.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, u32> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        seat_idx: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idx = *seat_idx as usize;
        if idx >= state.seats.len() {
            return;
        }
        match event {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => {
                if let Some(sidx) = state
                    .surfaces
                    .iter()
                    .position(|s| s.wl_surface == surface)
                {
                    let sid = state.surfaces[sidx].id;
                    let scale = state.surfaces[sidx].scale as f64;
                    state.seats[idx].focus = Some(sid.0);
                    state.seats[idx].pos = (surface_x * scale, surface_y * scale);
                    state.seats[idx].frame_buffer.push(PointerEvent {
                        surface: sid,
                        kind: PointerEventKind::Enter,
                        x: surface_x * scale,
                        y: surface_y * scale,
                    });
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if let Some(sidx) = state
                    .surfaces
                    .iter()
                    .position(|s| s.wl_surface == surface)
                {
                    let sid = state.surfaces[sidx].id;
                    let (x, y) = state.seats[idx].pos;
                    state.seats[idx].frame_buffer.push(PointerEvent {
                        surface: sid,
                        kind: PointerEventKind::Leave,
                        x,
                        y,
                    });
                    state.seats[idx].focus = None;
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                let Some(sid_u32) = state.seats[idx].focus else { return };
                let Some(sidx) = state.surface_idx(sid_u32) else { return };
                let scale = state.surfaces[sidx].scale as f64;
                let x = surface_x * scale;
                let y = surface_y * scale;
                state.seats[idx].pos = (x, y);
                state.seats[idx].frame_buffer.push(PointerEvent {
                    surface: SurfaceId(sid_u32),
                    kind: PointerEventKind::Motion,
                    x,
                    y,
                });
            }
            wl_pointer::Event::Button { button, state: bs, .. } => {
                let Some(sid_u32) = state.seats[idx].focus else { return };
                let b = PointerButton::from_code(button);
                let kind = match bs {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => PointerEventKind::Press(b),
                    WEnum::Value(wl_pointer::ButtonState::Released) => {
                        PointerEventKind::Release(b)
                    }
                    _ => return,
                };
                let (x, y) = state.seats[idx].pos;
                state.seats[idx].frame_buffer.push(PointerEvent {
                    surface: SurfaceId(sid_u32),
                    kind,
                    x,
                    y,
                });
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let Some(sid_u32) = state.seats[idx].focus else { return };
                let axis = match axis {
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => Axis::Vertical(value),
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => Axis::Horizontal(value),
                    _ => return,
                };
                let (x, y) = state.seats[idx].pos;
                state.seats[idx].frame_buffer.push(PointerEvent {
                    surface: SurfaceId(sid_u32),
                    kind: PointerEventKind::Scroll(axis),
                    x,
                    y,
                });
            }
            wl_pointer::Event::Frame => {
                let drained: Vec<PointerEvent> = state.seats[idx].frame_buffer.drain(..).collect();
                for ev in drained {
                    state.pending_events.push_back(Event::Pointer(ev));
                }
            }
            _ => {}
        }
    }
}

// Globals with no client-bound events.
delegate_noop!(AppState: wl_compositor::WlCompositor);
delegate_noop!(AppState: wl_shm_pool::WlShmPool);
delegate_noop!(AppState: ignore wl_shm::WlShm);
delegate_noop!(AppState: zwlr_layer_shell_v1::ZwlrLayerShellV1);

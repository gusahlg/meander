//! Wayland-client `Dispatch` glue.
//!
//! One impl per protocol object. The bodies are deliberately mechanical: they
//! translate wire events into mutations on `AppState` and enqueue
//! [`Event`](crate::Event)s for the user's `next_event` loop.

use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::event::{CloseReason, Event};
use crate::input::{
    PointerButton, PointerEvent, PointerEventKind, Scroll, ScrollAxis, ScrollSource,
};
use crate::output::{map_subpixel, map_transform, OutputEntry, OutputId};
use crate::surface::{Anchor, KeyboardInteractivity, Layer, SurfaceId};

use super::caps::PointerCaps;
use super::{caps, AppState, SeatEntry};

// ---------------------------------------------------------------------------
// Public-shape → wayland enum mapping
// ---------------------------------------------------------------------------

pub(super) fn map_layer(l: Layer) -> zwlr_layer_shell_v1::Layer {
    match l {
        Layer::Background => zwlr_layer_shell_v1::Layer::Background,
        Layer::Bottom => zwlr_layer_shell_v1::Layer::Bottom,
        Layer::Top => zwlr_layer_shell_v1::Layer::Top,
        Layer::Overlay => zwlr_layer_shell_v1::Layer::Overlay,
    }
}

pub(super) fn map_anchor(a: Anchor) -> zwlr_layer_surface_v1::Anchor {
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

pub(super) fn map_keyboard(
    k: KeyboardInteractivity,
) -> zwlr_layer_surface_v1::KeyboardInteractivity {
    match k {
        KeyboardInteractivity::None => zwlr_layer_surface_v1::KeyboardInteractivity::None,
        KeyboardInteractivity::Exclusive => zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
        KeyboardInteractivity::OnDemand => zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand,
    }
}

// ---------------------------------------------------------------------------
// Dispatch impls
// ---------------------------------------------------------------------------

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
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => {
                    let v = version.min(caps::MAX_COMPOSITOR_VERSION);
                    state.caps.compositor_version = v;
                    state.compositor_registry_name = Some(name);
                    state.compositor =
                        Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, v, qh, ()));
                }
                "wl_shm" => {
                    let v = version.min(caps::MAX_SHM_VERSION);
                    state.caps.shm_version = v;
                    state.shm_registry_name = Some(name);
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, v, qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    let v = version.min(caps::MAX_LAYER_SHELL_VERSION);
                    state.caps.layer_shell_version = v;
                    state.layer_shell_registry_name = Some(name);
                    state.layer_shell = Some(
                        registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                            name,
                            v,
                            qh,
                            (),
                        ),
                    );
                }
                "wl_seat" => {
                    let v = version.min(caps::MAX_SEAT_VERSION);
                    // Bind with a *stable* id as user-data, not the current Vec
                    // index: a seat removed later must not renumber the events of
                    // the seats that outlive it.
                    let id = state.next_seat_id;
                    state.next_seat_id += 1;
                    let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, v, qh, id);
                    state.seats.push(SeatEntry {
                        id,
                        registry_name: name,
                        seat,
                        seat_version: v,
                        pointer: None,
                        pointer_caps: None,
                        focus: None,
                        pos: (0.0, 0.0),
                        frame_buffer: Vec::new(),
                        pending_axis: PendingAxis::default(),
                    });
                }
                "wl_output" => {
                    let v = version.min(4);
                    let id_u32 = state.next_output_id;
                    state.next_output_id += 1;
                    let id = OutputId(id_u32);
                    let output = registry.bind::<wl_output::WlOutput, _, _>(name, v, qh, id_u32);
                    state.outputs.push(OutputEntry::new(output, id, name));
                    state.pending_events.push_back(Event::OutputAdded(id));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                // Output: purge from surfaces, recompute scale, then emit
                // OutputRemoved (all handled by remove_output).
                if state.outputs.iter().any(|o| o.registry_name == name) {
                    state.remove_output(name);
                    return;
                }
                // Seat: release its pointer and drop it. The stable seat id
                // means no other seat's events are disturbed.
                if let Some(sidx) = state.seats.iter().position(|s| s.registry_name == name) {
                    let mut seat = state.seats.remove(sidx);
                    seat.drop_pointer();
                    // Best-effort destructor for the seat proxy itself (v5+).
                    if seat.seat_version >= 5 {
                        seat.seat.release();
                    }
                    return;
                }
                // A required singleton going away leaves the connection unable
                // to service surfaces; surface it as a typed event so the app
                // can tear down rather than spin on dead proxies.
                if state.compositor_registry_name == Some(name) {
                    state.compositor = None;
                    state.compositor_registry_name = None;
                    state
                        .pending_events
                        .push_back(Event::GlobalLost("wl_compositor"));
                } else if state.shm_registry_name == Some(name) {
                    state.shm = None;
                    state.shm_registry_name = None;
                    state.pending_events.push_back(Event::GlobalLost("wl_shm"));
                } else if state.layer_shell_registry_name == Some(name) {
                    state.layer_shell = None;
                    state.layer_shell_registry_name = None;
                    state
                        .pending_events
                        .push_back(Event::GlobalLost("zwlr_layer_shell_v1"));
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
        let Some(idx) = state.output_idx_for_object(output) else {
            return;
        };
        // Properties accumulate silently across this batch; we surface them as
        // a single `OutputChanged` only when `Done` arrives, matching the
        // atomic-update contract in the wl_output protocol.
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
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
                ..
            } => {
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                    state.outputs[idx].info.mode = Some((width, height, refresh));
                }
            }
            wl_output::Event::Scale { factor } => {
                state.outputs[idx].info.scale = factor;
            }
            wl_output::Event::Name { name } => {
                state.outputs[idx].info.name = Some(name);
            }
            wl_output::Event::Description { description } => {
                state.outputs[idx].info.description = Some(description);
            }
            wl_output::Event::Done => {
                let id = state.outputs[idx].info.id;
                state.pending_events.push_back(Event::OutputChanged(id));
                // Output scale may have changed; refresh all surfaces that
                // sit on this output.
                let this_output = state.outputs[idx].output.clone();
                let surface_count = state.surfaces.len();
                for sidx in 0..surface_count {
                    let entered = state.surfaces[sidx]
                        .entered
                        .iter()
                        .any(|e| e == &this_output);
                    if entered {
                        state.refresh_surface_scale(sidx);
                    }
                }
            }
            _ => {}
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
        let Some(sidx) = state.surface_idx(*sid_u32) else {
            return;
        };
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
        let Some(sidx) = state.surface_idx(*sid_u32) else {
            return;
        };
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                let surface = &mut state.surfaces[sidx];
                // Width or height of 0 means "client picks". If the user
                // requested a non-zero dimension, honour it; otherwise fall
                // back to the compositor-supplied dimension.
                let final_w = if width == 0 {
                    surface.requested.0
                } else {
                    width
                };
                let final_h = if height == 0 {
                    surface.requested.1
                } else {
                    height
                };
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
        seat_id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(idx) = state.seat_idx(*seat_id) else {
            return;
        };
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let WEnum::Value(caps) = capabilities else {
                return;
            };
            let want_pointer = caps.contains(wl_seat::Capability::Pointer);
            let has_pointer = state.seats[idx].pointer.is_some();
            if want_pointer && !has_pointer {
                // The pointer inherits the seat's negotiated version.
                let version = state.seats[idx].seat_version;
                let p = seat.get_pointer(qh, *seat_id);
                state.seats[idx].pointer = Some(p);
                state.seats[idx].pointer_caps = Some(PointerCaps::new(version));
            }
            if !want_pointer && has_pointer {
                // Capability withdrawn: release the pointer and clear its focus
                // and any half-collected frame/axis state.
                state.seats[idx].drop_pointer();
            }
        }
    }
}

/// Scroll state accumulated across one `wl_pointer` `axis*`/`frame` batch.
///
/// Wayland splits a single logical scroll into several events (`axis_source`,
/// `axis`, `axis_discrete`/`axis_value120`, `axis_stop`) terminated by `frame`.
/// We gather them per axis and materialise one [`Scroll`] per axis at `frame`,
/// so the continuous `value` and the discrete step count are never conflated.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PendingAxis {
    source: Option<ScrollSource>,
    vertical: AxisAccum,
    horizontal: AxisAccum,
}

#[derive(Debug, Default, Clone, Copy)]
struct AxisAccum {
    active: bool,
    value: f64,
    discrete: Option<f64>,
    stop: bool,
}

impl PendingAxis {
    fn axis_mut(&mut self, axis: ScrollAxis) -> &mut AxisAccum {
        match axis {
            ScrollAxis::Vertical => &mut self.vertical,
            ScrollAxis::Horizontal => &mut self.horizontal,
        }
    }

    /// Materialise the accumulated batch into one [`Scroll`] per active axis.
    /// Pure — the ordering is vertical-then-horizontal for determinism.
    fn to_scrolls(self) -> impl Iterator<Item = Scroll> {
        let source = self.source.unwrap_or(ScrollSource::Unknown);
        [
            (ScrollAxis::Vertical, self.vertical),
            (ScrollAxis::Horizontal, self.horizontal),
        ]
        .into_iter()
        .filter(|(_, a)| a.active)
        .map(move |(axis, a)| Scroll {
            axis,
            value: a.value,
            discrete: a.discrete,
            source,
            stop: a.stop,
        })
    }
}

fn map_axis(axis: WEnum<wl_pointer::Axis>) -> Option<ScrollAxis> {
    match axis {
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => Some(ScrollAxis::Vertical),
        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => Some(ScrollAxis::Horizontal),
        _ => None,
    }
}

fn map_source(source: wl_pointer::AxisSource) -> ScrollSource {
    match source {
        wl_pointer::AxisSource::Wheel => ScrollSource::Wheel,
        wl_pointer::AxisSource::Finger => ScrollSource::Finger,
        wl_pointer::AxisSource::Continuous => ScrollSource::Continuous,
        wl_pointer::AxisSource::WheelTilt => ScrollSource::WheelTilt,
        _ => ScrollSource::Unknown,
    }
}

impl AppState {
    /// Flush one seat's accumulated axis state into `Scroll` pointer events at
    /// the close of a `frame`. Pushes straight to `pending_events` — the
    /// frame_buffer for this batch has already been drained by the caller.
    fn flush_pending_axis(&mut self, idx: usize) {
        let pending = std::mem::take(&mut self.seats[idx].pending_axis);
        let Some(sid_u32) = self.seats[idx].focus else {
            return;
        };
        let (x, y) = self.seats[idx].pos;
        for scroll in pending.to_scrolls() {
            self.pending_events.push_back(Event::Pointer(PointerEvent {
                surface: SurfaceId(sid_u32),
                kind: PointerEventKind::Scroll(scroll),
                x,
                y,
            }));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, u32> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        seat_id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(idx) = state.seat_idx(*seat_id) else {
            return;
        };
        let has_frame = state.seats[idx].pointer_caps.is_some_and(|c| c.has_frame());
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(sidx) = state.surface_idx_for_wl(&surface) {
                    let sid = state.surfaces[sidx].id;
                    let scale = state.surfaces[sidx].scale as f64;
                    let (x, y) = (surface_x * scale, surface_y * scale);
                    state.seats[idx].focus = Some(sid.0);
                    state.seats[idx].pos = (x, y);
                    state.deliver_pointer(
                        idx,
                        PointerEvent {
                            surface: sid,
                            kind: PointerEventKind::Enter,
                            x,
                            y,
                        },
                    );
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if let Some(sidx) = state.surface_idx_for_wl(&surface) {
                    let sid = state.surfaces[sidx].id;
                    let (x, y) = state.seats[idx].pos;
                    state.deliver_pointer(
                        idx,
                        PointerEvent {
                            surface: sid,
                            kind: PointerEventKind::Leave,
                            x,
                            y,
                        },
                    );
                    state.seats[idx].focus = None;
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                let Some(sid_u32) = state.seats[idx].focus else {
                    return;
                };
                let Some(sidx) = state.surface_idx(sid_u32) else {
                    return;
                };
                let scale = state.surfaces[sidx].scale as f64;
                let x = surface_x * scale;
                let y = surface_y * scale;
                state.seats[idx].pos = (x, y);
                state.deliver_pointer(
                    idx,
                    PointerEvent {
                        surface: SurfaceId(sid_u32),
                        kind: PointerEventKind::Motion,
                        x,
                        y,
                    },
                );
            }
            wl_pointer::Event::Button {
                button, state: bs, ..
            } => {
                let Some(sid_u32) = state.seats[idx].focus else {
                    return;
                };
                let b = PointerButton::from_code(button);
                let kind = match bs {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => PointerEventKind::Press(b),
                    WEnum::Value(wl_pointer::ButtonState::Released) => PointerEventKind::Release(b),
                    _ => return,
                };
                let (x, y) = state.seats[idx].pos;
                state.deliver_pointer(
                    idx,
                    PointerEvent {
                        surface: SurfaceId(sid_u32),
                        kind,
                        x,
                        y,
                    },
                );
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let Some(axis) = map_axis(axis) else {
                    return;
                };
                if has_frame {
                    let accum = state.seats[idx].pending_axis.axis_mut(axis);
                    accum.active = true;
                    accum.value += value;
                } else {
                    // Pre-v5 pointer: no frame batching and no source/discrete
                    // metadata, so deliver a continuous scroll immediately.
                    if let Some(sid_u32) = state.seats[idx].focus {
                        let (x, y) = state.seats[idx].pos;
                        state.deliver_pointer(
                            idx,
                            PointerEvent {
                                surface: SurfaceId(sid_u32),
                                kind: PointerEventKind::Scroll(Scroll {
                                    axis,
                                    value,
                                    discrete: None,
                                    source: ScrollSource::Unknown,
                                    stop: false,
                                }),
                                x,
                                y,
                            },
                        );
                    }
                }
            }
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(src),
            } => {
                state.seats[idx].pending_axis.source = Some(map_source(src));
            }
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                // v5..8: integer wheel clicks. Superseded by value120 on v8+.
                if let Some(axis) = map_axis(axis) {
                    let accum = state.seats[idx].pending_axis.axis_mut(axis);
                    accum.active = true;
                    accum.discrete = Some(accum.discrete.unwrap_or(0.0) + discrete as f64);
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                // v8+: high-resolution wheel, 120 units per logical click.
                if let Some(axis) = map_axis(axis) {
                    let accum = state.seats[idx].pending_axis.axis_mut(axis);
                    accum.active = true;
                    accum.discrete = Some(accum.discrete.unwrap_or(0.0) + value120 as f64 / 120.0);
                }
            }
            wl_pointer::Event::AxisStop { axis, .. } => {
                if let Some(axis) = map_axis(axis) {
                    let accum = state.seats[idx].pending_axis.axis_mut(axis);
                    accum.active = true;
                    accum.stop = true;
                }
            }
            wl_pointer::Event::Frame => {
                // Drain the buffered discrete events, then materialise scroll(s)
                // from the accumulated axis state. Split borrows to avoid an
                // intermediate Vec; frame_buffer keeps its capacity.
                let seats = &mut state.seats;
                let events = &mut state.pending_events;
                for ev in seats[idx].frame_buffer.drain(..) {
                    events.push_back(Event::Pointer(ev));
                }
                state.flush_pending_axis(idx);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Record advertised formats so the draw path can skip the R/B swap when
        // the swap-free `Abgr8888` is available. `Argb8888` is guaranteed and
        // needs no recording.
        if let wl_shm::Event::Format { format } = event {
            if matches!(format, WEnum::Value(wl_shm::Format::Abgr8888)) {
                state.caps.abgr_supported = true;
            }
        }
    }
}

// Globals with no client-bound events.
delegate_noop!(AppState: wl_compositor::WlCompositor);
delegate_noop!(AppState: wl_shm_pool::WlShmPool);
delegate_noop!(AppState: zwlr_layer_shell_v1::ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    use super::super::event_targets_surface;
    use super::*;
    use crate::input::PointerEvent;

    fn accum(active: bool, value: f64, discrete: Option<f64>, stop: bool) -> AxisAccum {
        AxisAccum {
            active,
            value,
            discrete,
            stop,
        }
    }

    #[test]
    fn empty_batch_yields_no_scrolls() {
        let p = PendingAxis::default();
        assert_eq!(p.to_scrolls().count(), 0);
    }

    #[test]
    fn continuous_vertical_scroll_keeps_value_and_no_discrete() {
        let p = PendingAxis {
            source: Some(ScrollSource::Finger),
            vertical: accum(true, 12.5, None, false),
            horizontal: AxisAccum::default(),
        };
        let scrolls: Vec<_> = p.to_scrolls().collect();
        assert_eq!(scrolls.len(), 1);
        assert_eq!(scrolls[0].axis, ScrollAxis::Vertical);
        assert_eq!(scrolls[0].value, 12.5);
        assert_eq!(scrolls[0].discrete, None);
        assert_eq!(scrolls[0].source, ScrollSource::Finger);
        assert!(!scrolls[0].stop);
    }

    #[test]
    fn wheel_scroll_reports_discrete_steps_without_losing_continuous_value() {
        // A notched wheel: value120 of 120 == one step, plus its continuous
        // value. Discrete and continuous must both survive, never conflated.
        let p = PendingAxis {
            source: Some(ScrollSource::Wheel),
            vertical: accum(true, 10.0, Some(1.0), false),
            horizontal: AxisAccum::default(),
        };
        let scrolls: Vec<_> = p.to_scrolls().collect();
        assert_eq!(scrolls.len(), 1);
        assert_eq!(scrolls[0].discrete, Some(1.0));
        assert_eq!(scrolls[0].value, 10.0);
        assert_eq!(scrolls[0].source, ScrollSource::Wheel);
    }

    #[test]
    fn both_axes_active_emit_vertical_then_horizontal() {
        let p = PendingAxis {
            source: Some(ScrollSource::Continuous),
            vertical: accum(true, 1.0, None, false),
            horizontal: accum(true, 2.0, None, false),
        };
        let scrolls: Vec<_> = p.to_scrolls().collect();
        assert_eq!(scrolls.len(), 2);
        assert_eq!(scrolls[0].axis, ScrollAxis::Vertical);
        assert_eq!(scrolls[1].axis, ScrollAxis::Horizontal);
    }

    #[test]
    fn axis_stop_is_reported_with_zero_value() {
        let p = PendingAxis {
            source: Some(ScrollSource::Finger),
            vertical: accum(true, 0.0, None, true),
            horizontal: AxisAccum::default(),
        };
        let scrolls: Vec<_> = p.to_scrolls().collect();
        assert_eq!(scrolls.len(), 1);
        assert!(scrolls[0].stop);
        assert_eq!(scrolls[0].value, 0.0);
    }

    #[test]
    fn missing_source_falls_back_to_unknown() {
        let p = PendingAxis {
            source: None,
            vertical: accum(true, 3.0, None, false),
            horizontal: AxisAccum::default(),
        };
        let scrolls: Vec<_> = p.to_scrolls().collect();
        assert_eq!(scrolls[0].source, ScrollSource::Unknown);
    }

    #[test]
    fn map_axis_ignores_unknown_axes() {
        assert_eq!(
            map_axis(WEnum::Value(wl_pointer::Axis::VerticalScroll)),
            Some(ScrollAxis::Vertical)
        );
        assert_eq!(
            map_axis(WEnum::Value(wl_pointer::Axis::HorizontalScroll)),
            Some(ScrollAxis::Horizontal)
        );
        assert_eq!(map_axis(WEnum::Unknown(99)), None);
    }

    #[test]
    fn event_targets_surface_matches_only_the_named_surface() {
        let a = SurfaceId(1);
        let b = SurfaceId(2);
        let cfg = Event::Configure {
            surface: a,
            width: 10,
            height: 10,
            scale: 1,
        };
        assert!(event_targets_surface(&cfg, a));
        assert!(!event_targets_surface(&cfg, b));

        let ptr = Event::Pointer(PointerEvent {
            surface: a,
            kind: PointerEventKind::Motion,
            x: 0.0,
            y: 0.0,
        });
        assert!(event_targets_surface(&ptr, a));
        assert!(!event_targets_surface(&ptr, b));

        // Output/global events are surface-agnostic.
        assert!(!event_targets_surface(&Event::OutputAdded(OutputId(1)), a));
        assert!(!event_targets_surface(&Event::GlobalLost("wl_shm"), a));
    }
}

//! meander — a low-level Wayland UI toolkit for river/gharial desktops.
//!
//! Meander is not a desktop environment, a config-driven panel, or a widget
//! library. It is a small set of primitives — a Wayland connection, layer-shell
//! surfaces, a 2D canvas, input events, and an output enumerator — that you
//! assemble in plain Rust to build whatever surface you want: a bar, a
//! launcher, a tooltip overlay, a notification daemon, a wallpaper, a HUD.
//!
//! Companion crate [`meander-gharial`] gives first-class access to the gharial
//! window manager over its IPC socket, so a meander surface can subscribe to
//! the live tag mask, active mode, focused window and layout parameters.
//!
//! # The shape of a meander program
//!
//! ```no_run
//! use std::time::Duration;
//! use meander::{App, Anchor, Color, Event, Layer};
//!
//! let mut app = App::connect()?;
//! let bar = app.layer_surface()
//!     .namespace("meander.bar")
//!     .layer(Layer::Top)
//!     .anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT)
//!     .size(0, 28)
//!     .exclusive_zone(28)
//!     .build()?;
//!
//! loop {
//!     while let Some(ev) = app.next_event() {
//!         match ev {
//!             Event::Configure { surface, .. } if surface == bar => {
//!                 app.surface(bar).draw(|c| {
//!                     c.fill(Color::hex(0x14141Cff));
//!                     c.rect(8.0, 8.0, 12.0, 12.0, Color::hex(0xC8324Bff));
//!                 })?;
//!             }
//!             Event::Closed { .. } => return Ok(()),
//!             _ => {}
//!         }
//!     }
//!     app.dispatch(Some(Duration::from_millis(100)))?;
//! }
//! # Ok::<(), meander::Error>(())
//! ```
//!
//! # Design notes
//!
//! - **You own the event loop.** `App` exposes `dispatch`, `flush`,
//!   `next_event` and the raw `connection_fd` so you can interleave it with
//!   pipes, timers, sockets, or a `calloop` event loop of your own.
//! - **No widgets, no layout engine.** A surface gives you a `Canvas` of raw
//!   pixels. You position things with arithmetic.
//! - **Pure Rust stack.** `wayland-client` + `wayland-protocols-wlr` for the
//!   wire, `tiny-skia` for vector drawing, `fontdue` for glyph rasterisation,
//!   `rustix` for memfd / mmap. No `libwayland` runtime dependency.
//! - **Layer-shell first.** Meander targets `wlr-layer-shell` because that is
//!   what desktop UI surfaces actually want. xdg-shell windows are out of
//!   scope; if you need one, use a different toolkit.

#![deny(rust_2018_idioms)]
#![allow(clippy::too_many_arguments)]

pub mod canvas;
pub mod color;
pub mod error;
pub mod event;
pub mod font;
pub mod input;
pub mod output;
pub mod surface;

mod runtime;
mod shm;

pub use canvas::Canvas;
pub use color::Color;
pub use error::{Error, Result};
pub use event::{CloseReason, Event};
pub use font::Font;
pub use input::{Axis, PointerButton, PointerEvent, PointerEventKind};
pub use output::{OutputId, OutputInfo, Subpixel, Transform};
pub use runtime::{App, SurfaceHandle};
pub use surface::{Anchor, KeyboardInteractivity, Layer, LayerSurfaceBuilder, SurfaceId};

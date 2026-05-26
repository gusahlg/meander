# meander

A low-level Wayland desktop UI toolkit, built in plain Rust, for river and
gharial.

Meander is not a desktop environment, not a configurable bar, not a widget
framework. It is a small Rust library that gives you:

- a pure-Rust Wayland connection,
- `wlr-layer-shell` surfaces (top/bottom/left/right anchored, exclusive zones,
  layers),
- a software-rendered `tiny-skia` canvas with fontdue-based text,
- pointer input, output enumeration, frame callbacks, and
- a first-class IPC client for the [gharial][gharial] window manager.

You assemble these into whatever desktop surface you want — a bar, a launcher,
a notification overlay, a wallpaper, a HUD. It is closer to "you are writing
the bar yourself, in Rust" than to "you are configuring a bar someone else
wrote." That's deliberate.

[gharial]: https://github.com/gusahlg/gharial

## Status

**0.1.0** — first usable cut. Targets:

- river 0.4+ with the [gharial][gharial] WM at HEAD (`gharial 0.2.0`),
- any other wlroots-based compositor that advertises `zwlr_layer_shell_v1`
  (sway, hyprland, wayfire, niri, ...). Meander has no river-specific
  protocol dependency; `meander-gharial` is the part that knows about gharial.

What ships:

- `wlr_layer_shell_v1` surfaces with full anchor/margin/exclusive-zone
  control,
- HiDPI: surfaces track entered outputs' max scale and emit
  `Event::Configure { scale }`,
- double-buffered memfd-backed shm pool with on-demand resize,
- `tiny-skia` canvas: fill, rect, stroke_rect, rounded_rect, anti-aliased
  text with arbitrary `fontdue` faces,
- pointer events with frame coalescing,
- output enumeration (name, geometry, mode, scale, transform, subpixel),
- a `Gharial` IPC client mirroring `gharialctl`'s verbs, plus a background
  `StatusPoller` thread,
- a working example bar (`meander-bar`) in `examples/bar`.

What is not in 0.1 (planned for 0.2):

- keyboard input (xkbcommon mapping). For now, gharial owns global bindings
  via `river-xkb-bindings-v1` and surfaces handle pointer only.
- touch input,
- viewports / fractional scaling (`wp_fractional_scale_v1`),
- xdg-output for richer monitor names on compositors that need it,
- session lock (`ext_session_lock_v1`),
- a high-level event loop integration (`calloop` adapter).

## Layout

```
crates/
├── meander/             the core toolkit (Wayland + canvas + input)
└── meander-gharial/     gharial IPC client + background poller
examples/
└── bar/                 a minimal status bar built on meander
```

## Building

```sh
cargo build --release --workspace
# the example bar lands at target/release/meander-bar
```

Runtime dependencies: a wlroots-based Wayland compositor (river +
[gharial][gharial] is the design target), and a font file for the example bar
(`MEANDER_BAR_FONT=...` or pass a path).

## The shape of a meander program

```rust
use std::time::Duration;
use meander::{App, Anchor, Color, Event, Font, Layer};

let mut app = App::connect()?;
let bar = app.layer_surface()
    .namespace("meander.bar")
    .layer(Layer::Top)
    .anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT)
    .size(0, 28)
    .exclusive_zone(28)
    .build()?;

loop {
    while let Some(ev) = app.next_event() {
        match ev {
            Event::Configure { surface, .. } if surface == bar => {
                app.surface(bar).draw(|c| {
                    c.fill(Color::hex(0x14141Cff));
                    c.rect(8.0, 8.0, 12.0, 12.0, Color::hex(0xC8324Bff));
                })?;
            }
            Event::Closed { .. } => return Ok(()),
            _ => {}
        }
    }
    app.dispatch(Some(Duration::from_millis(100)))?;
}
```

You own the event loop. `app.dispatch` blocks until either Wayland delivers
something or the timeout elapses; `app.connection_fd()` is there if you want
to interleave with your own `poll`/`epoll`/`calloop` setup.

## Talking to gharial

`meander-gharial` exposes a typed handle that resolves the socket the same
way `gharialctl` does (`$GHARIAL_SOCKET`, then
`$XDG_RUNTIME_DIR/gharial-$WAYLAND_DISPLAY.sock`). It mirrors gharial's verb
surface so you do not have to shell out:

```rust
use std::time::Duration;
use meander_gharial::Gharial;

let g = Gharial::connect()?;             // pings the daemon
let poller = g.start_polling(Duration::from_millis(100));

// In your render loop:
let status = poller.latest_or_default();
println!("mode = {}, tags = {:#010b}", status.mode, status.tags);

// User clicked tag 3:
g.tag_focus(3)?;
```

`Status` exposes the well-known parameters (`mode`, `tags`, `occupied`,
`focused_tags`, `main_ratio`, `main_count`, `gaps`, `outer_padding`,
`orientation`, `border_width`) and keeps unknown keys in `extras` so meander
stays forward-compatible if gharial grows new parameters.

## Design notes

### Pure-Rust stack

Meander uses `wayland-client`'s pure-Rust backend, so there is no runtime
`libwayland` dependency. Drawing is `tiny-skia` (also pure Rust). Text is
`fontdue`. shm is memfd + mmap via `rustix`. The whole stack is the same
"no system Wayland C library" choice gharial made, for the same reasons.

### Layer-shell only

Meander targets `wlr-layer-shell` because that is the protocol desktop UI
surfaces actually want. `xdg-shell` toplevel windows are out of scope; if
you need one, use a different toolkit. This is a deliberate scope cut to
keep meander small and obvious.

### You own the event loop

The runtime exposes `next_event`, `dispatch(timeout)`, `dispatch_pending`,
`flush`, and the raw `connection_fd`. There is no built-in widget tree, no
reactive system, no main-loop macro. Compose meander into whatever event
loop you like.

### Coordinates and HiDPI

Surface sizes you request are in *logical* pixels. The buffer meander hands
you is `width * scale` by `height * scale` *physical* pixels. `canvas.scale()`
tells you the multiplier so your text and shapes can stay sharp.

## License

MIT.

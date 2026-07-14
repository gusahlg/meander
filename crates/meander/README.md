# meander

A low-level Wayland UI toolkit for river/gharial desktops.

Meander is not a desktop environment, a config-driven panel, or a widget
library. It is a small set of primitives — a Wayland connection, layer-shell
surfaces, a 2D canvas, input events, and an output enumerator — that you
assemble in plain Rust to build whatever surface you want: a bar, a launcher, a
tooltip overlay, a notification daemon, a wallpaper, a HUD.

- **You own the event loop.** `App` exposes `dispatch`, `flush`, `next_event`
  and the raw `connection_fd` so you can interleave it with pipes, timers,
  sockets, or a `calloop` loop of your own.
- **No widgets, no layout engine.** A surface gives you a `Canvas` of raw
  pixels. You position things with arithmetic.
- **Pure Rust stack.** `wayland-client` + `wayland-protocols-wlr` for the wire,
  `tiny-skia` for vector drawing, `fontdue` for glyph rasterisation, `rustix`
  for memfd/mmap. No `libwayland` runtime dependency.
- **Layer-shell first.** Meander targets `wlr-layer-shell` because that is what
  desktop UI surfaces actually want.

```rust,no_run
use std::time::Duration;
use meander::{App, Anchor, Color, Event, Layer};

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
# Ok::<(), meander::Error>(())
```

Companion crate [`meander-gharial`](../meander-gharial) gives first-class access
to the gharial window manager over its IPC socket.

## License

MIT

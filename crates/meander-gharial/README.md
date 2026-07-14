# meander-gharial

Client for the [gharial](https://github.com/gusahlg/meander) window manager's
IPC socket, for use with [`meander`](../meander) surfaces (or on its own).

Gharial speaks a single-line request / single-line response protocol on a Unix
socket. This crate re-implements the wire format (kept deliberately
source-compatible with the `gharial-ipc` crate that ships in the gharial repo)
and adds:

- a typed `Gharial` handle that resolves the socket path the same way
  `gharialctl` does (`$GHARIAL_SOCKET`, then
  `$XDG_RUNTIME_DIR/gharial-$WAYLAND_DISPLAY.sock`),
- typed wrappers for the most useful verbs (`ping`, `status`, `get`, `set`,
  `spawn`, tag focus, mode, ...), and
- `Gharial::poll_status` / `Gharial::start_polling` for surfaces that want to
  redraw whenever WM state changes.

```rust,no_run
use meander_gharial::Gharial;
let g = Gharial::connect()?;
let status = g.status()?;
println!("mode = {}", status.mode);
# Ok::<(), meander_gharial::Error>(())
```

## License

MIT

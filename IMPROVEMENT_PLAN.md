# Meander improvement plan

Audit date: 2026-07-14. This plan describes the current working tree, including
the staged runtime/canvas/gharial refactor. It intentionally favors small,
testable changes over a rewrite.

## Current baseline

- The workspace is compact (~3,550 Rust source lines) and the recent
  `runtime/{mod,dispatch}` split is a good boundary to keep.
- `cargo test --workspace --all-targets` passes all 67 unit tests; both doctests
  pass separately.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- `cargo fmt --all -- --check` currently fails on the staged files.
- Strict docs fail because `crates/meander/src/lib.rs:9` contains an unresolved
  intra-doc link to `meander-gharial`.
- No CI configuration or runtime/transport integration tests are present.

## Priority 0: make the safety boundary boring

### 1. Introduce a checked `BufferLayout`

Compositor-controlled logical dimensions and scale are multiplied unchecked in
`crates/meander/src/runtime/mod.rs:318-321`. `ShmPool::new` then performs more
unchecked size arithmetic and lossy `usize`/`i32` conversions before
`pixels_mut` constructs an unsafe slice (`crates/meander/src/shm.rs:46-121`). In
release mode, an overflow can make the slice larger than the mapping.

- Add a pure `BufferLayout::new(width, height, scale, buffer_count)` that uses
  `checked_mul` and `i32::try_from` for physical size, stride, per-buffer bytes,
  offsets, and total pool bytes.
- Enforce a documented maximum allocation and return a typed
  `InvalidBufferDimensions`/`BufferTooLarge` error before `memfd`, `mmap`, or a
  Wayland request occurs.
- Make the unsafe slice use values from this validated layout and replace its
  `debug_assert!` with an invariant that is also checked in release builds.
- Unit-test zero/minimum sizes, `i32::MAX` boundaries, every overflow step, high
  scale, and both buffer offsets.

Done when arbitrary input cannot reach `mmap`, protocol integer casts, or
`from_raw_parts_mut` without passing one tested layout constructor.

### 2. Validate public protocol inputs before allocating objects

- Validate layer-shell zero-size/anchor rules, exclusive zones (only `-1` or a
  non-negative value), requested output, and version-dependent keyboard mode.
- Resolve the output before creating `wl_surface`; make surface creation
  transactional so an error or failed flush cannot leave an ID/state entry or
  protocol object half-created (`runtime/mod.rs:409-466`).
- Validate gharial tag numbers as the documented `1..=32` range.
- Reject empty or whitespace/control-containing IPC commands. The public
  `Request::encode` currently writes `command` verbatim, so a newline can frame
  an unintended second request (`meander-gharial/src/ipc.rs:41-50`).
- Reject a zero polling interval rather than allowing a tight socket/CPU loop.

Done when invalid caller input produces a typed local error and sends no
partial Wayland or IPC request.

## Priority 1: make protocol behavior correct under failure and hotplug

### 3. Centralize negotiated Wayland capabilities

The registry binds `min(advertised, supported)` but later assumes newer
requests/events exist (`runtime/dispatch.rs:76-118`). In particular,
`set_buffer_scale`/`damage_buffer`, layer-shell `OnDemand`, pointer `release`,
and pointer `frame` have different minimum versions.

- Store registry name and negotiated version for each bound global/object.
- Choose and document a minimum supported compositor/layer-shell version. For
  each optional feature, either use a version-safe fallback or return
  `UnsupportedProtocolVersion`; never issue an unsupported request.
- Deliver pointer events immediately on pre-`wl_pointer.frame` versions, or
  explicitly require the version that supplies frames. Add the modern scroll
  fields (`axis_source`, `axis_discrete`/`value120`, and `axis_stop`) without
  claiming continuous values are discrete steps.
- Exercise the dispatch decisions as pure version-matrix tests.

### 4. Make registry removal and surface teardown complete

`global_remove` currently handles outputs only (`runtime/dispatch.rs:122-129`),
while seat user data is a positional `Vec` index. Output removal also leaves
entered-output proxies behind and can leave a stale HiDPI scale.

- Give seats stable IDs and retain their registry names; avoid indices that
  change after removal (a small generational table or tombstoned slots is
  sufficient).
- On output removal, purge it from every surface and recompute max scale before
  emitting the final events. Test 1x/2x overlap and removal in both orders.
- Handle seat and required-global removal deliberately: release supported
  objects, clear focus/buffered pointer events, and surface a connection/global
  loss event or error.
- On surface destruction, purge queued input/configure/frame events and clear
  any seat focus referring to it.
- In `App::dispatch`, cover socket error/hangup and output backpressure according
  to wayland-client's prepare-read contract instead of only checking `POLLIN`.

### 5. Harden gharial transport and parsing

- Replace unbounded `BufRead::read_line` (`meander-gharial/src/ipc.rs:99-114`)
  with a small documented maximum response size and an absolute deadline. A
  slow trickle must not reset the overall timeout indefinitely.
- Add `Status::try_parse -> Result<Status, StatusParseError>` for known fields;
  reject invalid masks, integers, and non-finite ratios. Keep lossy parsing only
  as an explicitly named compatibility API. The poller must retain the last
  good snapshot and publish a parse error rather than silently changing a bad
  mask to zero.
- Make `wait_until_ready(timeout)` cap each request/sleep to the remaining
  deadline; its current 500 ms per-request timeout can exceed a shorter overall
  deadline (`meander-gharial/src/lib.rs:110-121`).
- Make empty responses use the existing `ParseError::Empty` variant.
- Add a fake `UnixListener` test server covering fragmented success, daemon
  error, EOF, oversized lines, slow trickle, timeout, and shutdown during a
  request. Import upstream gharial golden vectors or depend on its shared IPC
  crate to detect wire-format drift.

Done when the runtime and IPC tests cover old versions, hot-unplug, malformed
input, timeouts, and maximum sizes without a live desktop session.

## Priority 2: profile, then remove full-frame work

### 6. Establish small repeatable benchmarks

Benchmark before changing the renderer. Include a 1920x28 scale-1 bar and a
1920x28 logical/scale-2 bar, with cold and warm glyph caches. Record separately:

- canvas primitives and text rendering;
- buffer clear;
- RGBA-to-BGRA conversion;
- the complete example-bar frame; and
- hot-cache `text_width` plus `text`.

Keep before/after numbers in PR descriptions. Do not add complex SIMD or a new
container unless the representative benchmark shows a material improvement.

### 7. Remove redundant memory passes in this order

Every frame currently clears the whole buffer, often fills it again, swaps R/B
over every pixel, damages the full buffer, and commits (`runtime/mod.rs:340-358`).

1. Make clearing lazy: preserve today's transparent-start semantics, but let a
   first full `Canvas::fill` replace the clear instead of writing every pixel
   twice.
2. Record advertised `wl_shm` formats. When `Abgr8888` is advertised, render
   tiny-skia's RGBA bytes directly; retain guaranteed `Argb8888` plus the swap
   as the compatibility path.
3. Only after measuring, add caller-supplied damage rectangles. Account for the
   age/content of both back buffers before permitting partial redraws.
4. Consider a draw option that requests the next frame callback in the same
   presentation commit, avoiding the separate `request_frame` commit.

### 8. Bound and batch the text cache

`font.rs:26-121` has two unbounded maps keyed by exact `f32` bits and locks once
per glyph lookup. Dynamic sizes can grow memory forever, and measuring then
drawing the same run repeats lookups.

- Validate finite positive font sizes and consolidate metrics/bitmap state into
  one entry per key.
- Add a byte/entry budget with LRU eviction or explicit cache controls.
- Add an internal prepared text run so one cache lock/layout pass can serve
  measurement and drawing; keep the simple existing API as sugar.
- Measure contention and allocations before considering a different lock.

## Priority 3: improve event-loop integration and project hygiene

### 9. Make `StatusPoller` change-driven

- Return `Result` from a new `try_start_polling` instead of panicking on thread
  creation; define the compatibility behavior of `start_polling` separately.
- Store `{snapshot, last_error, revision}` coherently and only publish/notify
  when status changes. Expose a receiver or pollable eventfd so applications do
  not wake every 100 ms merely to compare clones.
- Bound shutdown latency even when a request is in flight.
- Update the example to retain `Arc<Status>`, record scale-correct tag hit
  rectangles while drawing, and report click-command/poller errors instead of
  discarding them (`examples/bar/src/main.rs:80-95,158-171`).

### 10. Add inexpensive quality gates and trim unused build surface

- Format the staged refactor and fix the broken rustdoc link.
- Add CI for `cargo fmt --check`, locked tests (including doctests), strict
  Clippy, strict rustdoc, and the declared MSRV.
- Add `rust-version.workspace = true` and `repository.workspace = true` to
  publishable members; current `cargo metadata` reports both as absent. Add the
  license/readme metadata needed for packaging.
- Remove the unused direct `wayland-protocols` dependency and rustix `pipe`
  feature. Disable tiny-skia's default PNG feature if benchmarks/build checks
  confirm only `std`/`simd` are needed.

## Suggested PR sequence

1. **Green baseline:** formatting, docs, CI, manifest inheritance.
2. **Safe buffers:** `BufferLayout`, typed errors, boundary tests.
3. **Validated edges:** layer builder, IPC command/tag/interval validation, fake
   socket tests.
4. **Wayland lifecycle:** capability matrix, hotplug cleanup, transactional
   creation, dispatch error handling.
5. **Strict status + poller:** fallible parsing, coherent revision/notification,
   bounded shutdown, corrected example.
6. **Measured rendering:** benchmarks, lazy clear, advertised direct pixel
   format; pursue dirty regions/text-run caching only when the data justifies it.

## Deliberately deferred

- Do not replace all small vectors with hash maps yet. Linear lookup is simple
  and likely cheap for a handful of surfaces/outputs; use stable identities for
  correctness first and benchmark lookup before optimizing it.
- Do not introduce an async runtime or widget tree; both conflict with the
  library's stated low-level, caller-owned-event-loop scope.
- Do not hand-write SIMD for pixel conversion before testing the advertised
  direct-format path, which may remove that conversion entirely.

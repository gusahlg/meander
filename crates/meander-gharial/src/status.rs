//! Parsed gharial status snapshot and a background poller.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;

use crate::Gharial;

/// A field in a `status` line could not be parsed strictly by
/// [`Status::try_parse`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StatusParseError {
    #[error("invalid tag mask for `{key}`: {value:?}")]
    BadMask { key: &'static str, value: String },
    #[error("invalid integer for `{key}`: {value:?}")]
    BadInt { key: &'static str, value: String },
    #[error("invalid ratio for `{key}`: {value:?} (must be a finite number)")]
    BadRatio { key: &'static str, value: String },
}

/// All 32 gharial tags as a bitmask.
pub type TagMask = u32;

/// One parsed result of `gharialctl status`.
///
/// gharial's status line is a semicolon-separated list of `key=value` pairs.
/// Unknown keys are kept in `extras` so meander stays forward-compatible with
/// new gharial parameters without code changes here.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// Active binding mode ("default" when nothing else is pushed).
    pub mode: String,
    /// Bitmask of currently-visible tags (bit 0 = tag 1).
    pub tags: TagMask,
    /// Bitmask of tags that contain at least one window.
    pub occupied: TagMask,
    /// Bitmask of tags containing the focused window.
    pub focused_tags: TagMask,
    /// Master-stack ratio (typically 0.0..=1.0).
    pub main_ratio: Option<f32>,
    /// Number of windows in the master area.
    pub main_count: Option<u32>,
    /// Inner gap, in pixels.
    pub gaps: Option<i32>,
    /// Outer padding, in pixels.
    pub outer_padding: Option<i32>,
    /// Tile orientation as gharial reports it ("left", "right", "top",
    /// "bottom").
    pub orientation: Option<String>,
    /// Border width, in pixels.
    pub border_width: Option<i32>,
    /// Catch-all for keys this version of meander does not yet model.
    pub extras: Vec<(String, String)>,
}

impl Default for Status {
    fn default() -> Self {
        // `mode` is "default" rather than "" so `latest_or_default()` and a
        // freshly parsed status both report the same canonical idle mode —
        // clients that style on mode == "default" don't need to special-case
        // the brief window before the first poll completes.
        Self {
            mode: "default".into(),
            tags: 0,
            occupied: 0,
            focused_tags: 0,
            main_ratio: None,
            main_count: None,
            gaps: None,
            outer_padding: None,
            orientation: None,
            border_width: None,
            extras: Vec::new(),
        }
    }
}

impl Status {
    /// Parse a `key=value;key=value;...` line as emitted by `status`, rejecting
    /// a malformed known field rather than silently coercing it.
    ///
    /// A bad tag mask, integer, or non-finite ratio yields a
    /// [`StatusParseError`] — the poller keeps its last good snapshot instead of
    /// publishing, say, a mask that quietly collapsed to zero. Unknown keys are
    /// still tolerated (kept in [`extras`](Status::extras)) so new gharial
    /// parameters do not break this parser.
    pub fn try_parse(body: &str) -> Result<Self, StatusParseError> {
        let mut s = Self::default();
        for pair in body.split(';') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let Some((k, v)) = pair.split_once('=') else {
                s.extras.push((pair.into(), String::new()));
                continue;
            };
            match k {
                "mode" => s.mode = v.into(),
                "tags" => s.tags = parse_mask_strict("tags", v)?,
                "occupied" => s.occupied = parse_mask_strict("occupied", v)?,
                "focused-tags" | "focused_tags" => {
                    s.focused_tags = parse_mask_strict("focused-tags", v)?
                }
                "main-ratio" | "main_ratio" => {
                    s.main_ratio = Some(parse_ratio_strict("main-ratio", v)?)
                }
                "main-count" | "main_count" => {
                    s.main_count = Some(parse_int_strict("main-count", v)?)
                }
                "gaps" => s.gaps = Some(parse_int_strict("gaps", v)?),
                "outer-padding" | "outer_padding" => {
                    s.outer_padding = Some(parse_int_strict("outer-padding", v)?)
                }
                "orientation" => s.orientation = Some(v.into()),
                "border-width" | "border_width" => {
                    s.border_width = Some(parse_int_strict("border-width", v)?)
                }
                _ => s.extras.push((k.into(), v.into())),
            }
        }
        if s.mode.is_empty() {
            s.mode = "default".into();
        }
        Ok(s)
    }

    /// Lossy compatibility parser: coerces any malformed known field to a
    /// default (mask/integer `0`, ratio `None`) instead of erroring. Prefer
    /// [`Status::try_parse`]; this exists for callers that must never fail on a
    /// slightly-off status line.
    pub fn parse_lossy(body: &str) -> Self {
        let mut s = Self::default();
        for pair in body.split(';') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let Some((k, v)) = pair.split_once('=') else {
                s.extras.push((pair.into(), String::new()));
                continue;
            };
            match k {
                "mode" => s.mode = v.into(),
                "tags" => s.tags = parse_mask_lossy(v),
                "occupied" => s.occupied = parse_mask_lossy(v),
                "focused-tags" | "focused_tags" => s.focused_tags = parse_mask_lossy(v),
                "main-ratio" | "main_ratio" => s.main_ratio = v.trim().parse().ok(),
                "main-count" | "main_count" => s.main_count = v.trim().parse().ok(),
                "gaps" => s.gaps = v.trim().parse().ok(),
                "outer-padding" | "outer_padding" => s.outer_padding = v.trim().parse().ok(),
                "orientation" => s.orientation = Some(v.into()),
                "border-width" | "border_width" => s.border_width = v.trim().parse().ok(),
                _ => s.extras.push((k.into(), v.into())),
            }
        }
        if s.mode.is_empty() {
            s.mode = "default".into();
        }
        s
    }

    /// Convenience: is tag `n` (1-indexed) currently visible?
    pub fn tag_active(&self, n: u32) -> bool {
        (1..=32).contains(&n) && (self.tags & (1u32 << (n - 1))) != 0
    }

    /// Convenience: does tag `n` (1-indexed) contain at least one window?
    pub fn tag_occupied(&self, n: u32) -> bool {
        (1..=32).contains(&n) && (self.occupied & (1u32 << (n - 1))) != 0
    }
}

fn parse_mask_digits(v: &str) -> Result<u32, ()> {
    let v = v.trim();
    if let Some(rest) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u32::from_str_radix(rest, 16).map_err(|_| ())
    } else if let Some(rest) = v.strip_prefix("0b").or_else(|| v.strip_prefix("0B")) {
        u32::from_str_radix(rest, 2).map_err(|_| ())
    } else {
        v.parse().map_err(|_| ())
    }
}

fn parse_mask_strict(key: &'static str, v: &str) -> Result<TagMask, StatusParseError> {
    parse_mask_digits(v).map_err(|_| StatusParseError::BadMask {
        key,
        value: v.trim().into(),
    })
}

fn parse_mask_lossy(v: &str) -> TagMask {
    parse_mask_digits(v).unwrap_or(0)
}

fn parse_int_strict<T: std::str::FromStr>(
    key: &'static str,
    v: &str,
) -> Result<T, StatusParseError> {
    v.trim().parse().map_err(|_| StatusParseError::BadInt {
        key,
        value: v.trim().into(),
    })
}

fn parse_ratio_strict(key: &'static str, v: &str) -> Result<f32, StatusParseError> {
    let bad = || StatusParseError::BadRatio {
        key,
        value: v.trim().into(),
    };
    let r: f32 = v.trim().parse().map_err(|_| bad())?;
    if r.is_finite() {
        Ok(r)
    } else {
        Err(bad())
    }
}

/// Smallest allowed poll interval. Guards against a zero interval that would
/// spin the socket/CPU as fast as the daemon can answer.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

use std::os::fd::{AsRawFd, RawFd};

use rustix::event::{eventfd, EventfdFlags};

/// A coherent read of the poller's state, taken under a single lock so the
/// snapshot, error, and revision always agree with each other.
#[derive(Debug, Clone)]
pub struct PollSnapshot {
    /// Most recent successful status, shared without cloning the [`Status`].
    pub status: Option<Arc<Status>>,
    /// Error string from the most recent failed fetch, if the last attempt
    /// failed. Cleared once a fetch succeeds again.
    pub last_error: Option<String>,
    /// Monotonic counter bumped every time the published status *or* the error
    /// state changes. Compare against a stored value to detect change cheaply.
    pub revision: u64,
}

#[derive(Default)]
struct Inner {
    snapshot: Option<Arc<Status>>,
    last_error: Option<String>,
    revision: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    stop: AtomicBool,
    /// eventfd signalled whenever `revision` advances, so a consumer can block
    /// in its own poll loop instead of waking on a timer to compare clones.
    event: rustix::fd::OwnedFd,
}

impl Shared {
    /// Bump the revision and wake any fd waiters.
    fn notify(&self, inner: &mut Inner) {
        inner.revision += 1;
        // eventfd add-1; NONBLOCK write can only fail at the u64 counter
        // saturation point, which a draining consumer prevents. Ignore errors.
        let _ = rustix::io::write(&self.event, &1u64.to_ne_bytes());
    }
}

/// Background poller that fetches `status` on a fixed interval and publishes a
/// new snapshot only when the state actually changes.
///
/// Drop to stop the polling thread (shutdown latency is bounded by the in-flight
/// request timeout). [`latest`](Self::latest) returns the most recent successful
/// snapshot; [`latest_or_default`](Self::latest_or_default) keeps rendering even
/// if the daemon briefly went away. Instead of waking on a timer to compare
/// clones, watch [`notify_fd`](Self::notify_fd) in your own poll loop and
/// [`drain`](Self::drain) it when it fires, or compare [`revision`](Self::revision).
pub struct StatusPoller {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl StatusPoller {
    /// Fallible constructor: rejects a zero/sub-minimum interval and surfaces an
    /// eventfd or thread-spawn failure instead of panicking.
    pub(crate) fn try_new(client: Gharial, interval: Duration) -> crate::Result<Self> {
        if interval < MIN_POLL_INTERVAL {
            return Err(crate::Error::BadArgs(
                "poll interval must be at least MIN_POLL_INTERVAL",
            ));
        }
        let event = eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC).map_err(|e| {
            crate::Error::Io {
                socket: "eventfd".into(),
                source: e.into(),
            }
        })?;
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner::default()),
            stop: AtomicBool::new(false),
            event,
        });
        let shared_t = shared.clone();
        let handle = thread::Builder::new()
            .name("meander-gharial-poller".into())
            .spawn(move || run_poller(client, &shared_t, interval))
            .map_err(|e| crate::Error::Io {
                socket: "poller thread".into(),
                source: e,
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    /// Infallible convenience constructor. Clamps a sub-minimum interval up to
    /// [`MIN_POLL_INTERVAL`] and panics only if the OS refuses an eventfd or a
    /// thread — use [`Gharial::try_start_polling`] to handle those instead.
    pub(crate) fn new(client: Gharial, interval: Duration) -> Self {
        Self::try_new(client, interval.max(MIN_POLL_INTERVAL))
            .expect("failed to start gharial status poller")
    }

    /// Take a coherent read of the current snapshot, error, and revision.
    pub fn poll(&self) -> PollSnapshot {
        let inner = self.shared.inner.lock().expect("poller mutex poisoned");
        PollSnapshot {
            status: inner.snapshot.clone(),
            last_error: inner.last_error.clone(),
            revision: inner.revision,
        }
    }

    /// Latest successful snapshot as an owned value, or `None` if we haven't
    /// gotten one yet. Allocates a fresh [`Status`] — prefer
    /// [`latest_arc`](Self::latest_arc) on hot paths.
    pub fn latest(&self) -> Option<Status> {
        self.latest_arc().map(|a| (*a).clone())
    }

    /// Latest successful snapshot as a shared [`Arc`], or `None`.
    pub fn latest_arc(&self) -> Option<Arc<Status>> {
        self.shared
            .inner
            .lock()
            .expect("poller mutex poisoned")
            .snapshot
            .clone()
    }

    pub fn latest_or_default(&self) -> Status {
        self.latest().unwrap_or_default()
    }

    /// Most recent error string, if the last fetch failed.
    pub fn last_error(&self) -> Option<String> {
        self.shared
            .inner
            .lock()
            .expect("poller mutex poisoned")
            .last_error
            .clone()
    }

    /// Current revision. Advances whenever the published status or error state
    /// changes; stash it and compare to detect updates without cloning.
    pub fn revision(&self) -> u64 {
        self.shared
            .inner
            .lock()
            .expect("poller mutex poisoned")
            .revision
    }

    /// A pollable fd that becomes readable whenever the revision advances. Add
    /// it to your own `poll`/`epoll`/`calloop` set; call [`drain`](Self::drain)
    /// after it fires. The fd is owned by the poller — do not close it.
    pub fn notify_fd(&self) -> RawFd {
        self.shared.event.as_raw_fd()
    }

    /// Reset the notification fd after it has signalled, so it does not
    /// immediately re-fire. Safe to call spuriously.
    pub fn drain(&self) {
        let mut buf = [0u8; 8];
        let _ = rustix::io::read(&self.shared.event, &mut buf);
    }
}

impl Drop for StatusPoller {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run_poller(client: Gharial, shared: &Shared, interval: Duration) {
    while !shared.stop.load(Ordering::Relaxed) {
        match client.status() {
            Ok(s) => publish_status(shared, s),
            Err(e) => publish_error(shared, e.to_string()),
        }
        sleep_interruptible(&shared.stop, interval);
    }
}

/// Publish a status, notifying only if it differs from the last one (or clears a
/// standing error).
fn publish_status(shared: &Shared, status: Status) {
    let mut inner = shared.inner.lock().expect("poller mutex poisoned");
    let changed = inner.snapshot.as_deref() != Some(&status);
    let recovered = inner.last_error.is_some();
    if changed {
        inner.snapshot = Some(Arc::new(status));
    }
    if changed || recovered {
        inner.last_error = None;
        shared.notify(&mut inner);
    }
}

/// Publish an error, keeping the last good snapshot and notifying only when the
/// error text changes (so a persistent failure does not spin the consumer).
fn publish_error(shared: &Shared, err: String) {
    let mut inner = shared.inner.lock().expect("poller mutex poisoned");
    if inner.last_error.as_deref() != Some(err.as_str()) {
        inner.last_error = Some(err);
        shared.notify(&mut inner);
    }
}

fn sleep_interruptible(stop: &AtomicBool, total: Duration) {
    // Wake every 50ms so a drop doesn't have to wait for the full interval.
    let step = Duration::from_millis(50).min(total);
    let mut left = total;
    while left > Duration::ZERO && !stop.load(Ordering::Relaxed) {
        let s = step.min(left);
        thread::sleep(s);
        left = left.saturating_sub(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_status_line() {
        let s = Status::try_parse(
            "mode=default;tags=0x3;occupied=0x7;focused-tags=0x1;\
             main-ratio=0.55;main-count=1;gaps=8;outer-padding=12;\
             orientation=left;border-width=2",
        )
        .unwrap();
        assert_eq!(s.mode, "default");
        assert_eq!(s.tags, 0x3);
        assert_eq!(s.occupied, 0x7);
        assert_eq!(s.focused_tags, 0x1);
        assert_eq!(s.main_ratio, Some(0.55));
        assert_eq!(s.main_count, Some(1));
        assert_eq!(s.gaps, Some(8));
        assert_eq!(s.outer_padding, Some(12));
        assert_eq!(s.orientation.as_deref(), Some("left"));
        assert_eq!(s.border_width, Some(2));
        assert!(s.extras.is_empty());
    }

    #[test]
    fn defaults_mode_to_default_when_missing() {
        let s = Status::try_parse("tags=0x1").unwrap();
        assert_eq!(s.mode, "default");
        assert_eq!(s.tags, 0x1);
    }

    #[test]
    fn unknown_keys_land_in_extras() {
        let s = Status::try_parse("mode=launcher;weird-key=42;tags=0x2").unwrap();
        assert_eq!(s.mode, "launcher");
        assert_eq!(s.tags, 0x2);
        assert_eq!(s.extras, vec![("weird-key".into(), "42".into())]);
    }

    #[test]
    fn accepts_snake_and_kebab_aliases() {
        let snake =
            Status::try_parse("main_ratio=0.5;focused_tags=0x4;outer_padding=3;border_width=1")
                .unwrap();
        let kebab =
            Status::try_parse("main-ratio=0.5;focused-tags=0x4;outer-padding=3;border-width=1")
                .unwrap();
        assert_eq!(snake, kebab);
    }

    #[test]
    fn parses_decimal_hex_and_binary_masks() {
        assert_eq!(Status::try_parse("tags=5").unwrap().tags, 5);
        assert_eq!(Status::try_parse("tags=0x10").unwrap().tags, 0x10);
        assert_eq!(Status::try_parse("tags=0X10").unwrap().tags, 0x10);
        assert_eq!(Status::try_parse("tags=0b101").unwrap().tags, 0b101);
        assert_eq!(Status::try_parse("tags=0B101").unwrap().tags, 0b101);
    }

    #[test]
    fn try_parse_rejects_malformed_known_fields() {
        assert!(matches!(
            Status::try_parse("tags=garbage"),
            Err(StatusParseError::BadMask { key: "tags", .. })
        ));
        assert!(matches!(
            Status::try_parse("gaps=notanint"),
            Err(StatusParseError::BadInt { key: "gaps", .. })
        ));
        assert!(matches!(
            Status::try_parse("main-ratio=nan"),
            Err(StatusParseError::BadRatio {
                key: "main-ratio",
                ..
            })
        ));
        assert!(matches!(
            Status::try_parse("main-ratio=inf"),
            Err(StatusParseError::BadRatio { .. })
        ));
    }

    #[test]
    fn lossy_parse_coerces_garbage_to_defaults() {
        assert_eq!(Status::parse_lossy("tags=garbage").tags, 0);
        assert_eq!(Status::parse_lossy("gaps=notanint").gaps, None);
        assert_eq!(Status::parse_lossy("main-ratio=notafloat").main_ratio, None);
        // Valid fields still parse the same as the strict path.
        assert_eq!(Status::parse_lossy("tags=0x10").tags, 0x10);
    }

    #[test]
    fn ignores_empty_pairs() {
        let s = Status::try_parse(";;mode=foo;;").unwrap();
        assert_eq!(s.mode, "foo");
        assert!(s.extras.is_empty());
    }

    #[test]
    fn tag_active_and_occupied_are_one_indexed() {
        let s = Status {
            tags: 0b101,     // tags 1 and 3
            occupied: 0b110, // tags 2 and 3
            ..Status::default()
        };
        assert!(s.tag_active(1));
        assert!(!s.tag_active(2));
        assert!(s.tag_active(3));
        assert!(!s.tag_occupied(1));
        assert!(s.tag_occupied(2));
        assert!(s.tag_occupied(3));
    }

    #[test]
    fn tag_helpers_reject_out_of_range() {
        let s = Status {
            tags: u32::MAX,
            ..Status::default()
        };
        assert!(!s.tag_active(0));
        assert!(s.tag_active(1));
        assert!(s.tag_active(32));
        assert!(!s.tag_active(33));
    }

    #[test]
    fn pair_without_equals_lands_in_extras_with_empty_value() {
        let s = Status::try_parse("mode=x;flag").unwrap();
        assert_eq!(s.extras, vec![("flag".into(), String::new())]);
    }

    #[test]
    fn poller_is_send_and_sync() {
        // Compile-time check: the poller must be shareable across threads.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StatusPoller>();
        assert_send_sync::<PollSnapshot>();
    }

    // --- change-notification unit tests over the shared state directly -------

    fn shared() -> Shared {
        Shared {
            inner: Mutex::new(Inner::default()),
            stop: AtomicBool::new(false),
            event: eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC).unwrap(),
        }
    }

    fn revision(s: &Shared) -> u64 {
        s.inner.lock().unwrap().revision
    }

    #[test]
    fn first_status_notifies_and_sets_revision() {
        let s = shared();
        publish_status(&s, Status::try_parse("tags=0x1").unwrap());
        assert_eq!(revision(&s), 1);
        assert!(s.inner.lock().unwrap().snapshot.is_some());
    }

    #[test]
    fn unchanged_status_does_not_bump_revision() {
        let s = shared();
        let st = Status::try_parse("tags=0x1").unwrap();
        publish_status(&s, st.clone());
        publish_status(&s, st);
        assert_eq!(revision(&s), 1);
    }

    #[test]
    fn changed_status_bumps_revision() {
        let s = shared();
        publish_status(&s, Status::try_parse("tags=0x1").unwrap());
        publish_status(&s, Status::try_parse("tags=0x2").unwrap());
        assert_eq!(revision(&s), 2);
    }

    #[test]
    fn error_publishes_once_and_keeps_last_snapshot() {
        let s = shared();
        publish_status(&s, Status::try_parse("tags=0x1").unwrap());
        publish_error(&s, "boom".into());
        publish_error(&s, "boom".into()); // same error: no new notification
        let inner = s.inner.lock().unwrap();
        assert_eq!(inner.revision, 2);
        assert_eq!(inner.last_error.as_deref(), Some("boom"));
        // Last good snapshot is retained through the error.
        assert!(inner.snapshot.is_some());
    }

    #[test]
    fn recovering_from_error_clears_it_and_notifies() {
        let s = shared();
        publish_error(&s, "boom".into());
        // Same status value as default, but the error must still clear.
        publish_status(&s, Status::default());
        let inner = s.inner.lock().unwrap();
        assert_eq!(inner.last_error, None);
        assert_eq!(inner.revision, 2);
    }

    #[test]
    fn default_uses_default_mode_for_consistency_with_parse() {
        let parsed_no_mode = Status::try_parse("tags=0x1").unwrap();
        assert_eq!(Status::default().mode, "default");
        assert_eq!(parsed_no_mode.mode, "default");
    }

    #[test]
    fn default_status_round_trips_through_parse_when_empty() {
        // Parsing an empty body produces a Status equal to default (after the
        // empty-mode promotion).
        let parsed = Status::try_parse("").unwrap();
        assert_eq!(parsed, Status::default());
    }
}

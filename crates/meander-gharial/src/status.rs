//! Parsed gharial status snapshot and a background poller.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{Gharial, Result};

/// All 32 gharial tags as a bitmask.
pub type TagMask = u32;

/// One parsed result of `gharialctl status`.
///
/// gharial's status line is a semicolon-separated list of `key=value` pairs.
/// Unknown keys are kept in `extras` so meander stays forward-compatible with
/// new gharial parameters without code changes here.
#[derive(Debug, Clone, Default, PartialEq)]
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

impl Status {
    /// Parse a `key=value;key=value;...` line as emitted by `status`.
    pub fn parse(body: &str) -> Self {
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
                "tags" => s.tags = parse_mask(v),
                "occupied" => s.occupied = parse_mask(v),
                "focused-tags" | "focused_tags" => s.focused_tags = parse_mask(v),
                "main-ratio" | "main_ratio" => s.main_ratio = v.parse().ok(),
                "main-count" | "main_count" => s.main_count = v.parse().ok(),
                "gaps" => s.gaps = v.parse().ok(),
                "outer-padding" | "outer_padding" => s.outer_padding = v.parse().ok(),
                "orientation" => s.orientation = Some(v.into()),
                "border-width" | "border_width" => s.border_width = v.parse().ok(),
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
        n >= 1 && n <= 32 && (self.tags & (1u32 << (n - 1))) != 0
    }

    /// Convenience: does tag `n` (1-indexed) contain at least one window?
    pub fn tag_occupied(&self, n: u32) -> bool {
        n >= 1 && n <= 32 && (self.occupied & (1u32 << (n - 1))) != 0
    }
}

fn parse_mask(v: &str) -> TagMask {
    let v = v.trim();
    if let Some(rest) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u32::from_str_radix(rest, 16).unwrap_or(0)
    } else if let Some(rest) = v.strip_prefix("0b").or_else(|| v.strip_prefix("0B")) {
        u32::from_str_radix(rest, 2).unwrap_or(0)
    } else {
        v.parse().unwrap_or(0)
    }
}

/// Background poller that fetches `status` on a fixed interval.
///
/// Drop to stop the polling thread. `latest()` returns the most recent
/// successful snapshot; `latest_or_default()` is convenient when you want to
/// keep rendering even if the daemon briefly went away.
pub struct StatusPoller {
    inner: Arc<Mutex<Option<Status>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl StatusPoller {
    pub(crate) fn new(client: Gharial, interval: Duration) -> Self {
        let inner: Arc<Mutex<Option<Status>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let inner_t = inner.clone();
        let stop_t = stop.clone();
        let err_t = last_error.clone();
        let handle = thread::Builder::new()
            .name("meander-gharial-poller".into())
            .spawn(move || {
                while !stop_t.load(Ordering::Relaxed) {
                    match client.status() {
                        Ok(s) => {
                            *inner_t.lock().unwrap() = Some(s);
                            *err_t.lock().unwrap() = None;
                        }
                        Err(e) => {
                            *err_t.lock().unwrap() = Some(e.to_string());
                        }
                    }
                    sleep_interruptible(&stop_t, interval);
                }
            })
            .expect("spawn poller thread");
        Self {
            inner,
            stop,
            handle: Some(handle),
            last_error,
        }
    }

    /// Latest successful snapshot, or `None` if we haven't gotten one yet.
    pub fn latest(&self) -> Option<Status> {
        self.inner.lock().unwrap().clone()
    }

    pub fn latest_or_default(&self) -> Status {
        self.latest().unwrap_or_default()
    }

    /// Most recent error string, if the last fetch failed.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }
}

impl Drop for StatusPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn sleep_interruptible(stop: &Arc<AtomicBool>, total: Duration) {
    // Wake every 50ms so a drop doesn't have to wait for the full interval.
    let step = Duration::from_millis(50).min(total);
    let mut left = total;
    while left > Duration::ZERO && !stop.load(Ordering::Relaxed) {
        let s = step.min(left);
        thread::sleep(s);
        left = left.saturating_sub(s);
    }
}

// `Gharial::poll_status` lives on Gharial directly; this trait is unused.
#[allow(dead_code)]
fn _result_alias_marker() -> Result<()> {
    Ok(())
}

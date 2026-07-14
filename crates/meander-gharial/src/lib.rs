//! Client for the gharial window manager's IPC socket.
//!
//! Gharial speaks a single-line request / single-line response protocol on a
//! Unix socket. This crate re-implements the wire format (kept deliberately
//! source-compatible with the `gharial-ipc` crate that ships in the gharial
//! repo) and adds:
//!
//! - a typed [`Gharial`] handle that resolves the socket path the same way
//!   `gharialctl` does (`$GHARIAL_SOCKET`, then
//!   `$XDG_RUNTIME_DIR/gharial-$WAYLAND_DISPLAY.sock`),
//! - typed wrappers for the most useful verbs (`ping`, `status`, `get`,
//!   `set`, `spawn`, tag focus, mode, ...), and
//! - [`Gharial::poll_status`] / [`Gharial::start_polling`] for surfaces that
//!   want to redraw whenever WM state changes.
//!
//! ```no_run
//! use meander_gharial::Gharial;
//! let g = Gharial::connect()?;
//! let status = g.status()?;
//! println!("mode = {}", status.mode);
//! # Ok::<(), meander_gharial::Error>(())
//! ```

mod ipc;
mod status;

pub use ipc::{Error, ParseError, Request, Response, Result, MAX_RESPONSE_BYTES};
pub use status::{PollSnapshot, Status, StatusParseError, StatusPoller, MIN_POLL_INTERVAL};

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// Environment variable consulted before XDG defaults.
pub const SOCKET_ENV: &str = "GHARIAL_SOCKET";
/// Socket basename used when constructing the XDG path.
pub const SOCKET_BASENAME: &str = "gharial";

/// Resolve gharial's socket path the same way `gharialctl` does.
///
/// Precedence:
/// 1. `$GHARIAL_SOCKET`
/// 2. `$XDG_RUNTIME_DIR/gharial-$WAYLAND_DISPLAY.sock`
/// 3. `$XDG_RUNTIME_DIR/gharial.sock`
/// 4. `/tmp/gharial-$USER.sock`
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os(SOCKET_ENV) {
        return PathBuf::from(p);
    }
    let basename = match std::env::var_os("WAYLAND_DISPLAY") {
        Some(d) if !d.is_empty() => {
            let mut s = OsString::from(SOCKET_BASENAME);
            s.push("-");
            s.push(&d);
            s.push(".sock");
            s
        }
        _ => OsString::from(format!("{SOCKET_BASENAME}.sock")),
    };
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let mut p = PathBuf::from(dir);
        p.push(basename);
        return p;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
    PathBuf::from(format!("/tmp/{SOCKET_BASENAME}-{user}.sock"))
}

/// Typed handle to a running gharial daemon.
///
/// Each method opens a fresh connection (`gharialctl`-style), sends one
/// request, reads one response, closes. Cheap (<1ms locally) but not free —
/// for tight loops, prefer [`Gharial::start_polling`].
#[derive(Debug, Clone)]
pub struct Gharial {
    socket: PathBuf,
    timeout: Duration,
}

impl Gharial {
    /// Open a handle using the same socket-resolution rules as `gharialctl`.
    pub fn connect() -> Result<Self> {
        let s = Self {
            socket: socket_path(),
            timeout: Duration::from_millis(500),
        };
        s.ping()?;
        Ok(s)
    }

    /// Open a handle without doing a `ping`, useful when the daemon may not be
    /// up yet and you want to poll for readiness yourself.
    pub fn with_socket(path: impl Into<PathBuf>) -> Self {
        Self {
            socket: path.into(),
            timeout: Duration::from_millis(500),
        }
    }

    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    /// Set the per-request timeout (default 500ms). Applies to subsequent
    /// calls; in-flight calls are unaffected.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Block until the daemon answers `ping`, or `timeout` elapses.
    ///
    /// Every probe and sleep is capped to the time *remaining* to the overall
    /// deadline, so a per-request timeout longer than `timeout` (the default is
    /// 500 ms) can never overrun the budget the caller asked for.
    pub fn wait_until_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let poll_gap = Duration::from_millis(25);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            // Never let a single probe outlast the caller's deadline.
            if self.ping_within(remaining.min(self.timeout)).is_ok() {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            std::thread::sleep(poll_gap.min(remaining));
        }
    }

    /// Low-level: send any command (the verb + args you'd pass to
    /// `gharialctl`) and read the parsed response.
    pub fn request(&self, command: &str, args: &[&str]) -> Result<Response> {
        self.request_within(command, args, self.timeout)
    }

    fn request_within(&self, command: &str, args: &[&str], timeout: Duration) -> Result<Response> {
        let req = Request {
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        };
        ipc::send_one(&self.socket, &req, timeout)
    }

    fn ping_within(&self, timeout: Duration) -> Result<()> {
        match self.request_within("ping", &[], timeout)? {
            Response::Ok(_) => Ok(()),
            Response::Err(e) => Err(Error::Daemon(e)),
        }
    }

    /// `ping` — succeeds when the daemon is reachable.
    pub fn ping(&self) -> Result<()> {
        match self.request("ping", &[])? {
            Response::Ok(_) => Ok(()),
            Response::Err(e) => Err(Error::Daemon(e)),
        }
    }

    /// Print the daemon's version string.
    pub fn version(&self) -> Result<String> {
        match self.request("version", &[])? {
            Response::Ok(b) => Ok(b),
            Response::Err(e) => Err(Error::Daemon(e)),
        }
    }

    /// Get a single layout parameter (`get <key>`).
    pub fn get(&self, key: &str) -> Result<String> {
        match self.request("get", &[key])? {
            Response::Ok(b) => Ok(b),
            Response::Err(e) => Err(Error::Daemon(e)),
        }
    }

    /// Set a layout parameter (`set <key> <value>`). Supports the same
    /// absolute / relative-add / relative-subtract forms `gharialctl` does.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        match self.request("set", &[key, value])? {
            Response::Ok(_) => Ok(()),
            Response::Err(e) => Err(Error::Daemon(e)),
        }
    }

    /// Full key=value snapshot of the daemon's parameters.
    ///
    /// Parses strictly: a malformed known field (bad mask, integer, or ratio)
    /// is an [`Error::Status`] rather than a silently-coerced value. For the
    /// permissive behaviour use [`Gharial::status_lossy`].
    pub fn status(&self) -> Result<Status> {
        let body = match self.request("status", &[])? {
            Response::Ok(b) => b,
            Response::Err(e) => return Err(Error::Daemon(e)),
        };
        Ok(Status::try_parse(&body)?)
    }

    /// Like [`Gharial::status`] but tolerant: malformed known fields are coerced
    /// to defaults instead of failing. Handy against an older/newer daemon whose
    /// status line you would rather read approximately than not at all.
    pub fn status_lossy(&self) -> Result<Status> {
        let body = match self.request("status", &[])? {
            Response::Ok(b) => b,
            Response::Err(e) => return Err(Error::Daemon(e)),
        };
        Ok(Status::parse_lossy(&body))
    }

    /// Launch a program detached, as gharialctl would.
    pub fn spawn(&self, argv: &[&str]) -> Result<()> {
        if argv.is_empty() {
            return Err(Error::BadArgs("spawn requires at least one argument"));
        }
        match self.request("spawn", argv)? {
            Response::Ok(_) => Ok(()),
            Response::Err(e) => Err(Error::Daemon(e)),
        }
    }

    /// `tag focus N` — show only tag N (1..=32).
    pub fn tag_focus(&self, n: u32) -> Result<()> {
        self.tag("focus", n)
    }

    /// `tag toggle N` — add/remove tag N from the active set.
    pub fn tag_toggle(&self, n: u32) -> Result<()> {
        self.tag("toggle", n)
    }

    /// `tag move N` — send the focused window to tag N.
    pub fn tag_move(&self, n: u32) -> Result<()> {
        self.tag("move", n)
    }

    /// `tag window-toggle N` — add/remove tag N from the focused window.
    pub fn tag_window_toggle(&self, n: u32) -> Result<()> {
        self.tag("window-toggle", n)
    }

    fn tag(&self, action: &str, n: u32) -> Result<()> {
        // gharial models exactly 32 tags, 1-indexed; anything else would be
        // rejected by the daemon, so fail locally with a typed error instead of
        // a round-trip.
        if !(1..=32).contains(&n) {
            return Err(Error::BadArgs("tag number must be in 1..=32"));
        }
        let n_str = n.to_string();
        match self.request("tag", &[action, &n_str])? {
            Response::Ok(_) => Ok(()),
            Response::Err(e) => Err(Error::Daemon(e)),
        }
    }

    /// `mode <name>` — enter a named binding mode (or "exit" to return).
    pub fn enter_mode(&self, name: &str) -> Result<()> {
        match self.request("mode", &[name])? {
            Response::Ok(_) => Ok(()),
            Response::Err(e) => Err(Error::Daemon(e)),
        }
    }

    /// Build a background poller that fetches `status` every `interval`. The
    /// returned [`StatusPoller`] owns a thread; drop it to stop polling.
    ///
    /// A sub-minimum interval is clamped up to [`MIN_POLL_INTERVAL`] so it
    /// cannot spin the socket. Panics if the OS refuses an eventfd or a thread —
    /// use [`try_start_polling`](Self::try_start_polling) to handle that.
    pub fn start_polling(&self, interval: Duration) -> StatusPoller {
        StatusPoller::new(self.clone(), interval)
    }

    /// Fallible poller constructor: rejects a zero/sub-minimum interval with
    /// [`Error::BadArgs`] and returns OS eventfd/thread-spawn failures instead
    /// of panicking.
    pub fn try_start_polling(&self, interval: Duration) -> Result<StatusPoller> {
        StatusPoller::try_new(self.clone(), interval)
    }

    /// Single-shot poll: returns the current status or an error.
    pub fn poll_status(&self) -> Result<Status> {
        self.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `std::env` is process-global; serialise these tests so they don't see
    // each other's mutations.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        keys: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn save(keys: &[&'static str]) -> Self {
            Self {
                keys: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
            }
        }

        fn set(&self, key: &str, value: &str) {
            // SAFETY: tests run under ENV_LOCK so no other thread observes
            // a torn read of process env state.
            unsafe { std::env::set_var(key, value) };
        }

        fn unset(&self, key: &str) {
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.keys {
                unsafe {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    const KEYS: &[&str] = &[
        "GHARIAL_SOCKET",
        "XDG_RUNTIME_DIR",
        "WAYLAND_DISPLAY",
        "USER",
    ];

    #[test]
    fn tag_helpers_reject_out_of_range_without_touching_the_socket() {
        // `with_socket` never connects, so these calls exercise only the local
        // range check; a bad tag must fail before any I/O.
        let g = Gharial::with_socket("/nonexistent/gharial.sock");
        assert!(matches!(g.tag_focus(0), Err(Error::BadArgs(_))));
        assert!(matches!(g.tag_focus(33), Err(Error::BadArgs(_))));
        assert!(matches!(g.tag_toggle(100), Err(Error::BadArgs(_))));
        assert!(matches!(g.tag_move(0), Err(Error::BadArgs(_))));
        assert!(matches!(g.tag_window_toggle(33), Err(Error::BadArgs(_))));
    }

    #[test]
    fn gharial_socket_env_wins() {
        let _lock = ENV_LOCK.lock().unwrap();
        let g = EnvGuard::save(KEYS);
        g.set("GHARIAL_SOCKET", "/explicit/path.sock");
        g.set("XDG_RUNTIME_DIR", "/should/be/ignored");
        g.set("WAYLAND_DISPLAY", "wayland-7");
        assert_eq!(socket_path(), PathBuf::from("/explicit/path.sock"));
    }

    #[test]
    fn xdg_runtime_dir_with_wayland_display() {
        let _lock = ENV_LOCK.lock().unwrap();
        let g = EnvGuard::save(KEYS);
        g.unset("GHARIAL_SOCKET");
        g.set("XDG_RUNTIME_DIR", "/run/user/1000");
        g.set("WAYLAND_DISPLAY", "wayland-2");
        assert_eq!(
            socket_path(),
            PathBuf::from("/run/user/1000/gharial-wayland-2.sock"),
        );
    }

    #[test]
    fn xdg_runtime_dir_without_wayland_display() {
        let _lock = ENV_LOCK.lock().unwrap();
        let g = EnvGuard::save(KEYS);
        g.unset("GHARIAL_SOCKET");
        g.set("XDG_RUNTIME_DIR", "/run/user/42");
        g.unset("WAYLAND_DISPLAY");
        assert_eq!(socket_path(), PathBuf::from("/run/user/42/gharial.sock"));
    }

    #[test]
    fn xdg_runtime_dir_treats_empty_wayland_display_as_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let g = EnvGuard::save(KEYS);
        g.unset("GHARIAL_SOCKET");
        g.set("XDG_RUNTIME_DIR", "/run/user/42");
        g.set("WAYLAND_DISPLAY", "");
        assert_eq!(socket_path(), PathBuf::from("/run/user/42/gharial.sock"));
    }

    #[test]
    fn falls_back_to_tmp_with_user_when_xdg_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        let g = EnvGuard::save(KEYS);
        g.unset("GHARIAL_SOCKET");
        g.unset("XDG_RUNTIME_DIR");
        g.set("USER", "alice");
        assert_eq!(socket_path(), PathBuf::from("/tmp/gharial-alice.sock"));
    }

    #[test]
    fn falls_back_to_tmp_with_default_user_when_user_missing() {
        let _lock = ENV_LOCK.lock().unwrap();
        let g = EnvGuard::save(KEYS);
        g.unset("GHARIAL_SOCKET");
        g.unset("XDG_RUNTIME_DIR");
        g.unset("USER");
        assert_eq!(socket_path(), PathBuf::from("/tmp/gharial-default.sock"));
    }
}

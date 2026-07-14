//! Wire format for the gharial IPC socket.
//!
//! Stays line-compatible with the `gharial-ipc` crate vendored inside the
//! gharial repo: single newline-terminated request, single newline-terminated
//! `ok`/`err` response, double-quoted tokens with `\\` and `\"` escapes.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use thiserror::Error;

/// Maximum bytes read for a single response line before giving up. A gharial
/// `ok`/`err` line is short; anything past this is a misbehaving or malicious
/// peer, not a real answer.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Smallest socket read timeout we ever set. A `Duration::ZERO` timeout means
/// "block forever" on some platforms, so we floor tiny remaining budgets here
/// and let the absolute deadline decide when to give up.
const MIN_READ_TIMEOUT: Duration = Duration::from_millis(1);

#[derive(Debug, Error)]
pub enum Error {
    #[error("io while talking to gharial at {socket}: {source}")]
    Io {
        socket: String,
        #[source]
        source: io::Error,
    },
    #[error("could not parse a response from gharial: {0}")]
    Parse(#[from] ParseError),
    #[error("could not parse gharial status: {0}")]
    Status(#[from] crate::status::StatusParseError),
    #[error("gharial daemon refused the request: {0}")]
    Daemon(String),
    #[error("gharial did not answer within the configured timeout")]
    Timeout,
    #[error("bad arguments: {0}")]
    BadArgs(&'static str),
    #[error("gharial response exceeded the {max}-byte maximum without a newline")]
    Oversized { max: usize },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub command: String,
    pub args: Vec<String>,
}

impl Request {
    /// Check the command is a single safe token before it is framed onto the
    /// wire. Args are quote-escaped by the token encoder and are always safe,
    /// but the command verb is written verbatim, so an empty, whitespace-, or
    /// control-character-containing command could frame an unintended second
    /// request (or a truncated one). Reject those up front.
    pub fn validate(&self) -> Result<()> {
        if self.command.is_empty() {
            return Err(Error::BadArgs("command must not be empty"));
        }
        if self
            .command
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(Error::BadArgs(
                "command must not contain whitespace or control characters",
            ));
        }
        Ok(())
    }

    /// Encode to the newline-terminated wire form. Does not validate; callers
    /// that put the bytes on a socket should use [`Request::validate`] first
    /// (as `send_one` does).
    pub fn encode(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.command);
        for a in &self.args {
            out.push(' ');
            push_token(&mut out, a);
        }
        out.push('\n');
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ok(String),
    Err(String),
}

impl Response {
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Err(ParseError::Empty);
        }
        let (tag, rest) = match line.find(' ') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, ""),
        };
        let body = unescape(rest);
        match tag {
            "ok" => Ok(Self::Ok(body)),
            "err" => Ok(Self::Err(body)),
            other => Err(ParseError::BadTag(other.into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("response was empty")]
    Empty,
    #[error("response tag was neither `ok` nor `err`: {0}")]
    BadTag(String),
}

pub fn send_one(path: &Path, req: &Request, timeout: Duration) -> Result<Response> {
    req.validate()?;
    let socket_str = path.display().to_string();
    let io_err = |e: io::Error| Error::Io {
        socket: socket_str.clone(),
        source: e,
    };

    // One absolute deadline for the whole exchange. Every socket timeout below
    // is derived from the time *remaining* to it, so a slow trickle of bytes
    // can never extend the overall budget.
    let deadline = Instant::now() + timeout;

    let mut stream = UnixStream::connect(path).map_err(&io_err)?;
    set_deadline_timeouts(&stream, deadline).map_err(&io_err)?;
    stream
        .write_all(req.encode().as_bytes())
        .map_err(map_timeout(&io_err))?;
    stream.flush().map_err(map_timeout(&io_err))?;

    // The daemon writes one line then keeps the socket open; read up to the
    // first newline, bounded by both MAX_RESPONSE_BYTES and the deadline.
    let mut line = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => return Err(Error::Timeout),
        };
        stream
            .set_read_timeout(Some(remaining.max(MIN_READ_TIMEOUT)))
            .map_err(&io_err)?;

        match stream.read(&mut chunk) {
            Ok(0) => {
                // EOF. Parse whatever we have; a fully empty response maps to
                // ParseError::Empty rather than a bare I/O error.
                break;
            }
            Ok(n) => {
                if let Some(nl) = chunk[..n].iter().position(|&b| b == b'\n') {
                    line.extend_from_slice(&chunk[..=nl]);
                    break;
                }
                line.extend_from_slice(&chunk[..n]);
                if line.len() > MAX_RESPONSE_BYTES {
                    return Err(Error::Oversized {
                        max: MAX_RESPONSE_BYTES,
                    });
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(Error::Timeout);
            }
            Err(e) => return Err(io_err(e)),
        }
    }

    let text = String::from_utf8_lossy(&line);
    Ok(Response::parse(&text)?)
}

/// Set read+write timeouts from the time remaining to `deadline`.
fn set_deadline_timeouts(stream: &UnixStream, deadline: Instant) -> io::Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO)
        .max(MIN_READ_TIMEOUT);
    stream.set_read_timeout(Some(remaining))?;
    stream.set_write_timeout(Some(remaining))?;
    Ok(())
}

/// Map a WouldBlock/TimedOut write error to `Error::Timeout`, else to the given
/// I/O error constructor.
fn map_timeout<'a>(io_err: &'a impl Fn(io::Error) -> Error) -> impl Fn(io::Error) -> Error + 'a {
    move |e: io::Error| {
        if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut {
            Error::Timeout
        } else {
            io_err(e)
        }
    }
}

// ---- token helpers (must stay byte-identical with gharial's encoder) ----

fn push_token(out: &mut String, tok: &str) {
    let needs_quote = tok.is_empty()
        || tok
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\\' || c == '\n');
    if !needs_quote {
        out.push_str(tok);
        return;
    }
    out.push('"');
    for c in tok.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.encode().trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_simple() {
        let r = Request {
            command: "set".into(),
            args: vec!["main-ratio".into(), "0.55".into()],
        };
        assert_eq!(r.encode(), "set main-ratio 0.55\n");
    }

    #[test]
    fn validate_accepts_a_plain_command() {
        let r = Request {
            command: "status".into(),
            args: vec![],
        };
        assert!(r.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_command() {
        let r = Request {
            command: String::new(),
            args: vec![],
        };
        assert!(matches!(r.validate(), Err(Error::BadArgs(_))));
    }

    #[test]
    fn validate_rejects_command_with_newline() {
        // A newline in the command would frame a second request on the wire.
        let r = Request {
            command: "ping\nset gaps 999".into(),
            args: vec![],
        };
        assert!(matches!(r.validate(), Err(Error::BadArgs(_))));
    }

    #[test]
    fn validate_rejects_command_with_space() {
        let r = Request {
            command: "set gaps".into(),
            args: vec![],
        };
        assert!(matches!(r.validate(), Err(Error::BadArgs(_))));
    }

    #[test]
    fn validate_rejects_control_characters() {
        let r = Request {
            command: "pi\tng".into(),
            args: vec![],
        };
        assert!(matches!(r.validate(), Err(Error::BadArgs(_))));
    }

    #[test]
    fn round_trip_quoted() {
        let r = Request {
            command: "bind".into(),
            args: vec!["super+q".into(), "spawn rio -e nvim foo".into()],
        };
        let enc = r.encode();
        assert!(enc.contains("\"spawn rio -e nvim foo\""));
    }

    #[test]
    fn parse_responses() {
        assert_eq!(
            Response::parse("ok hello").unwrap(),
            Response::Ok("hello".into())
        );
        assert_eq!(
            Response::parse("err nope").unwrap(),
            Response::Err("nope".into())
        );
        assert!(matches!(
            Response::parse("wat please").unwrap_err(),
            ParseError::BadTag(_)
        ));
    }

    #[test]
    fn parse_strips_trailing_newline_and_cr() {
        assert_eq!(
            Response::parse("ok hi\r\n").unwrap(),
            Response::Ok("hi".into())
        );
        assert_eq!(
            Response::parse("ok hi\n").unwrap(),
            Response::Ok("hi".into())
        );
    }

    #[test]
    fn parse_ok_with_no_body() {
        assert_eq!(Response::parse("ok").unwrap(), Response::Ok("".into()));
        assert_eq!(Response::parse("err").unwrap(), Response::Err("".into()));
    }

    #[test]
    fn parse_empty_line_is_empty_error() {
        assert!(matches!(Response::parse(""), Err(ParseError::Empty)));
        assert!(matches!(Response::parse("\n"), Err(ParseError::Empty)));
        assert!(matches!(Response::parse("\r\n"), Err(ParseError::Empty)));
    }

    #[test]
    fn unescape_handles_all_escape_sequences() {
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape(r"a\nb"), "a\nb");
        assert_eq!(unescape(r"a\rb"), "a\rb");
        assert_eq!(unescape(r"a\\b"), "a\\b");
        assert_eq!(unescape(r#"a\"b"#), "a\"b");
    }

    #[test]
    fn unescape_passes_through_unknown_escapes() {
        // Unknown escapes keep both the backslash and the next char so we
        // never silently drop user data.
        assert_eq!(unescape(r"a\xb"), r"a\xb");
    }

    #[test]
    fn unescape_keeps_dangling_backslash() {
        assert_eq!(unescape("trail\\"), "trail\\");
    }

    #[test]
    fn quote_round_trip_through_unescape() {
        // Encoder wraps tokens that need quoting; decoder must invert the
        // escape sequences so a round-trip is lossless.
        let mut encoded = String::new();
        push_token(&mut encoded, "say \"hi\"");
        // strip the surrounding quotes that push_token adds (the response
        // decoder runs over the unquoted body).
        let stripped = encoded
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap();
        assert_eq!(unescape(stripped), "say \"hi\"");
    }
}

/// Integration tests against a fake gharial daemon backed by a real
/// `UnixListener`, so the transport, framing, bounding, and timeout logic is
/// exercised end-to-end without a live desktop session.
#[cfg(test)]
mod server_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::Instant;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Owns the socket path and unlinks it on drop.
    struct Server {
        path: PathBuf,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Spawn a one-shot server whose `handler` runs against the accepted
    /// connection. Returns a guard owning the socket path.
    fn serve<F>(handler: F) -> Server
    where
        F: FnOnce(UnixStream) + Send + 'static,
    {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "meander-gharial-test-{}-{}.sock",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind test socket");
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handler(stream);
            }
        });
        Server { path }
    }

    fn status_req() -> Request {
        Request {
            command: "status".into(),
            args: vec![],
        }
    }

    #[test]
    fn single_write_success() {
        let s = serve(|mut stream| {
            let _ = stream.write_all(b"ok hello\n");
        });
        let resp = send_one(&s.path, &status_req(), Duration::from_secs(2)).unwrap();
        assert_eq!(resp, Response::Ok("hello".into()));
    }

    #[test]
    fn fragmented_success_is_reassembled() {
        let s = serve(|mut stream| {
            let _ = stream.write_all(b"ok hel");
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(20));
            let _ = stream.write_all(b"lo\n");
        });
        let resp = send_one(&s.path, &status_req(), Duration::from_secs(2)).unwrap();
        assert_eq!(resp, Response::Ok("hello".into()));
    }

    #[test]
    fn daemon_error_is_returned_as_err_response() {
        let s = serve(|mut stream| {
            let _ = stream.write_all(b"err nope\n");
        });
        let resp = send_one(&s.path, &status_req(), Duration::from_secs(2)).unwrap();
        assert_eq!(resp, Response::Err("nope".into()));
    }

    #[test]
    fn eof_without_data_is_empty_error() {
        let s = serve(|mut stream| {
            // Consume the request so the client's write succeeds, then close
            // without answering.
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
            drop(stream);
        });
        let err = send_one(&s.path, &status_req(), Duration::from_secs(2)).unwrap_err();
        assert!(
            matches!(err, Error::Parse(ParseError::Empty)),
            "got {err:?}"
        );
    }

    #[test]
    fn oversized_response_without_newline_is_rejected() {
        let s = serve(|mut stream| {
            let junk = vec![b'x'; MAX_RESPONSE_BYTES + 4096];
            let _ = stream.write_all(&junk);
            // Keep the connection open a moment so the client reads it all.
            thread::sleep(Duration::from_millis(50));
        });
        let err = send_one(&s.path, &status_req(), Duration::from_secs(2)).unwrap_err();
        assert!(matches!(err, Error::Oversized { .. }), "got {err:?}");
    }

    #[test]
    fn slow_trickle_hits_the_absolute_deadline() {
        let s = serve(|mut stream| {
            // Dribble bytes forever, never a newline. The absolute deadline must
            // stop the client regardless of ongoing activity.
            for _ in 0..1000 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(30));
            }
        });
        let start = Instant::now();
        let err = send_one(&s.path, &status_req(), Duration::from_millis(120)).unwrap_err();
        assert!(matches!(err, Error::Timeout), "got {err:?}");
        // Bounded: the trickle must not extend the deadline indefinitely.
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn no_response_times_out() {
        let s = serve(|stream| {
            // Hold the connection open without answering past the client budget.
            thread::sleep(Duration::from_millis(400));
            drop(stream);
        });
        let start = Instant::now();
        let err = send_one(&s.path, &status_req(), Duration::from_millis(100)).unwrap_err();
        assert!(matches!(err, Error::Timeout), "got {err:?}");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn shutdown_after_partial_line_parses_what_arrived() {
        let s = serve(|mut stream| {
            // Write a complete line then close mid-stream (no trailing data).
            let _ = stream.write_all(b"ok partial\n");
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
        let resp = send_one(&s.path, &status_req(), Duration::from_secs(2)).unwrap();
        assert_eq!(resp, Response::Ok("partial".into()));
    }
}

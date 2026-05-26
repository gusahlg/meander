//! Wire format for the gharial IPC socket.
//!
//! Stays line-compatible with the `gharial-ipc` crate vendored inside the
//! gharial repo: single newline-terminated request, single newline-terminated
//! `ok`/`err` response, double-quoted tokens with `\\` and `\"` escapes.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use thiserror::Error;

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
    #[error("gharial daemon refused the request: {0}")]
    Daemon(String),
    #[error("gharial did not answer within the configured timeout")]
    Timeout,
    #[error("bad arguments: {0}")]
    BadArgs(&'static str),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub command: String,
    pub args: Vec<String>,
}

impl Request {
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
    let socket_str = path.display().to_string();
    let mut stream = UnixStream::connect(path).map_err(|e| Error::Io {
        socket: socket_str.clone(),
        source: e,
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| Error::Io {
            socket: socket_str.clone(),
            source: e,
        })?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| Error::Io {
            socket: socket_str.clone(),
            source: e,
        })?;
    stream
        .write_all(req.encode().as_bytes())
        .map_err(|e| Error::Io {
            socket: socket_str.clone(),
            source: e,
        })?;
    stream.flush().map_err(|e| Error::Io {
        socket: socket_str.clone(),
        source: e,
    })?;
    // The daemon writes one line then keeps the socket open; we read up to a
    // newline.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(|e| {
        if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut {
            Error::Timeout
        } else {
            Error::Io {
                socket: socket_str.clone(),
                source: e,
            }
        }
    })?;
    if n == 0 {
        return Err(Error::Io {
            socket: socket_str,
            source: io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed without answering"),
        });
    }
    Ok(Response::parse(&line)?)
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

// Silence the unused-import warning when `Read` would otherwise look unused.
fn _unused_read_trait(_: &mut dyn Read) {}

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
}

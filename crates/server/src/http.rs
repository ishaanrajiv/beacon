//! HTTP/1.1 over std's sockets, as Trail does it: a request line and headers, a body with a
//! length or in chunks, keep-alive, `Expect: 100-continue`. Each connection has a thread that
//! mostly waits; at most [`MAX_CONNECTIONS`] are open at once. Every wait has a limit.

use beacon_core::Problem;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The longest a request line and headers may be.
const MAX_HEAD: usize = 16 * 1024;
/// Connections open at once. One more is answered `503` and closed.
pub const MAX_CONNECTIONS: usize = 64;
const CHUNK: usize = 16 * 1024;
/// Between requests on a kept-open connection.
const IDLE: Duration = Duration::from_secs(60);
/// From a request's first byte to the end of its headers, and for its body.
const ARRIVE: Duration = Duration::from_secs(30);
const WRITE: Duration = Duration::from_secs(30);

/// One request, read whole.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Lowercased names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: SocketAddr,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// An answer: a status, a JSON body (none for `304`), and extra headers.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Option<Value>,
    pub headers: Vec<(&'static str, String)>,
}

impl Reply {
    pub fn json(status: u16, body: Value) -> Reply {
        Reply {
            status,
            body: Some(body),
            headers: Vec::new(),
        }
    }

    pub fn ok(body: Value) -> Reply {
        Reply::json(200, body)
    }

    /// Beacon's error shape (`spec/api.md`, "Errors").
    pub fn problems(status: u16, problems: &[Problem]) -> Reply {
        let first = problems
            .first()
            .cloned()
            .unwrap_or_else(|| Problem::new("request", "the request failed", "report it as a bug"));
        Reply::json(status, json!({"error": first, "problems": problems}))
    }

    pub fn problem(status: u16, at: &str, message: impl Into<String>, fix: impl Into<String>) -> Reply {
        Reply::problems(status, &[Problem::new(at, message, fix)])
    }

    pub fn with(mut self, name: &'static str, value: String) -> Reply {
        self.headers.push((name, value));
        self
    }
}

/// Decodes `%41` and `+` in a query string part.
fn decode(text: &str, plus: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' if plus => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a request target into its path and decoded query parameters.
pub fn split_url(url: &str) -> (String, Vec<(String, String)>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let pairs = query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (decode(key, true), decode(value, true))
        })
        .collect();
    (decode(path, false), pairs)
}

/// The connections open now, so stopping the server can close them.
#[derive(Default)]
pub(crate) struct Open {
    streams: Mutex<HashMap<u64, TcpStream>>,
}

impl Open {
    pub(crate) fn close_all(&self) {
        for stream in crate::lock(&self.streams).values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

struct Seat {
    open: Arc<Open>,
    id: u64,
}

impl Drop for Seat {
    fn drop(&mut self) {
        crate::lock(&self.open.streams).remove(&self.id);
    }
}

enum Stop {
    Closed,
    Refuse(Reply),
}

fn refuse(status: u16, message: impl Into<String>, fix: &str) -> Stop {
    Stop::Refuse(Reply::problem(status, "request", message, fix))
}

struct Connection {
    stream: TcpStream,
    peer: SocketAddr,
    buf: Vec<u8>,
}

impl Connection {
    fn more(&mut self, deadline: Instant) -> Result<(), Stop> {
        let slow = || {
            refuse(
                408,
                "the request took too long to arrive",
                "send the whole request without pausing",
            )
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(slow());
        }
        if self.stream.set_read_timeout(Some(left)).is_err() {
            return Err(Stop::Closed);
        }
        let before = self.buf.len();
        self.buf.resize(before + CHUNK, 0);
        let read = self.stream.read(&mut self.buf[before..]);
        self.buf.truncate(before + read.as_ref().map_or(0, |n| *n));
        match read {
            Ok(0) => Err(Stop::Closed),
            Ok(_) => Ok(()),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Err(slow()),
            Err(_) => Err(Stop::Closed),
        }
    }

    fn take(&mut self, mut n: usize, out: &mut Vec<u8>, deadline: Instant) -> Result<(), Stop> {
        while n > 0 {
            if self.buf.is_empty() {
                self.more(deadline)?;
            }
            let some = n.min(self.buf.len());
            out.extend_from_slice(&self.buf[..some]);
            self.buf.drain(..some);
            n -= some;
        }
        Ok(())
    }

    fn line(&mut self, deadline: Instant) -> Result<String, Stop> {
        loop {
            if let Some(end) = self.buf.windows(2).position(|pair| pair == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[..end]).into_owned();
                self.buf.drain(..end + 2);
                return Ok(line);
            }
            if self.buf.len() > 1024 {
                return Err(refuse(
                    400,
                    "a chunk of the body has no size line",
                    "send the body with a Content-Length",
                ));
            }
            self.more(deadline)?;
        }
    }

    /// Reads one request; the bool is "close after answering".
    fn next(&mut self, limit: usize) -> Result<(Request, bool), Stop> {
        if self.buf.is_empty() {
            self.more(Instant::now() + IDLE).map_err(|_| Stop::Closed)?;
        }
        let deadline = Instant::now() + ARRIVE;
        let end = loop {
            let found = self.buf.windows(4).position(|four| four == b"\r\n\r\n");
            if found.unwrap_or(self.buf.len()) > MAX_HEAD {
                return Err(refuse(
                    431,
                    format!("the request line and headers are over {MAX_HEAD} bytes"),
                    "send fewer or shorter headers",
                ));
            }
            if let Some(end) = found {
                break end;
            }
            self.more(deadline)?;
        };
        let malformed = |what: &str| {
            refuse(
                400,
                format!("this is not an HTTP/1.1 request: {what}"),
                "send `METHOD /path HTTP/1.1`, then `Name: value` headers, each line ending CRLF",
            )
        };
        let head = std::str::from_utf8(&self.buf[..end])
            .map_err(|_| malformed("the headers are not UTF-8"))?
            .to_string();
        self.buf.drain(..end + 4);
        let mut lines = head.split("\r\n");
        let mut first = lines.next().unwrap_or_default().split(' ');
        let (Some(method), Some(target), Some(version), None) =
            (first.next(), first.next(), first.next(), first.next())
        else {
            return Err(malformed("the first line is not `METHOD /path HTTP/1.1`"));
        };
        let keeps_alive = match version {
            "HTTP/1.1" => true,
            "HTTP/1.0" => false,
            _ => return Err(malformed("the version is not HTTP/1.1")),
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                return Err(malformed("a header line has no colon"));
            };
            if name.is_empty() || name.contains(|c: char| c.is_ascii_whitespace()) {
                return Err(malformed("a header name has a space in it"));
            }
            headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
        }
        let one = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        let chunked = match one("transfer-encoding") {
            None => false,
            Some(value) if value.eq_ignore_ascii_case("chunked") => true,
            Some(other) => {
                return Err(refuse(
                    501,
                    format!("Beacon does not read a body sent as `{other}`"),
                    "send the body with a Content-Length",
                ));
            }
        };
        let mut length: Option<usize> = None;
        for (_, value) in headers.iter().filter(|(name, _)| name == "content-length") {
            let parsed: usize = value
                .parse()
                .map_err(|_| malformed("Content-Length is not a number"))?;
            if length.is_some_and(|earlier| earlier != parsed) {
                return Err(malformed("two Content-Length headers disagree"));
            }
            length = Some(parsed);
        }
        if chunked && length.is_some() {
            return Err(malformed("it has both a Content-Length and a chunked body"));
        }
        let too_large = || {
            Stop::Refuse(Reply::problem(
                413,
                "body",
                format!("the body is over {limit} bytes"),
                "send less, or raise limits.body_bytes in beacon.toml",
            ))
        };
        if length.is_some_and(|length| length > limit) {
            return Err(too_large());
        }
        if (chunked || length.is_some_and(|length| length > 0))
            && one("expect").is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
            && self.stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err()
        {
            return Err(Stop::Closed);
        }
        let deadline = Instant::now() + ARRIVE;
        let mut body = Vec::new();
        if chunked {
            loop {
                let line = self.line(deadline)?;
                let size = line.split(';').next().unwrap_or_default().trim();
                let size = usize::from_str_radix(size, 16)
                    .map_err(|_| malformed("a chunk's size is not a hexadecimal number"))?;
                if size == 0 {
                    while !self.line(deadline)?.is_empty() {}
                    break;
                }
                // Checked so a size near usize::MAX can't wrap the sum round under the limit.
                if size > limit.saturating_sub(body.len()) {
                    return Err(too_large());
                }
                self.take(size, &mut body, deadline)?;
                if !self.line(deadline)?.is_empty() {
                    return Err(malformed("a chunk is longer than its size line says"));
                }
            }
        } else if let Some(length) = length {
            body.reserve_exact(length);
            self.take(length, &mut body, deadline)?;
        }
        let close = match one("connection") {
            Some(value) if value.eq_ignore_ascii_case("close") => true,
            Some(value) if value.eq_ignore_ascii_case("keep-alive") => false,
            _ => !keeps_alive,
        };
        let (path, query) = split_url(target);
        Ok((
            Request {
                method: method.to_string(),
                path,
                query,
                headers,
                body,
                peer: self.peer,
            },
            close,
        ))
    }

    fn respond(&mut self, reply: &Reply, close: bool, with_body: bool) -> std::io::Result<()> {
        let reason = match reply.status {
            200 => "OK",
            201 => "Created",
            304 => "Not Modified",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            408 => "Request Timeout",
            409 => "Conflict",
            410 => "Gone",
            413 => "Content Too Large",
            422 => "Unprocessable Content",
            431 => "Request Header Fields Too Large",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "",
        };
        let text = match (&reply.body, reply.status) {
            (_, 304) | (None, _) => String::new(),
            (Some(body), _) => body.to_string(),
        };
        let mut out = Vec::with_capacity(text.len() + 200);
        out.extend_from_slice(format!("HTTP/1.1 {} {reason}\r\n", reply.status).as_bytes());
        if reply.status != 304 {
            out.extend_from_slice(
                format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    text.len()
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(b"Cache-Control: no-cache\r\n");
        for (name, value) in &reply.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        if close {
            out.extend_from_slice(b"Connection: close\r\n");
        }
        out.extend_from_slice(b"\r\n");
        if with_body {
            out.extend_from_slice(text.as_bytes());
        }
        self.stream.write_all(&out)
    }

    fn linger(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Write);
        let until = Instant::now() + Duration::from_secs(2);
        let mut sink = [0u8; 4096];
        while Instant::now() < until {
            let left = until
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            if self.stream.set_read_timeout(Some(left)).is_err() {
                break;
            }
            match self.stream.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
}

/// What answers a request.
pub(crate) trait Handler: Send + Sync + 'static {
    fn body_limit(&self) -> usize;
    fn handle(&self, request: Request) -> Reply;
}

fn converse(handler: &dyn Handler, stream: TcpStream, peer: SocketAddr) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(WRITE));
    let mut connection = Connection {
        stream,
        peer,
        buf: Vec::new(),
    };
    loop {
        match connection.next(handler.body_limit()) {
            Ok((mut request, close)) => {
                let head_only = request.method == "HEAD";
                if head_only {
                    request.method = "GET".into();
                }
                let reply = handler.handle(request);
                if connection.respond(&reply, close, !head_only).is_err() || close {
                    return;
                }
            }
            Err(Stop::Closed) => return,
            Err(Stop::Refuse(reply)) => {
                if connection.respond(&reply, true, true).is_ok() {
                    connection.linger();
                }
                return;
            }
        }
    }
}

/// Accepts connections until `stop` is set. Returns the accepting thread.
pub(crate) fn serve(
    handler: Arc<dyn Handler>,
    listener: TcpListener,
    open: Arc<Open>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("beacon-accept".to_string())
        .spawn(move || {
            let mut next_id = 0u64;
            for stream in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(stream) = stream else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                let Ok(peer) = stream.peer_addr() else {
                    continue;
                };
                next_id += 1;
                let id = next_id;
                {
                    let mut streams = crate::lock(&open.streams);
                    if streams.len() >= MAX_CONNECTIONS {
                        drop(streams);
                        let mut connection = Connection {
                            stream,
                            peer,
                            buf: Vec::new(),
                        };
                        let _ = connection.stream.set_write_timeout(Some(Duration::from_secs(1)));
                        let busy = Reply::problem(
                            503,
                            "connections",
                            format!("{MAX_CONNECTIONS} connections are open already"),
                            "reuse connections; try again shortly",
                        );
                        let _ = connection.respond(&busy, true, true);
                        continue;
                    }
                    if let Ok(copy) = stream.try_clone() {
                        streams.insert(id, copy);
                    }
                }
                let seat = Seat {
                    open: open.clone(),
                    id,
                };
                let handler = handler.clone();
                let _ = std::thread::Builder::new()
                    .name("beacon-http".to_string())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        let _seat = seat;
                        converse(handler.as_ref(), stream, peer);
                    });
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_and_decode() {
        let (path, query) = split_url("/v1/inbox?source=a%2Cb&class=update+x&flag");
        assert_eq!(path, "/v1/inbox");
        assert_eq!(
            query,
            vec![
                ("source".to_string(), "a,b".to_string()),
                ("class".to_string(), "update x".to_string()),
                ("flag".to_string(), String::new()),
            ]
        );
        assert_eq!(decode("100%", true), "100%");
        assert_eq!(decode("%zz", true), "%zz");
        assert_eq!(decode("a+b", false), "a+b");
        assert_eq!(decode("%4", true), "%4");
    }
}

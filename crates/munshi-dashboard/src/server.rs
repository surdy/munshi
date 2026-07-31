//! The dashboard's HTTP surface: a blocking HTTP/1.1 server over `std::net::TcpListener`, one
//! thread per connection, no async runtime and no framework.
//!
//! It is the server counterpart of Munshi's minimal HTTP client (ADR 0006) and keeps the same
//! shape deliberately: one request per connection, `Connection: close`, an explicit
//! `Content-Length` on every response, and no chunked encoding or keep-alive bookkeeping. Two
//! routes exist — the embedded page and the JSON snapshot — and everything else is 404.
//!
//! Binding is restricted to loopback addresses. That restriction is the entire security model: the
//! snapshot carries session identifiers, project names and summary titles, and nothing here
//! authenticates a caller.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::DashboardError;
use crate::collect::Collector;

/// The single-file page, embedded so the binary is the whole deployment.
const INDEX_HTML: &str = include_str!("../assets/index.html");

/// Bound on one request head. The page sends only bare `GET`s, so this leaves ample room for
/// browser headers while denying a peer any unbounded allocation.
const MAX_REQUEST_BYTES: usize = 8192;

/// Bound on reading a request head from a connected peer.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on one write to a connected peer. A browser tab that navigates away mid-response frees
/// its connection thread at this bound rather than holding it.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The routes this server answers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// The embedded dashboard page.
    Index,
    /// The JSON snapshot the page polls.
    Data,
    /// Anything else, including any method other than `GET`.
    NotFound,
}

/// Accepts the requested bind address only when it is a loopback address.
///
/// The dashboard publishes unauthenticated session metadata, so binding a routable address would
/// expose it to the network; refusing here is clearer than documenting the hazard.
pub(crate) fn loopback_bind(address: SocketAddr) -> Result<SocketAddr, DashboardError> {
    if address.ip().is_loopback() {
        Ok(address)
    } else {
        Err(DashboardError::NonLoopbackBind(address))
    }
}

/// Serves connections until the process is signalled. Accept failures are logged and the loop
/// continues: one refused connection must not take the dashboard down.
pub(crate) fn serve(listener: &TcpListener, collector: &Arc<Collector>) {
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let collector = Arc::clone(collector);
                if let Err(error) = thread::Builder::new()
                    .name("dashboard-connection".to_owned())
                    .spawn(move || handle(stream, &collector))
                {
                    eprintln!("dashboard: could not spawn a connection thread: {error}");
                }
            }
            Err(error) => eprintln!("dashboard: accept failed: {error}"),
        }
    }
}

/// Answers one request and closes the connection. Every outcome, including a peer that disconnects
/// mid-response, ends the thread quietly; only the access line reaches stdout.
fn handle(mut stream: TcpStream, collector: &Collector) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "unknown".to_owned(), |address| address.to_string());
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));

    let head = match read_head(&mut stream) {
        Ok(head) => head,
        Err(error) => {
            eprintln!("dashboard: {peer} - could not read the request: {error}");
            return;
        }
    };
    let head = String::from_utf8_lossy(&head);
    let Some((method, target)) = parse_request_line(&head) else {
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            b"bad request",
        );
        return;
    };

    let (status, content_type, body) = match route(method, target) {
        Route::Index => (
            "200 OK",
            "text/html; charset=utf-8",
            Arc::new(INDEX_HTML.as_bytes().to_vec()),
        ),
        Route::Data => ("200 OK", "application/json", collector.snapshot()),
        Route::NotFound => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            Arc::new(b"not found".to_vec()),
        ),
    };
    println!("{peer} - {method} {target} {status}");
    let _ = write_response(&mut stream, status, content_type, &body);
}

/// Routes one request. Only `GET` is served; a query string is ignored so the page's cache-busting
/// fetches route the same as a bare path.
pub(crate) fn route(method: &str, target: &str) -> Route {
    if method != "GET" {
        return Route::NotFound;
    }
    match target.split('?').next().unwrap_or(target) {
        "/" | "/index.html" => Route::Index,
        "/api/data" => Route::Data,
        _ => Route::NotFound,
    }
}

/// Splits a request head's first line into method and request-target, rejecting anything that is
/// not two non-empty space-separated tokens.
pub(crate) fn parse_request_line(head: &str) -> Option<(&str, &str)> {
    let mut tokens = head.split("\r\n").next()?.split(' ');
    let method = tokens.next().filter(|token| !token.is_empty())?;
    let target = tokens.next().filter(|token| !token.is_empty())?;
    Some((method, target))
}

/// Reads up to the blank line that ends a request head, or [`MAX_REQUEST_BYTES`], whichever comes
/// first. Request bodies are never read because no route accepts one.
fn read_head<R: Read>(stream: &mut R) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
        if head.len() >= MAX_REQUEST_BYTES || head.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    Ok(head)
}

/// Writes one complete response. `Content-Length` frames the body without chunked encoding,
/// `Connection: close` ends the connection, and `Cache-Control: no-store` keeps a snapshot that is
/// stale within 30 seconds out of the browser's cache.
fn write_response<W: Write>(
    stream: &mut W,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_addresses_are_accepted() {
        for address in ["127.0.0.1:8877", "127.0.0.53:1", "[::1]:8877"] {
            let parsed: SocketAddr = address.parse().expect("a socket address");
            assert_eq!(loopback_bind(parsed).expect("loopback is accepted"), parsed);
        }
    }

    #[test]
    fn routable_addresses_are_refused() {
        for address in ["0.0.0.0:8877", "192.168.16.169:8877", "[::]:8877"] {
            let parsed: SocketAddr = address.parse().expect("a socket address");
            let error = loopback_bind(parsed).expect_err("a routable address is refused");
            assert!(
                matches!(error, DashboardError::NonLoopbackBind(_)),
                "{error}"
            );
            assert!(error.to_string().contains("loopback"));
        }
    }

    #[test]
    fn only_the_page_and_the_snapshot_are_routed() {
        assert_eq!(route("GET", "/"), Route::Index);
        assert_eq!(route("GET", "/index.html"), Route::Index);
        assert_eq!(route("GET", "/api/data"), Route::Data);
        assert_eq!(route("GET", "/api"), Route::NotFound);
        assert_eq!(route("GET", "/api/data/"), Route::NotFound);
        assert_eq!(route("GET", "/../server.py"), Route::NotFound);
        assert_eq!(route("GET", "/favicon.ico"), Route::NotFound);
    }

    #[test]
    fn query_strings_do_not_change_the_route() {
        assert_eq!(route("GET", "/api/data?t=1700000000"), Route::Data);
        assert_eq!(route("GET", "/?scope=all"), Route::Index);
    }

    #[test]
    fn methods_other_than_get_are_not_found() {
        for method in ["POST", "PUT", "DELETE", "HEAD", "OPTIONS", "get"] {
            assert_eq!(route(method, "/api/data"), Route::NotFound, "{method}");
        }
    }

    #[test]
    fn request_lines_split_into_method_and_target() {
        let head = "GET /api/data HTTP/1.1\r\nHost: 127.0.0.1:8877\r\n\r\n";
        assert_eq!(parse_request_line(head), Some(("GET", "/api/data")));
        assert_eq!(parse_request_line(""), None);
        assert_eq!(parse_request_line("GET\r\n\r\n"), None);
        assert_eq!(parse_request_line(" /api/data HTTP/1.1\r\n"), None);
    }

    /// Hands out one chunk per `read` and fails afterwards, so a test can prove the head read
    /// stops at the blank line instead of waiting for a peer that will send nothing more.
    struct ChunkedReader {
        chunks: Vec<&'static [u8]>,
        next: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let chunk = self
                .chunks
                .get(self.next)
                .ok_or_else(|| io::Error::other("read past the request head"))?;
            self.next += 1;
            let length = chunk.len().min(buffer.len());
            buffer[..length].copy_from_slice(&chunk[..length]);
            Ok(length)
        }
    }

    #[test]
    fn reading_a_head_stops_at_the_blank_line() {
        let mut reader = ChunkedReader {
            chunks: vec![b"GET /api/data HTTP/1.1\r\nHost: 127.0.0.1:8877\r\n\r\n"],
            next: 0,
        };
        let head = read_head(&mut reader).expect("the head reads without waiting for more");
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(
            parse_request_line(&String::from_utf8_lossy(&head)),
            Some(("GET", "/api/data"))
        );
    }

    #[test]
    fn reading_a_head_reassembles_split_chunks() {
        let mut reader = ChunkedReader {
            chunks: vec![b"GET /ap", b"i/data HTTP/1.1\r\n", b"Host: x\r\n\r\n"],
            next: 0,
        };
        let head = read_head(&mut reader).expect("the split head reads");
        assert_eq!(
            parse_request_line(&String::from_utf8_lossy(&head)),
            Some(("GET", "/api/data"))
        );
    }

    #[test]
    fn reading_a_head_is_bounded() {
        let flood = vec![b'a'; 4 * MAX_REQUEST_BYTES];
        let head = read_head(&mut &flood[..]).expect("the read is bounded, not an error");
        assert!(head.len() < 2 * MAX_REQUEST_BYTES);
    }

    #[test]
    fn responses_are_framed_closed_and_uncacheable() {
        let mut written = Vec::new();
        write_response(&mut written, "200 OK", "application/json", b"{\"ok\":true}")
            .expect("writing to a buffer succeeds");
        let written = String::from_utf8(written).expect("the response is utf-8");
        assert!(written.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(written.contains("\r\nContent-Length: 11\r\n"));
        assert!(written.contains("\r\nContent-Type: application/json\r\n"));
        assert!(written.contains("\r\nCache-Control: no-store\r\n"));
        assert!(written.contains("\r\nConnection: close\r\n"));
        assert!(written.ends_with("\r\n\r\n{\"ok\":true}"));
    }

    #[test]
    fn the_embedded_page_is_the_dashboard() {
        assert!(INDEX_HTML.contains("fetch(\"/api/data\""));
        assert!(INDEX_HTML.contains("<title>"));
    }
}

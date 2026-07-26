//! A minimal synchronous HTTP/1.1 client shared by the Notesmith delivery sink and the Patwari
//! archive-upload client.
//!
//! Both peers are trusted localhost/LAN daemons, so this deliberately speaks plain HTTP over
//! `std::net` and adds no async or TLS dependency (ADR 0006). It supports arbitrary request
//! headers and raw binary request bodies with content-length framing, which the resumable
//! chunk uploads (`application/octet-stream` with custom `x-patwari-chunk-*` headers) require.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use thiserror::Error;

/// Bounds the response body this client reads from a peer.
const MAX_RESPONSE_BYTES: usize = 1_048_576;

#[derive(Debug, Error)]
pub(crate) enum HttpError {
    #[error("endpoint {0} is not a supported http URL")]
    UnsupportedEndpoint(String),
    #[error("http transport failed: {0}")]
    Transport(String),
    #[error("http protocol error: {0}")]
    Protocol(String),
}

/// One request header. Values are treated as opaque and are never placed in a format string
/// destined for a log, so a bearer credential passed here is never echoed.
pub(crate) struct Header<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

/// A single HTTP request. `headers` carries every header except `Host`, `Connection`, and
/// `Content-Length`, which this client always writes itself.
pub(crate) struct HttpRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a [Header<'a>],
    pub body: Option<&'a [u8]>,
}

pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// Response headers, names lowercased. Retrieval reads the `x-patwari-*` metadata headers the
    /// artifact-content route emits; upload only ever inspects the status line and body.
    pub headers: Vec<(String, String)>,
}

impl HttpResponse {
    /// A bounded, lossy text view of the body for safe diagnostics.
    pub(crate) fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body)
            .chars()
            .take(512)
            .collect()
    }

    /// The first value of a response header, matched case-insensitively.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Splits an `http://host[:port]` endpoint into its host and port, defaulting to port 80. Only
/// plain `http` is accepted; `https` and other schemes are rejected (trusted-network model).
pub(crate) fn parse_http_endpoint(endpoint: &str) -> Result<(String, u16), HttpError> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| HttpError::UnsupportedEndpoint(endpoint.to_owned()))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|_| HttpError::UnsupportedEndpoint(endpoint.to_owned()))?;
            (host.to_owned(), port)
        }
        None => (authority.to_owned(), 80),
    };
    if host.is_empty() {
        return Err(HttpError::UnsupportedEndpoint(endpoint.to_owned()));
    }
    Ok((host, port))
}

/// Percent-encodes a path segment string for safe inclusion in a request-target, preserving `/`.
pub(crate) fn encode_path(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Sends one request over a fresh `Connection: close` socket and reads the full response, bounding
/// the body at [`MAX_RESPONSE_BYTES`].
pub(crate) fn send(
    host: &str,
    port: u16,
    timeout: Duration,
    request: &HttpRequest<'_>,
) -> Result<HttpResponse, HttpError> {
    send_with_limit(host, port, timeout, request, MAX_RESPONSE_BYTES)
}

/// Like [`send`], but bounds the response body at `max_response_bytes`. Retrieval downloads stored
/// artifact bytes that can far exceed the default JSON-response bound, so it raises the ceiling; a
/// truncated body simply fails the mandatory stored-hash verification rather than being emitted.
pub(crate) fn send_with_limit(
    host: &str,
    port: u16,
    timeout: Duration,
    request: &HttpRequest<'_>,
    max_response_bytes: usize,
) -> Result<HttpResponse, HttpError> {
    let address = (host, port)
        .to_socket_addrs()
        .map_err(|error| HttpError::Transport(error.to_string()))?
        .next()
        .ok_or_else(|| HttpError::Transport(format!("could not resolve {host}:{port}")))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| HttpError::Transport(error.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| HttpError::Transport(error.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| HttpError::Transport(error.to_string()))?;

    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n",
        request.method, request.path,
    );
    for header in request.headers {
        head.push_str(header.name);
        head.push_str(": ");
        head.push_str(header.value);
        head.push_str("\r\n");
    }
    if let Some(body) = request.body {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .map_err(|error| HttpError::Transport(error.to_string()))?;
    if let Some(body) = request.body {
        stream
            .write_all(body)
            .map_err(|error| HttpError::Transport(error.to_string()))?;
    }
    stream
        .flush()
        .map_err(|error| HttpError::Transport(error.to_string()))?;

    let mut raw = Vec::new();
    stream
        .take(max_response_bytes as u64)
        .read_to_end(&mut raw)
        .map_err(|error| HttpError::Transport(error.to_string()))?;
    parse_http_response(&raw)
}

pub(crate) fn parse_http_response(raw: &[u8]) -> Result<HttpResponse, HttpError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Protocol("response has no header terminator".to_owned()))?;
    let head = &raw[..split];
    let body = &raw[split + 4..];
    let head = String::from_utf8_lossy(head);
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Protocol("empty response".to_owned()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Protocol("unparseable status line".to_owned()))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let chunked = headers.iter().any(|(name, value)| {
        name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok(HttpResponse {
        status,
        body,
        headers,
    })
}

pub(crate) fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut output = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| HttpError::Protocol("malformed chunk header".to_owned()))?;
        let size_text = String::from_utf8_lossy(&body[..line_end]);
        let size =
            usize::from_str_radix(size_text.trim().split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| HttpError::Protocol("malformed chunk size".to_owned()))?;
        body = &body[line_end + 2..];
        if size == 0 {
            break;
        }
        if body.len() < size {
            return Err(HttpError::Protocol("truncated chunk body".to_owned()));
        }
        output.extend_from_slice(&body[..size]);
        body = &body[size..];
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_endpoints_and_rejects_others() {
        assert_eq!(
            parse_http_endpoint("http://127.0.0.1:27183").unwrap(),
            ("127.0.0.1".to_owned(), 27183)
        );
        assert_eq!(
            parse_http_endpoint("http://localhost").unwrap(),
            ("localhost".to_owned(), 80)
        );
        assert!(parse_http_endpoint("https://example.com").is_err());
        assert!(parse_http_endpoint("ftp://host").is_err());
    }

    #[test]
    fn encode_path_preserves_slashes_and_escapes_spaces() {
        assert_eq!(encode_path("Munshi/a b.md"), "Munshi/a%20b.md");
        assert_eq!(encode_path("plain-note.md"), "plain-note.md");
    }

    #[test]
    fn dechunks_a_chunked_body() {
        let chunked = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(chunked).unwrap(), b"Wikipedia");
    }

    #[test]
    fn parses_a_content_length_response() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Length: 13\r\n\r\n{\"path\":\"x\"}\n";
        let response = parse_http_response(raw).unwrap();
        assert_eq!(response.status, 201);
    }

    #[test]
    fn exposes_response_headers_case_insensitively() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Patwari-Compression: zstd\r\n\r\nhi";
        let response = parse_http_response(raw).unwrap();
        assert_eq!(response.header("x-patwari-compression"), Some("zstd"));
        assert_eq!(response.header("X-PATWARI-COMPRESSION"), Some("zstd"));
        assert_eq!(response.header("content-length"), Some("2"));
        assert_eq!(response.header("missing"), None);
        assert_eq!(response.body, b"hi");
    }
}

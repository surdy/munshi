//! A minimal synchronous HTTP/1.1 client shared by the Notesmith delivery sink and the Patwari
//! archive-upload client.
//!
//! The client deliberately stays a blocking, one-fresh-connection-per-request `std::net`
//! design with no async dependency (ADR 0006). `https://` endpoints wrap that same stream in
//! rustls, verifying against the system trust store with no TLS policy knobs (ADR 0013) — the
//! Caddy-published `*.clusterfault.com` names are the intended peers, and plain `http://`
//! remains fully supported for localhost tunnels and trusted-LAN addresses. It supports
//! arbitrary request headers and raw binary request bodies with content-length framing, which
//! the resumable chunk uploads (`application/octet-stream` with custom `x-patwari-chunk-*`
//! headers) require.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use thiserror::Error;

/// Bounds the response body this client reads from a peer.
const MAX_RESPONSE_BYTES: usize = 1_048_576;

#[derive(Debug, Error)]
pub(crate) enum HttpError {
    #[error("endpoint {0} is not a supported http(s) URL")]
    UnsupportedEndpoint(String),
    #[error("http transport failed: {0}")]
    Transport(String),
    #[error("http protocol error: {0}")]
    Protocol(String),
    #[error("tls setup failed: {0}")]
    Tls(String),
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

/// A parsed endpoint authority: scheme (as the `tls` flag), host, and port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpEndpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

impl HttpEndpoint {
    /// The scheme's default port; the `Host` header omits it, matching what proxies and
    /// virtual hosts conventionally expect.
    fn default_port(&self) -> u16 {
        if self.tls { 443 } else { 80 }
    }
}

/// Splits an `http://host[:port]` or `https://host[:port]` endpoint into its parsed
/// authority, defaulting the port per scheme (80/443); other schemes are rejected. For
/// `https` the client performs standard certificate verification against the system trust
/// store — whatever the presented certificate authorizes (DNS or IP SANs) is accepted, and
/// there are no TLS policy knobs (ADR 0013).
pub(crate) fn parse_http_endpoint(endpoint: &str) -> Result<HttpEndpoint, HttpError> {
    let (tls, rest) = if let Some(rest) = endpoint.strip_prefix("http://") {
        (false, rest)
    } else if let Some(rest) = endpoint.strip_prefix("https://") {
        (true, rest)
    } else {
        return Err(HttpError::UnsupportedEndpoint(endpoint.to_owned()));
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|_| HttpError::UnsupportedEndpoint(endpoint.to_owned()))?;
            (host.to_owned(), port)
        }
        None => (authority.to_owned(), if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(HttpError::UnsupportedEndpoint(endpoint.to_owned()));
    }
    Ok(HttpEndpoint { tls, host, port })
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

/// The process-wide TLS client configuration: rustls on the ring provider, verifying against
/// the system trust store (`rustls-native-certs`). Built once and shared so TLS 1.3 session
/// resumption spans every request of an invocation — the chunk-upload loop and the archive
/// walk are the request-dense paths, and resumed handshakes skip full chain verification.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, HttpError> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            let mut roots = rustls::RootCertStore::empty();
            let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
            if added == 0 {
                let detail = loaded
                    .errors
                    .first()
                    .map_or_else(|| "no certificates found".to_owned(), ToString::to_string);
                return Err(format!(
                    "no usable roots in the system trust store: {detail}"
                ));
            }
            Ok(Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ))
        })
        .clone()
        .map_err(HttpError::Tls)
}

/// One request's transport: the plain socket, or that same socket wrapped in rustls. TLS
/// reads are strict about `close_notify` — a peer that closes without it surfaces as a
/// transport error rather than a silently shortened body.
enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

/// Sends one request over a fresh `Connection: close` socket and reads the full response, bounding
/// the body at [`MAX_RESPONSE_BYTES`].
pub(crate) fn send(
    endpoint: &HttpEndpoint,
    timeout: Duration,
    request: &HttpRequest<'_>,
) -> Result<HttpResponse, HttpError> {
    send_with_limit(endpoint, timeout, request, MAX_RESPONSE_BYTES)
}

/// Like [`send`], but bounds the response body at `max_response_bytes`. Retrieval downloads stored
/// artifact bytes that can far exceed the default JSON-response bound, so it raises the ceiling; a
/// truncated body simply fails the mandatory stored-hash verification rather than being emitted.
pub(crate) fn send_with_limit(
    endpoint: &HttpEndpoint,
    timeout: Duration,
    request: &HttpRequest<'_>,
    max_response_bytes: usize,
) -> Result<HttpResponse, HttpError> {
    let HttpEndpoint { host, port, .. } = endpoint;
    let address = (host.as_str(), *port)
        .to_socket_addrs()
        .map_err(|error| HttpError::Transport(error.to_string()))?
        .next()
        .ok_or_else(|| HttpError::Transport(format!("could not resolve {host}:{port}")))?;
    let tcp = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| HttpError::Transport(error.to_string()))?;
    tcp.set_read_timeout(Some(timeout))
        .map_err(|error| HttpError::Transport(error.to_string()))?;
    tcp.set_write_timeout(Some(timeout))
        .map_err(|error| HttpError::Transport(error.to_string()))?;
    let mut stream = if endpoint.tls {
        let config = tls_config()?;
        let server_name = ServerName::try_from(host.clone())
            .map_err(|_| HttpError::Tls(format!("{host} is not a valid TLS server name")))?;
        let connection = rustls::ClientConnection::new(config, server_name)
            .map_err(|error| HttpError::Tls(error.to_string()))?;
        Transport::Tls(Box::new(rustls::StreamOwned::new(connection, tcp)))
    } else {
        Transport::Plain(tcp)
    };

    let host_header = if *port == endpoint.default_port() {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n",
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
    fn parses_http_and_https_endpoints_and_rejects_others() {
        assert_eq!(
            parse_http_endpoint("http://127.0.0.1:27183").unwrap(),
            HttpEndpoint {
                tls: false,
                host: "127.0.0.1".to_owned(),
                port: 27183
            }
        );
        assert_eq!(
            parse_http_endpoint("http://localhost").unwrap(),
            HttpEndpoint {
                tls: false,
                host: "localhost".to_owned(),
                port: 80
            }
        );
        assert_eq!(
            parse_http_endpoint("https://patwari.clusterfault.com").unwrap(),
            HttpEndpoint {
                tls: true,
                host: "patwari.clusterfault.com".to_owned(),
                port: 443
            }
        );
        assert_eq!(
            parse_http_endpoint("https://192.168.16.169:8443").unwrap(),
            HttpEndpoint {
                tls: true,
                host: "192.168.16.169".to_owned(),
                port: 8443
            }
        );
        assert!(parse_http_endpoint("ftp://host").is_err());
        assert!(parse_http_endpoint("https://").is_err());
        assert!(parse_http_endpoint("patwari.clusterfault.com").is_err());
    }

    #[test]
    fn host_header_omits_only_the_scheme_default_port() {
        let https_default = parse_http_endpoint("https://example.com").unwrap();
        assert_eq!(https_default.default_port(), 443);
        let http_default = parse_http_endpoint("http://example.com").unwrap();
        assert_eq!(http_default.default_port(), 80);
        // An explicit non-default port survives into the parsed authority so the Host
        // header carries it.
        let tunneled = parse_http_endpoint("http://127.0.0.1:18787").unwrap();
        assert_ne!(tunneled.port, tunneled.default_port());
    }

    #[test]
    fn tls_server_names_accept_both_dns_names_and_ip_literals() {
        use rustls::pki_types::ServerName;
        assert!(matches!(
            ServerName::try_from("patwari.clusterfault.com".to_owned()).unwrap(),
            ServerName::DnsName(_)
        ));
        assert!(matches!(
            ServerName::try_from("192.168.16.169".to_owned()).unwrap(),
            ServerName::IpAddress(_)
        ));
        assert!(ServerName::try_from("not a hostname".to_owned()).is_err());
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

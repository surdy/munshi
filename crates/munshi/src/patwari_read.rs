//! The shared Patwari read stack (issue #42).
//!
//! Claim-ticket retrieval ([`crate::retrieve`]) and archive-wide parse verification
//! ([`crate::verify_archive`]) read the same three Patwari surfaces: cursor-paginated listings,
//! the artifact-content route with its `x-patwari-*` metadata headers, and the stable
//! `error.code` error body. Both once carried their own copy of the wire rules, which would
//! drift apart on the next header or format change; the rules now live here once.
//!
//! What is shared is the *verification*, not the error taxonomy. Every helper reports a
//! classified failure ([`ReadError`], [`DownloadError`]) that its caller maps onto its own error
//! type, message, and exit code, so the two commands' pinned contracts stay independent — one
//! command's `TooLarge` exit code and the other's skipped-not-fatal accounting line come from
//! the same refusal without either learning about the other.
//!
//! # The three-stage verified download
//!
//! [`ReadClient::download_verified`] never hands back a byte it has not verified:
//!
//! 1. the transferred stored bytes must match the response's declared stored size and sha256;
//! 2. the bytes are decoded per the response's declared compression;
//! 3. the recovered original must match the response's declared original size and sha256 *and*
//!    the digest the caller already expected — the requested claim-ticket hash for retrieval,
//!    the listing's `original_sha256` for the archive walk.
//!
//! # The size gate
//!
//! Both callers know an artifact's stored *and* original sizes from the listing before any
//! transfer, so [`size_refusal`] gates on both against the caller's cap, inside
//! [`ReadClient::download_verified`] and therefore before the request is ever sent. Gating the
//! original size is what keeps a highly compressible artifact from decompressing into unbounded
//! memory: a stored blob comfortably under the cap can expand by orders of magnitude, and a
//! stored-size check alone would wave it through and then materialize the result with
//! `zstd::decode_all`.

use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::http::{self, Header, HttpError, HttpResponse};

/// The API base path every Patwari route is nested under.
pub(crate) const API_BASE: &str = "/api/v1";
/// Network timeout for a single request.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Page size requested from a listing (Patwari's maximum).
pub(crate) const LISTING_PAGE_SIZE: usize = 100;
/// Default upper bound on one downloaded artifact, in both its stored and its original form. The
/// server stores artifacts up to 1 GiB, so this sits below the storage ceiling: an artifact past
/// it is refused up front rather than truncated into a misleading verification failure. Callers
/// raise it deliberately with `--max-download-bytes`.
pub(crate) const MAX_ARTIFACT_DOWNLOAD_BYTES: usize = 128 * 1024 * 1024;
/// Read-bound headroom over the declared stored size, covering the status line and response
/// headers so an artifact whose stored size sits exactly at the cap still transfers completely.
const RESPONSE_HEADER_ALLOWANCE_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------

/// A classified failure from a listing request. Callers map each variant onto their own error
/// type: the `code` is left as the server sent it (or absent) because callers disagree on the
/// placeholder to substitute.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// The request never completed, or the endpoint is unusable.
    Http(HttpError),
    /// The server answered with a non-success status, carrying Patwari's stable `error.code`
    /// when the body had one.
    Status { status: u16, code: Option<String> },
    /// The server answered successfully but unintelligibly (malformed body, missing field).
    Protocol(String),
}

/// A classified failure from a verified download. Distinguishes the classes the two callers grade
/// differently: an integrity failure is a verification finding, a malformed response is a protocol
/// problem, and a refusal by the size gate is neither.
#[derive(Debug)]
pub(crate) enum DownloadError {
    /// The request never completed.
    Http(HttpError),
    /// The content route answered with a non-success status.
    Status { status: u16, code: Option<String> },
    /// The response was structurally unusable: a missing or unparseable metadata header, or a
    /// compression this build does not implement.
    Protocol(String),
    /// Transferred or recovered bytes failed a size or digest check. No bytes are returned.
    Verification(String),
    /// The stored bytes could not be decompressed. Carries the decoder's own message.
    Decompression(String),
    /// The size gate refused the artifact before any transfer.
    TooLarge(SizeRefusal),
}

/// Which declared size tripped the download cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeDimension {
    /// The bytes as the server stores them — what would cross the wire.
    Stored,
    /// The bytes as they decompress — what would be materialized in memory.
    Original,
}

/// The size gate's refusal, carrying enough for a caller to phrase its own message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SizeRefusal {
    pub dimension: SizeDimension,
    /// The declared size, in bytes, that exceeded the cap.
    pub size_bytes: u64,
    pub cap: u64,
}

/// Refuses an artifact whose listing declares either a stored or an original size past `cap`.
///
/// Checking the *original* size is the decompression-amplification gate: the listing carries it,
/// so an artifact that would balloon on decode is refused before a single byte is transferred,
/// rather than after `zstd::decode_all` has already materialized it. The stored dimension is
/// reported first so an artifact that is oversized both ways names the transfer that would have
/// failed first.
pub(crate) fn size_refusal(
    stored_size_bytes: u64,
    original_size_bytes: u64,
    cap: usize,
) -> Option<SizeRefusal> {
    let cap = cap as u64;
    if stored_size_bytes > cap {
        return Some(SizeRefusal {
            dimension: SizeDimension::Stored,
            size_bytes: stored_size_bytes,
            cap,
        });
    }
    if original_size_bytes > cap {
        return Some(SizeRefusal {
            dimension: SizeDimension::Original,
            size_bytes: original_size_bytes,
            cap,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Listing traversal
// ---------------------------------------------------------------------------

/// One traversal of a cursor-paginated listing.
#[derive(Debug)]
pub(crate) struct Listing<T> {
    pub items: Vec<T>,
    /// Whether the peer stopped returning cursors within the page bound. Callers disagree on what
    /// an unterminated traversal means — a partial match set is still useful to retrieval, while
    /// an acceptance walk that silently skipped snapshots is worse than no walk — so the bound is
    /// reported rather than resolved here.
    pub terminated: bool,
}

/// An artifact as the listing describes it: everything [`ReadClient::download_verified`] needs to
/// bound, fetch, and cross-check the download. Callers keep their own richer listing types and
/// build this for the download call.
pub(crate) struct ListedArtifact<'a> {
    pub content_url: &'a str,
    /// The listing's declared stored size: bounds the read and feeds the size gate.
    pub stored_size_bytes: u64,
    /// The listing's declared original size: feeds the amplification half of the size gate.
    pub original_size_bytes: u64,
    /// The bare (unprefixed) digest the recovered original bytes must equal.
    pub expected_original_sha256: &'a str,
    /// How the caller names `expected_original_sha256` in a mismatch message, e.g. `requested` or
    /// `listing's declared`.
    pub expected_label: &'a str,
}

// ---------------------------------------------------------------------------
// The read client
// ---------------------------------------------------------------------------

/// A synchronous Patwari read client bound to one archive server, speaking the shared blocking
/// [`crate::http`] client exactly as archive upload does.
pub(crate) struct ReadClient {
    host: String,
    port: u16,
    timeout: Duration,
}

impl ReadClient {
    /// Binds to `endpoint`. Callers map the [`HttpError`] onto their own unreachable-endpoint
    /// error, so the wording of an unusable endpoint stays each command's own.
    pub(crate) fn connect(endpoint: &str) -> Result<Self, HttpError> {
        let (host, port) = http::parse_http_endpoint(endpoint)?;
        Ok(Self {
            host,
            port,
            timeout: REQUEST_TIMEOUT,
        })
    }

    /// Walks a cursor-paginated listing, following only the server's own `next_cursor` (Patwari's
    /// traversal contract) for at most `max_pages` pages.
    ///
    /// `page_path` builds the request target for a page given the cursor to resume from, and
    /// `parse_item` reads one `items` entry, reporting a protocol message on a missing field.
    /// `label` names the listing in the `missing items` protocol message.
    pub(crate) fn paginate<T>(
        &self,
        label: &str,
        max_pages: usize,
        page_path: impl Fn(Option<&str>) -> String,
        parse_item: impl Fn(&Value) -> Result<T, String>,
    ) -> Result<Listing<T>, ReadError> {
        paginate_pages(
            label,
            max_pages,
            |cursor| self.get_json(&page_path(cursor)),
            parse_item,
        )
    }

    /// Fetches a single JSON document (a manifest, say), returning the parsed body.
    pub(crate) fn get_json(&self, path: &str) -> Result<Value, ReadError> {
        let response = self.get(path, None).map_err(ReadError::Http)?;
        if response.status != 200 {
            return Err(ReadError::Status {
                status: response.status,
                code: error_code(&response.body),
            });
        }
        parse_json(&response.body).map_err(ReadError::Protocol)
    }

    /// Downloads an artifact's stored bytes and returns its verified original bytes, or refuses.
    ///
    /// The size gate runs first, so an artifact past `cap` in either dimension costs no transfer.
    /// The read is then bounded at the declared stored size plus a header allowance: a body longer
    /// than declared is truncated into a stored-size verification failure instead of being read
    /// into memory unbounded. Every stage is described in the module docs; nothing returns to the
    /// caller until all three pass.
    pub(crate) fn download_verified(
        &self,
        artifact: &ListedArtifact<'_>,
        cap: usize,
    ) -> Result<Vec<u8>, DownloadError> {
        if let Some(refusal) = size_refusal(
            artifact.stored_size_bytes,
            artifact.original_size_bytes,
            cap,
        ) {
            return Err(DownloadError::TooLarge(refusal));
        }
        let limit = usize::try_from(artifact.stored_size_bytes)
            .unwrap_or(usize::MAX)
            .saturating_add(RESPONSE_HEADER_ALLOWANCE_BYTES);
        let response = self
            .get(artifact.content_url, Some(limit))
            .map_err(DownloadError::Http)?;
        if response.status != 200 {
            return Err(DownloadError::Status {
                status: response.status,
                code: error_code(&response.body),
            });
        }

        let compression = response
            .header("x-patwari-compression")
            .ok_or_else(|| {
                DownloadError::Protocol("content missing compression header".to_owned())
            })?
            .to_owned();
        let stored_sha_header = header_digest(&response, "x-patwari-stored-sha256")?;
        let original_sha_header = header_digest(&response, "x-patwari-original-sha256")?;
        let stored_size_header = header_u64(&response, "x-patwari-stored-size-bytes")?;
        let original_size_header = header_u64(&response, "x-patwari-original-size-bytes")?;

        // 1. The transferred stored bytes must match the server's declared stored digest and size.
        let stored_bytes = response.body;
        if stored_bytes.len() as u64 != stored_size_header {
            return Err(DownloadError::Verification(format!(
                "stored size mismatch: got {} bytes, expected {stored_size_header}",
                stored_bytes.len()
            )));
        }
        if sha256_hex(&stored_bytes) != stored_sha_header {
            return Err(DownloadError::Verification(
                "stored content hash does not match the archive's declared stored hash".to_owned(),
            ));
        }

        // 2. Decode per the declared compression to recover the original bytes. The gate above
        //    already bounded what this can expand to.
        let original_bytes = match compression.as_str() {
            "identity" => stored_bytes,
            "zstd" => zstd::decode_all(stored_bytes.as_slice())
                .map_err(|error| DownloadError::Decompression(error.to_string()))?,
            other => {
                return Err(DownloadError::Protocol(format!(
                    "unknown compression `{other}`"
                )));
            }
        };

        // 3. The recovered original must match the headers and the caller's expected digest.
        if original_bytes.len() as u64 != original_size_header {
            return Err(DownloadError::Verification(format!(
                "original size mismatch: got {} bytes, expected {original_size_header}",
                original_bytes.len()
            )));
        }
        let original_digest = sha256_hex(&original_bytes);
        if original_digest != original_sha_header {
            return Err(DownloadError::Verification(
                "decompressed content hash does not match the archive's declared original hash"
                    .to_owned(),
            ));
        }
        if original_digest != artifact.expected_original_sha256 {
            return Err(DownloadError::Verification(format!(
                "decompressed content hash sha256:{original_digest} does not match the {} sha256:{}",
                artifact.expected_label, artifact.expected_original_sha256
            )));
        }
        Ok(original_bytes)
    }

    fn get(
        &self,
        path: &str,
        max_response_bytes: Option<usize>,
    ) -> Result<HttpResponse, HttpError> {
        let headers = [Header {
            name: "Accept",
            value: "*/*",
        }];
        let request = http::HttpRequest {
            method: "GET",
            path,
            headers: &headers,
            body: None,
        };
        match max_response_bytes {
            Some(limit) => {
                http::send_with_limit(&self.host, self.port, self.timeout, &request, limit)
            }
            None => http::send(&self.host, self.port, self.timeout, &request),
        }
    }
}

/// The cursor-following page loop, over any source of pages. [`ReadClient::paginate`] is this with
/// the real HTTP fetch supplied; keeping the loop transport-free is what lets its bounding and
/// cursor discipline be unit-tested without a socket, rather than tempting a second copy.
fn paginate_pages<T>(
    label: &str,
    max_pages: usize,
    fetch: impl Fn(Option<&str>) -> Result<Value, ReadError>,
    parse_item: impl Fn(&Value) -> Result<T, String>,
) -> Result<Listing<T>, ReadError> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..max_pages {
        let value = fetch(cursor.as_deref())?;
        let page = value
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| ReadError::Protocol(format!("{label} missing items")))?;
        for item in page {
            items.push(parse_item(item).map_err(ReadError::Protocol)?);
        }
        cursor = value
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if cursor.is_none() {
            return Ok(Listing {
                items,
                terminated: true,
            });
        }
    }
    Ok(Listing {
        items,
        terminated: false,
    })
}

// ---------------------------------------------------------------------------
// Bounded JSON and header access
// ---------------------------------------------------------------------------

/// Parses a response body as JSON, reporting the parser's own message.
pub(crate) fn parse_json(body: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(body).map_err(|error| error.to_string())
}

/// Reads a required string field from a listing item.
pub(crate) fn required_str(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("listing item missing {key}"))
}

/// Reads a required unsigned-integer field from a listing item.
pub(crate) fn required_u64(value: &Value, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("listing item missing {key}"))
}

/// Reads an optional string field.
pub(crate) fn optional_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Follows a dotted path of object keys to a string leaf, without allocating on the way. Callers
/// word their own message for an absent path, because the documents differ.
pub(crate) fn nested_str(value: &Value, path: &[&str]) -> Option<String> {
    nested(value, path)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Follows a dotted path of object keys to an unsigned-integer leaf.
pub(crate) fn nested_u64(value: &Value, path: &[&str]) -> Option<u64> {
    nested(value, path).and_then(Value::as_u64)
}

fn nested<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |current, key| current.get(key))
}

fn header_digest(response: &HttpResponse, name: &str) -> Result<String, DownloadError> {
    response
        .header(name)
        .map(strip_digest)
        .ok_or_else(|| DownloadError::Protocol(format!("content missing {name} header")))
}

fn header_u64(response: &HttpResponse, name: &str) -> Result<u64, DownloadError> {
    response
        .header(name)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| DownloadError::Protocol(format!("content missing or invalid {name} header")))
}

/// Strips Patwari's `sha256:` document prefix, leaving the bare lowercase hex digest.
pub(crate) fn strip_digest(value: &str) -> String {
    value.strip_prefix("sha256:").unwrap_or(value).to_owned()
}

/// Reads Patwari's stable `error.code` from an error response body, if present.
pub(crate) fn error_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("error")?
        .get("code")?
        .as_str()
        .map(ToOwned::to_owned)
}

/// The lowercase hex sha256 of `bytes`, in Patwari's bare (unprefixed) digest form.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // The size gate
    // -----------------------------------------------------------------------

    #[test]
    fn size_gate_admits_an_artifact_within_the_cap_in_both_dimensions() {
        assert_eq!(size_refusal(100, 400, 1_000), None);
        // Exactly at the cap is admitted; the cap is a ceiling, not an exclusive bound.
        assert_eq!(size_refusal(1_000, 1_000, 1_000), None);
    }

    #[test]
    fn size_gate_refuses_an_oversized_stored_artifact() {
        let refusal = size_refusal(2_048, 4_096, 1_024).expect("stored size exceeds the cap");
        assert_eq!(refusal.dimension, SizeDimension::Stored);
        assert_eq!(refusal.size_bytes, 2_048);
        assert_eq!(refusal.cap, 1_024);
    }

    /// The amplification gate: a listing that declares a tiny stored size but a huge original
    /// size is refused on the original dimension, before any transfer or `zstd::decode_all`.
    #[test]
    fn size_gate_refuses_a_highly_compressible_artifact_on_its_declared_original_size() {
        let refusal = size_refusal(4_096, 8 * 1024 * 1024 * 1024, 128 * 1024 * 1024)
            .expect("declared original size exceeds the cap");
        assert_eq!(refusal.dimension, SizeDimension::Original);
        assert_eq!(refusal.size_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(refusal.cap, 128 * 1024 * 1024);
    }

    /// A download can never skip the gate: the refusal happens before the client would build a
    /// request, so an unreachable endpoint is never even contacted.
    #[test]
    fn download_refuses_an_amplifying_artifact_before_any_transfer() {
        // Port 1 on loopback refuses connections: reaching the network would surface as
        // `DownloadError::Http`, so a `TooLarge` proves nothing was transferred.
        let client = ReadClient::connect("http://127.0.0.1:1").unwrap();
        let artifact = ListedArtifact {
            content_url: "/api/v1/artifacts/art-1/content",
            stored_size_bytes: 1_024,
            original_size_bytes: 64 * 1024 * 1024 * 1024,
            expected_original_sha256: &"a".repeat(64),
            expected_label: "requested",
        };
        let error = client
            .download_verified(&artifact, 128 * 1024 * 1024)
            .expect_err("the amplification gate refuses the download");
        let DownloadError::TooLarge(refusal) = error else {
            panic!("expected a size refusal, got {error:?}");
        };
        assert_eq!(refusal.dimension, SizeDimension::Original);
    }

    // -----------------------------------------------------------------------
    // Pagination
    // -----------------------------------------------------------------------

    /// The page bound is a guard against a peer that never stops returning cursors: the walk
    /// stops at `max_pages` and reports that it did not terminate, leaving each caller to decide
    /// whether a partial listing is usable.
    #[test]
    fn pagination_reports_an_unterminated_walk_at_the_page_bound() {
        // A peer that always answers with one item and a fresh cursor.
        let pages = std::cell::RefCell::new(0usize);
        let listing = walk(3, |cursor| {
            *pages.borrow_mut() += 1;
            assert_eq!(cursor.is_none(), *pages.borrow() == 1);
            r#"{"items":[{"artifact_id":"a"}],"next_cursor":"more"}"#.to_owned()
        });
        assert_eq!(*pages.borrow(), 3, "the walk stops at the page bound");
        assert_eq!(listing.items.len(), 3);
        assert!(!listing.terminated);
    }

    #[test]
    fn pagination_follows_cursors_until_the_server_stops_returning_them() {
        let seen: std::cell::RefCell<Vec<Option<String>>> = std::cell::RefCell::new(Vec::new());
        let listing = walk(100, |cursor| {
            seen.borrow_mut().push(cursor.map(ToOwned::to_owned));
            match cursor {
                None => r#"{"items":[{"artifact_id":"a"}],"next_cursor":"c1"}"#.to_owned(),
                Some("c1") => r#"{"items":[{"artifact_id":"b"}],"next_cursor":"c2"}"#.to_owned(),
                _ => r#"{"items":[{"artifact_id":"c"}],"next_cursor":null}"#.to_owned(),
            }
        });
        assert!(listing.terminated);
        assert_eq!(listing.items, vec!["a", "b", "c"]);
        assert_eq!(
            *seen.borrow(),
            vec![None, Some("c1".to_owned()), Some("c2".to_owned())],
            "each page resumes from the cursor the previous one returned"
        );
    }

    #[test]
    fn pagination_reports_a_listing_without_items_as_a_protocol_error() {
        let error = walk_result(10, |_| r#"{"next_cursor":null}"#.to_owned())
            .expect_err("a listing with no items array is unintelligible");
        let ReadError::Protocol(message) = error else {
            panic!("expected a protocol error, got {error:?}");
        };
        assert_eq!(message, "test listing missing items");
    }

    #[test]
    fn pagination_reports_an_item_missing_a_field_as_a_protocol_error() {
        let error = walk_result(10, |_| {
            r#"{"items":[{"other":"x"}],"next_cursor":null}"#.to_owned()
        })
        .expect_err("an item missing its key field is unintelligible");
        let ReadError::Protocol(message) = error else {
            panic!("expected a protocol error, got {error:?}");
        };
        assert_eq!(message, "listing item missing artifact_id");
    }

    /// Drives [`ReadClient::paginate`]'s page loop over an in-memory server. The client's own
    /// socket path is exercised by the two commands' integration tests against a fake daemon; what
    /// needs unit coverage here is the cursor-following and bounding logic.
    fn walk(max_pages: usize, page: impl Fn(Option<&str>) -> String) -> Listing<String> {
        walk_result(max_pages, page).expect("the walk succeeds")
    }

    fn walk_result(
        max_pages: usize,
        page: impl Fn(Option<&str>) -> String,
    ) -> Result<Listing<String>, ReadError> {
        paginate_pages(
            "test listing",
            max_pages,
            |cursor| parse_json(page(cursor).as_bytes()).map_err(ReadError::Protocol),
            |item| required_str(item, "artifact_id"),
        )
    }

    // -----------------------------------------------------------------------
    // Header and JSON helpers
    // -----------------------------------------------------------------------

    #[test]
    fn strips_the_digest_prefix() {
        assert_eq!(strip_digest("sha256:abcd"), "abcd");
        assert_eq!(strip_digest("abcd"), "abcd");
    }

    #[test]
    fn reads_nested_document_paths_and_reports_absent_ones() {
        let value = parse_json(br#"{"manifest":{"capture":{"artifact_set_version":1}}}"#).unwrap();
        assert_eq!(
            nested_u64(&value, &["manifest", "capture", "artifact_set_version"]),
            Some(1)
        );
        assert_eq!(nested_str(&value, &["manifest", "session", "agent"]), None);
        assert_eq!(nested_u64(&value, &["manifest", "capture"]), None);
    }

    #[test]
    fn reads_the_stable_error_code_when_present() {
        assert_eq!(
            error_code(br#"{"error":{"code":"artifact_not_found"}}"#).as_deref(),
            Some("artifact_not_found")
        );
        assert_eq!(error_code(b"{}"), None);
        assert_eq!(error_code(b"not json"), None);
    }
}

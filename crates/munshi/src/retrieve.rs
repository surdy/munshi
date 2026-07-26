//! Claim-ticket retrieval: redeem a content hash for its exact original bytes from Patwari
//! (ADR 0010, issue #22).
//!
//! A claim ticket carries an original sha256. Retrieval resolves that hash through Patwari's
//! hash-addressed artifact listing (`GET /api/v1/artifacts?original_sha256=…`), downloads the
//! stored (possibly zstd-compressed) bytes of the chosen artifact, and reproduces the original
//! content locally — verifying both the stored and the original sha256 before a single byte is
//! emitted. Search within retrieved content stays entirely client-side; Patwari never interprets
//! content (its ADR 0004), so `--query` is a local substring scan over the decompressed bytes.
//!
//! Munshi is fully synchronous: this speaks the Patwari HTTP API over the shared blocking
//! [`crate::http`] client, exactly as archive upload (issue #19) does, and reuses the endpoint that
//! archive-upload configuration already recorded.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::http::{self, Header, HttpError};
use crate::patwari::{self, PatwariError};

/// The API base path every Patwari route is nested under.
const API_BASE: &str = "/api/v1";
/// Network timeout for a single retrieval request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Page size requested from the artifact listing. Blob dedup means a hash can match across several
/// snapshots, but the count is small in practice; this keeps a match set to one or two round-trips.
const LISTING_PAGE_SIZE: usize = 100;
/// Guards the listing-pagination loop against a misbehaving peer that never stops returning cursors.
const MAX_LISTING_PAGES: usize = 1_000;
/// Default upper bound on a downloaded stored artifact. The server stores artifacts up to 1 GiB, so
/// this is below the storage ceiling: an artifact whose stored size exceeds it is refused up front
/// (a distinct `TooLarge` error) rather than truncated into a misleading verification failure. A
/// caller can raise it deliberately with `--max-download-bytes`.
const MAX_ARTIFACT_DOWNLOAD_BYTES: usize = 128 * 1024 * 1024;
/// Lines of context printed either side of a `--query` match.
pub const QUERY_CONTEXT_LINES: usize = 2;

#[derive(Debug, Error)]
pub enum RetrieveError {
    /// The supplied hash is not a 64-character lowercase sha256 hex digest (checked before any I/O).
    #[error("invalid content hash: {0}")]
    InvalidHash(String),
    /// No archive-upload endpoint is configured, so there is no server to retrieve from.
    #[error(
        "no archive server is configured; set an archive-upload endpoint before retrieving content"
    )]
    NotConfigured,
    /// The listing returned no artifact for this hash.
    #[error(
        "no archived artifact matches sha256:{0}; its snapshot may not be uploaded yet, or the hash is unknown"
    )]
    NotFound(String),
    /// The archive server could not be reached (connection refused, DNS, timeout).
    #[error("archive server is unreachable: {0}")]
    Unreachable(String),
    /// The server spoke unexpectedly (malformed body, missing headers).
    #[error("archive server protocol error: {0}")]
    Protocol(String),
    /// The server returned a non-success status.
    #[error("archive server returned status {status}: {code}")]
    Server { status: u16, code: String },
    /// Downloaded content failed stored- or original-hash/size verification. No bytes are emitted.
    #[error("content verification failed: {0}")]
    Verification(String),
    /// The artifact's stored size exceeds the download cap. Refused before any transfer; raise the
    /// cap with `--max-download-bytes` to retrieve it deliberately.
    #[error(
        "archived artifact stored size {stored_size_bytes} bytes exceeds the {cap}-byte download cap; pass --max-download-bytes to raise it"
    )]
    TooLarge { stored_size_bytes: u64, cap: u64 },
    /// The stored bytes could not be zstd-decompressed.
    #[error("could not decompress stored content: {0}")]
    Decompression(String),
    /// `--output` names an existing file and `--force` was not given.
    #[error("refusing to overwrite existing file {}; pass --force to replace it", .0.display())]
    OutputExists(PathBuf),
    /// Writing the `--output` file failed.
    #[error("could not write output: {0}")]
    Io(#[source] std::io::Error),
    /// Reading the configured endpoint failed.
    #[error(transparent)]
    Config(PatwariError),
}

impl RetrieveError {
    /// A distinct, stable process exit code per failure class, so callers and scripts can tell the
    /// error kinds apart without parsing messages: 1 local I/O or config, 2 invalid input, 3 no
    /// server configured, 4 no matching artifact, 5 server/transport, 6 verification/decompression,
    /// 7 artifact larger than the download cap.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidHash(_) | Self::OutputExists(_) => 2,
            Self::NotConfigured => 3,
            Self::NotFound(_) => 4,
            Self::Unreachable(_) | Self::Protocol(_) | Self::Server { .. } => 5,
            Self::Verification(_) | Self::Decompression(_) => 6,
            Self::TooLarge { .. } => 7,
            Self::Io(_) | Self::Config(_) => 1,
        }
    }
}

/// One artifact matching a requested original hash. Sizes and digests are the bare values; digests
/// are lowercase hex without the wire `sha256:` prefix.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactMatch {
    pub artifact_id: String,
    pub snapshot_id: String,
    pub logical_path: String,
    pub media_type: Option<String>,
    pub original_sha256: String,
    pub original_size_bytes: u64,
    pub stored_sha256: String,
    pub stored_size_bytes: u64,
    pub compression: String,
    /// When the artifact's snapshot was archived; used to pick the newest match.
    pub created_at: String,
    #[serde(skip)]
    content_url: String,
}

/// Verified original content, ready to emit.
#[derive(Debug, Clone)]
pub struct RetrievedContent {
    pub artifact: ArtifactMatch,
    pub original_bytes: Vec<u8>,
}

/// The outcome of a `retrieve` invocation.
#[derive(Debug, Clone)]
pub enum RetrieveResult {
    /// `--list`: every match for the hash, newest first, with nothing downloaded.
    Listing(Vec<ArtifactMatch>),
    /// The default path: the newest match, downloaded and verified.
    Retrieved(Box<RetrievedContent>),
}

/// Resolves `input_hash` against the configured archive server and, unless `list` is set, downloads
/// and verifies the newest matching artifact's original bytes.
///
/// Newest-match selection uses each match's artifact `created_at` — the moment its snapshot was
/// archived — descending, tie-broken by `artifact_id` descending to match the server's own stable
/// ordering. Because blob dedup lets one original hash appear under several snapshots, the newest is
/// the most recently archived copy of identical bytes; every copy yields byte-identical content.
/// `endpoint_override` bypasses configuration to retrieve from an explicit server; when `None`, the
/// endpoint recorded by archive-upload configuration is used. The hash is validated locally before
/// either is consulted, so a malformed hash never triggers configuration reads or network access.
/// `max_download_bytes` overrides the default download cap; an artifact whose declared stored size
/// exceeds the effective cap is refused (`TooLarge`) before any transfer, so a large artifact fails
/// with an actionable error instead of a misleading truncated-body verification failure.
pub fn retrieve(
    state_directory: &Path,
    endpoint_override: Option<&str>,
    input_hash: &str,
    list: bool,
    max_download_bytes: Option<usize>,
) -> Result<RetrieveResult, RetrieveError> {
    let hash = normalize_hash(input_hash)?;
    let cap = max_download_bytes.unwrap_or(MAX_ARTIFACT_DOWNLOAD_BYTES);
    let endpoint = match endpoint_override {
        Some(endpoint) => endpoint.to_owned(),
        None => configured_endpoint(state_directory)?,
    };
    let client = RetrieveClient::connect(&endpoint)?;

    let mut matches = client.list_matches(&hash)?;
    if matches.is_empty() {
        return Err(RetrieveError::NotFound(hash));
    }
    sort_newest_first(&mut matches);

    if list {
        return Ok(RetrieveResult::Listing(matches));
    }

    let chosen = matches.into_iter().next().expect("match set is non-empty");
    // The listing already carries the stored size, so an oversized artifact is refused before any
    // bytes are transferred — never truncated into a misleading stored-size-mismatch verification.
    if chosen.stored_size_bytes > cap as u64 {
        return Err(RetrieveError::TooLarge {
            stored_size_bytes: chosen.stored_size_bytes,
            cap: cap as u64,
        });
    }
    let original_bytes = client.download_verified(&chosen, &hash, cap)?;
    Ok(RetrieveResult::Retrieved(Box::new(RetrievedContent {
        artifact: chosen,
        original_bytes,
    })))
}

/// Writes verified original bytes to `path`, refusing to clobber an existing file unless `force`.
pub fn write_output(path: &Path, bytes: &[u8], force: bool) -> Result<(), RetrieveError> {
    if !force && path.exists() {
        return Err(RetrieveError::OutputExists(path.to_path_buf()));
    }
    std::fs::write(path, bytes).map_err(RetrieveError::Io)
}

/// Normalizes a claim-ticket hash: strips an optional `sha256:` prefix and requires exactly 64
/// lowercase hexadecimal characters. Rejection happens before any network access.
fn normalize_hash(input: &str) -> Result<String, RetrieveError> {
    let candidate = input.strip_prefix("sha256:").unwrap_or(input);
    let valid = candidate.len() == 64
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid {
        Ok(candidate.to_owned())
    } else {
        Err(RetrieveError::InvalidHash(input.to_owned()))
    }
}

/// Reads the archive-upload endpoint recorded in configuration. Retrieval reuses upload's endpoint
/// and does not require upload to be enabled — only that a server address is configured.
fn configured_endpoint(state_directory: &Path) -> Result<String, RetrieveError> {
    let report = patwari::status(state_directory).map_err(RetrieveError::Config)?;
    report
        .settings
        .endpoint
        .filter(|endpoint| !endpoint.is_empty())
        .ok_or(RetrieveError::NotConfigured)
}

/// Orders matches newest-first: by snapshot archival time, then artifact id, both descending.
fn sort_newest_first(matches: &mut [ArtifactMatch]) {
    matches.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.artifact_id.cmp(&a.artifact_id))
    });
}

/// A synchronous retrieval client bound to one archive server.
struct RetrieveClient {
    host: String,
    port: u16,
    timeout: Duration,
}

impl RetrieveClient {
    fn connect(endpoint: &str) -> Result<Self, RetrieveError> {
        let (host, port) = http::parse_http_endpoint(endpoint).map_err(from_http)?;
        Ok(Self {
            host,
            port,
            timeout: REQUEST_TIMEOUT,
        })
    }

    /// Pages through `GET /api/v1/artifacts?original_sha256=…`, collecting every match.
    fn list_matches(&self, hash: &str) -> Result<Vec<ArtifactMatch>, RetrieveError> {
        let mut matches = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LISTING_PAGES {
            let mut path =
                format!("{API_BASE}/artifacts?original_sha256={hash}&limit={LISTING_PAGE_SIZE}");
            if let Some(cursor) = &cursor {
                path.push_str("&cursor=");
                path.push_str(&http::encode_path(cursor));
            }
            let response = self.get(&path)?;
            match response.status {
                200 => {}
                422 => return Err(RetrieveError::InvalidHash(hash.to_owned())),
                status => return Err(self.server_error(status, &response.body)),
            }
            let value = parse_json(&response.body)?;
            let items = value
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| RetrieveError::Protocol("listing missing items".to_owned()))?;
            for item in items {
                matches.push(artifact_match_from_value(item)?);
            }
            cursor = value
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(matches)
    }

    /// Downloads the artifact's stored bytes, verifies the stored digest, decompresses, and verifies
    /// the original digest and size against both the response headers and the requested hash. Any
    /// mismatch returns an error before the caller can emit content.
    fn download_verified(
        &self,
        artifact: &ArtifactMatch,
        requested_hash: &str,
        max_download_bytes: usize,
    ) -> Result<Vec<u8>, RetrieveError> {
        let response = self.get_with_limit(&artifact.content_url, max_download_bytes)?;
        match response.status {
            200 => {}
            404 => return Err(RetrieveError::NotFound(requested_hash.to_owned())),
            status => return Err(self.server_error(status, &response.body)),
        }

        let compression = response
            .header("x-patwari-compression")
            .ok_or_else(|| {
                RetrieveError::Protocol("content missing compression header".to_owned())
            })?
            .to_owned();
        let stored_sha_header = header_digest(&response, "x-patwari-stored-sha256")?;
        let original_sha_header = header_digest(&response, "x-patwari-original-sha256")?;
        let stored_size_header = header_u64(&response, "x-patwari-stored-size-bytes")?;
        let original_size_header = header_u64(&response, "x-patwari-original-size-bytes")?;

        // 1. The transferred stored bytes must match the server's declared stored digest and size.
        let stored_bytes = response.body;
        if stored_bytes.len() as u64 != stored_size_header {
            return Err(RetrieveError::Verification(format!(
                "stored size mismatch: got {} bytes, expected {stored_size_header}",
                stored_bytes.len()
            )));
        }
        let stored_digest = sha256_hex(&stored_bytes);
        if stored_digest != stored_sha_header {
            return Err(RetrieveError::Verification(
                "stored content hash does not match the archive's declared stored hash".to_owned(),
            ));
        }

        // 2. Decompress per the declared compression to recover the original bytes.
        let original_bytes = match compression.as_str() {
            "identity" => stored_bytes,
            "zstd" => zstd::decode_all(stored_bytes.as_slice())
                .map_err(|error| RetrieveError::Decompression(error.to_string()))?,
            other => {
                return Err(RetrieveError::Protocol(format!(
                    "unknown compression `{other}`"
                )));
            }
        };

        // 3. The recovered original must match the requested hash, the header hash, and the size.
        if original_bytes.len() as u64 != original_size_header {
            return Err(RetrieveError::Verification(format!(
                "original size mismatch: got {} bytes, expected {original_size_header}",
                original_bytes.len()
            )));
        }
        let original_digest = sha256_hex(&original_bytes);
        if original_digest != original_sha_header {
            return Err(RetrieveError::Verification(
                "decompressed content hash does not match the archive's declared original hash"
                    .to_owned(),
            ));
        }
        if original_digest != requested_hash {
            return Err(RetrieveError::Verification(format!(
                "decompressed content hash sha256:{original_digest} does not match the requested sha256:{requested_hash}"
            )));
        }
        Ok(original_bytes)
    }

    fn get(&self, path: &str) -> Result<http::HttpResponse, RetrieveError> {
        self.send(path, None)
    }

    fn get_with_limit(
        &self,
        path: &str,
        max_response_bytes: usize,
    ) -> Result<http::HttpResponse, RetrieveError> {
        self.send(path, Some(max_response_bytes))
    }

    fn send(
        &self,
        path: &str,
        max_response_bytes: Option<usize>,
    ) -> Result<http::HttpResponse, RetrieveError> {
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
        let result = match max_response_bytes {
            Some(limit) => {
                http::send_with_limit(&self.host, self.port, self.timeout, &request, limit)
            }
            None => http::send(&self.host, self.port, self.timeout, &request),
        };
        result.map_err(from_http)
    }

    fn server_error(&self, status: u16, body: &[u8]) -> RetrieveError {
        RetrieveError::Server {
            status,
            code: error_code(body).unwrap_or_else(|| "unknown".to_owned()),
        }
    }
}

/// A grouped `--query` search result over decompressed content.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResults {
    pub query: String,
    pub total_lines: usize,
    /// Number of individual lines that matched (not counting context lines).
    pub match_count: usize,
    /// Contiguous blocks of lines (a match plus its surrounding context), in file order.
    pub groups: Vec<Vec<MatchLine>>,
}

/// One line within a search result group.
#[derive(Debug, Clone, Serialize)]
pub struct MatchLine {
    /// 1-based line number in the original content.
    pub line_number: usize,
    /// Whether this specific line matched the query (versus being printed as context).
    pub matched: bool,
    pub text: String,
}

/// Case-insensitive substring search over the lines of `content`, returning each matching line with
/// `context` lines either side. Adjacent or overlapping matches merge into a single group. Content
/// is interpreted as UTF-8 lossily so binary artifacts still search without failing.
#[must_use]
pub fn search_content(content: &[u8], query: &str, context: usize) -> SearchResults {
    let text = String::from_utf8_lossy(content);
    let lines: Vec<&str> = text.lines().collect();
    let needle = query.to_lowercase();

    let matched_indexes: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect();

    let mut groups: Vec<Vec<MatchLine>> = Vec::new();
    let mut current: Option<(usize, usize)> = None;
    for &index in &matched_indexes {
        let start = index.saturating_sub(context);
        let end = (index + context).min(lines.len().saturating_sub(1));
        match current {
            // Merge when the new window touches or overlaps the running one.
            Some((cur_start, cur_end)) if start <= cur_end + 1 => {
                current = Some((cur_start, cur_end.max(end)));
            }
            Some((cur_start, cur_end)) => {
                groups.push(build_group(&lines, cur_start, cur_end, &matched_indexes));
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((start, end)) = current {
        groups.push(build_group(&lines, start, end, &matched_indexes));
    }

    SearchResults {
        query: query.to_owned(),
        total_lines: lines.len(),
        match_count: matched_indexes.len(),
        groups,
    }
}

fn build_group(
    lines: &[&str],
    start: usize,
    end: usize,
    matched_indexes: &[usize],
) -> Vec<MatchLine> {
    (start..=end)
        .map(|index| MatchLine {
            line_number: index + 1,
            matched: matched_indexes.contains(&index),
            text: lines[index].to_owned(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn artifact_match_from_value(value: &Value) -> Result<ArtifactMatch, RetrieveError> {
    Ok(ArtifactMatch {
        artifact_id: required_str(value, "artifact_id")?,
        snapshot_id: required_str(value, "snapshot_id")?,
        logical_path: required_str(value, "logical_path")?,
        media_type: value
            .get("media_type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        original_sha256: strip_digest(&required_str(value, "original_sha256")?),
        original_size_bytes: required_u64(value, "original_size_bytes")?,
        stored_sha256: strip_digest(&required_str(value, "stored_sha256")?),
        stored_size_bytes: required_u64(value, "stored_size_bytes")?,
        compression: required_str(value, "compression")?,
        created_at: required_str(value, "created_at")?,
        content_url: required_str(value, "content_url")?,
    })
}

fn parse_json(body: &[u8]) -> Result<Value, RetrieveError> {
    serde_json::from_slice(body).map_err(|error| RetrieveError::Protocol(error.to_string()))
}

fn required_str(value: &Value, key: &str) -> Result<String, RetrieveError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| RetrieveError::Protocol(format!("listing item missing {key}")))
}

fn required_u64(value: &Value, key: &str) -> Result<u64, RetrieveError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| RetrieveError::Protocol(format!("listing item missing {key}")))
}

fn header_digest(response: &http::HttpResponse, name: &str) -> Result<String, RetrieveError> {
    response
        .header(name)
        .map(strip_digest)
        .ok_or_else(|| RetrieveError::Protocol(format!("content missing {name} header")))
}

fn header_u64(response: &http::HttpResponse, name: &str) -> Result<u64, RetrieveError> {
    response
        .header(name)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| RetrieveError::Protocol(format!("content missing or invalid {name} header")))
}

/// Strips Patwari's `sha256:` document prefix, leaving the bare lowercase hex digest.
fn strip_digest(value: &str) -> String {
    value.strip_prefix("sha256:").unwrap_or(value).to_owned()
}

/// Reads Patwari's stable `error.code` from an error response body, if present.
fn error_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("error")?
        .get("code")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn from_http(error: HttpError) -> RetrieveError {
    match error {
        HttpError::UnsupportedEndpoint(endpoint) => {
            RetrieveError::Unreachable(format!("{endpoint} is not a supported http URL"))
        }
        HttpError::Transport(message) => RetrieveError::Unreachable(message),
        HttpError::Protocol(message) => RetrieveError::Protocol(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_validates_hashes() {
        let hex = "a".repeat(64);
        assert_eq!(normalize_hash(&hex).unwrap(), hex);
        assert_eq!(normalize_hash(&format!("sha256:{hex}")).unwrap(), hex);
        // Uppercase, wrong length, and non-hex are all rejected locally.
        assert!(matches!(
            normalize_hash(&"A".repeat(64)),
            Err(RetrieveError::InvalidHash(_))
        ));
        assert!(matches!(
            normalize_hash("abc"),
            Err(RetrieveError::InvalidHash(_))
        ));
        assert!(matches!(
            normalize_hash(&"g".repeat(64)),
            Err(RetrieveError::InvalidHash(_))
        ));
    }

    #[test]
    fn strips_the_digest_prefix() {
        assert_eq!(strip_digest("sha256:abcd"), "abcd");
        assert_eq!(strip_digest("abcd"), "abcd");
    }

    #[test]
    fn sorts_matches_newest_first() {
        let mut matches = vec![
            sample_match("older", "2026-07-24T00:00:00Z"),
            sample_match("newer", "2026-07-25T00:00:00Z"),
        ];
        sort_newest_first(&mut matches);
        assert_eq!(matches[0].artifact_id, "newer");
        assert_eq!(matches[1].artifact_id, "older");
    }

    #[test]
    fn search_finds_matches_with_context_and_merges_groups() {
        let content = b"alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot NEEDLE\ngolf\n";
        let results = search_content(content, "needle", 2);
        assert_eq!(results.match_count, 1);
        assert_eq!(results.total_lines, 7);
        assert_eq!(results.groups.len(), 1);
        let group = &results.groups[0];
        // Two context lines precede the match, one line follows (end of file).
        assert_eq!(group.first().unwrap().line_number, 4);
        let matched: Vec<usize> = group
            .iter()
            .filter(|line| line.matched)
            .map(|line| line.line_number)
            .collect();
        assert_eq!(matched, vec![6]);
    }

    #[test]
    fn adjacent_matches_merge_into_one_group() {
        let content = b"one match\ntwo\nthree match\nfour\nfive\nsix\nseven match\n";
        let results = search_content(content, "match", 1);
        assert_eq!(results.match_count, 3);
        // Lines 1 and 3 merge (windows touch); line 7 is far enough to stand alone.
        assert_eq!(results.groups.len(), 2);
    }

    fn sample_match(id: &str, created_at: &str) -> ArtifactMatch {
        ArtifactMatch {
            artifact_id: id.to_owned(),
            snapshot_id: "snap".to_owned(),
            logical_path: "transcript.jsonl".to_owned(),
            media_type: None,
            original_sha256: "a".repeat(64),
            original_size_bytes: 1,
            stored_sha256: "b".repeat(64),
            stored_size_bytes: 1,
            compression: "identity".to_owned(),
            created_at: created_at.to_owned(),
            content_url: format!("/api/v1/artifacts/{id}/content"),
        }
    }
}

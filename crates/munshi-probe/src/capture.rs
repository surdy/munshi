use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use tempfile::Builder;
use thiserror::Error;

#[derive(Debug, Clone)]
pub enum CaptureMode {
    Raw,
    Sanitized {
        replacement: String,
        preserved_values: BTreeSet<String>,
    },
}

#[derive(Debug, Serialize)]
pub struct CaptureReport {
    pub path: PathBuf,
    pub bytes_written: u64,
    pub sanitized: bool,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("hook input is not valid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("fixture already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("fixture path has no usable parent directory: {0}")]
    InvalidPath(PathBuf),
    #[error("fixture I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub fn capture_hook(
    mut input: impl Read,
    output: &Path,
    mode: CaptureMode,
) -> Result<CaptureReport, CaptureError> {
    let mut input_bytes = Vec::new();
    input.read_to_end(&mut input_bytes)?;

    let mut value: Value = serde_json::from_slice(&input_bytes)?;
    let sanitized = matches!(mode, CaptureMode::Sanitized { .. });
    let output_bytes = match mode {
        CaptureMode::Raw => input_bytes,
        CaptureMode::Sanitized {
            replacement,
            preserved_values,
        } => {
            sanitize_value(&mut value, &replacement, &preserved_values);
            let mut bytes = serde_json::to_vec_pretty(&value)?;
            bytes.push(b'\n');
            bytes
        }
    };

    atomic_create(output, &output_bytes)?;

    Ok(CaptureReport {
        path: output.to_path_buf(),
        bytes_written: output_bytes.len() as u64,
        sanitized,
    })
}

pub fn sanitize_value(value: &mut Value, replacement: &str, preserved_values: &BTreeSet<String>) {
    match value {
        Value::String(text) => {
            if !preserved_values.contains(text) {
                text.clear();
                text.push_str(replacement);
            }
        }
        Value::Array(values) => {
            for value in values {
                sanitize_value(value, replacement, preserved_values);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                sanitize_value(value, replacement, preserved_values);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn atomic_create(output: &Path, bytes: &[u8]) -> Result<(), CaptureError> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() || output.file_name().is_none() {
        return Err(CaptureError::InvalidPath(output.to_path_buf()));
    }

    let mut temporary = Builder::new()
        .prefix(".munshi-probe-")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;

    match temporary.persist_noclobber(output) {
        Ok(file) => {
            file.sync_all()?;
            File::open(parent)?.sync_all()?;
            Ok(())
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            Err(CaptureError::AlreadyExists(output.to_path_buf()))
        }
        Err(error) => Err(CaptureError::Io(error.error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursively_sanitizes_strings_and_preserves_explicit_values() {
        let mut value = serde_json::json!({
            "event": "agentStop",
            "session": "secret-session",
            "nested": ["private", 42, true, null, {"kind": "tool"}]
        });
        let preserved = BTreeSet::from(["agentStop".to_owned(), "tool".to_owned()]);

        sanitize_value(&mut value, "<redacted>", &preserved);

        assert_eq!(
            value,
            serde_json::json!({
                "event": "agentStop",
                "session": "<redacted>",
                "nested": ["<redacted>", 42, true, null, {"kind": "tool"}]
            })
        );
    }
}

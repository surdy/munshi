use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct TranscriptInspection {
    pub bytes: u64,
    pub lines: u64,
    pub json_valid_lines: u64,
    pub top_level_key_frequency: BTreeMap<String, u64>,
    pub discriminator_value_counts: BTreeMap<String, BTreeMap<String, u64>>,
}

#[derive(Debug, Error)]
pub enum InspectionError {
    #[error("transcript I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub fn inspect_transcript(
    path: &Path,
    discriminator_keys: &BTreeSet<String>,
) -> Result<TranscriptInspection, InspectionError> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut report = TranscriptInspection {
        bytes: 0,
        lines: 0,
        json_valid_lines: 0,
        top_level_key_frequency: BTreeMap::new(),
        discriminator_value_counts: discriminator_keys
            .iter()
            .map(|key| (key.clone(), BTreeMap::new()))
            .collect(),
    };
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line)?;
        if bytes_read == 0 {
            break;
        }
        report.bytes += bytes_read as u64;
        report.lines += 1;

        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        report.json_valid_lines += 1;

        let Value::Object(object) = value else {
            continue;
        };
        for key in object.keys() {
            *report
                .top_level_key_frequency
                .entry(key.clone())
                .or_default() += 1;
        }
        for key in discriminator_keys {
            let Some(value) = object.get(key).filter(|value| is_scalar(value)) else {
                continue;
            };
            let label =
                serde_json::to_string(value).expect("serializing a JSON scalar cannot fail");
            *report
                .discriminator_value_counts
                .get_mut(key)
                .expect("requested discriminator key is initialized")
                .entry(label)
                .or_default() += 1;
        }
    }

    Ok(report)
}

fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

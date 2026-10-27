//! Durable local quality context, separate from disposable audio staging.
use super::{LocalAsrSummary, validation::ParsedOutput};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeVersions {
    pub engine: String,
    pub engine_version: String,
    pub ctranslate2_version: String,
    pub python_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRange {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalChunkReceipt {
    pub schema_version: u32,
    pub chunk_index: usize,
    pub audio_start_seconds: f64,
    pub audio_duration_seconds: f64,
    pub core_start_seconds: f64,
    pub core_end_seconds: f64,
    pub model_id: String,
    pub runtime: Option<RuntimeVersions>,
    pub summary: LocalAsrSummary,
    pub review_ranges: Vec<ReviewRange>,
    pub no_owned_audio: bool,
}

#[derive(Serialize, Deserialize)]
struct StoredReceipt {
    receipt: LocalChunkReceipt,
    quality_report: serde_json::Value,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTranscriptionReport {
    pub chunks: Vec<LocalChunkReceipt>,
    pub missing_chunks: Vec<usize>,
}

fn metadata_identity(path: &Path) -> Result<String, String> {
    let metadata = path
        .metadata()
        .map_err(|_| "Cannot read local file identity.")?;
    let modified = metadata
        .modified()
        .map_err(|_| "Cannot read local file modification time.")?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "Invalid local file modification time.")?;
    Ok(format!("{}:{}", metadata.len(), modified.as_nanos()))
}

/// A fingerprint identifier, not a full model.bin content digest. Includes the
/// config/tokenizer content and binary size/mtime so common model changes differ.
pub(super) fn model_identifier(model: &Path) -> Result<String, String> {
    let mut digest = Sha256::new();
    digest.update(b"cortelol-model-fingerprint-v1");
    for name in ["config.json", "tokenizer.json"] {
        let mut content = Vec::new();
        std::fs::File::open(model.join(name))
            .map_err(|_| "Cannot read model metadata.")?
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut content)
            .map_err(|_| "Cannot read model metadata.")?;
        if content.len() > 32 * 1024 * 1024 {
            return Err("Model metadata exceeds 32 MiB.".into());
        }
        digest.update(name.as_bytes());
        digest.update((content.len() as u64).to_le_bytes());
        digest.update(content);
    }
    digest.update(metadata_identity(&model.join("model.bin"))?);
    Ok(format!("local-model-v1:{:x}", digest.finalize()))
}

fn source_key(source: &Path) -> Result<String, String> {
    let canonical = source
        .canonicalize()
        .map_err(|_| "Cannot identify the recording.")?;
    let mut digest = Sha256::new();
    digest.update(canonical.to_string_lossy().as_bytes());
    digest.update(metadata_identity(&canonical)?);
    Ok(format!("{:x}", digest.finalize()))
}

pub(super) fn receipt_directory(app: &AppHandle, source: &Path) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|_| "Application data directory is unavailable.")?
        .join("local-asr-receipts")
        .join(source_key(source)?))
}

pub(super) fn write_receipt(
    directory: &Path,
    chunk: crate::transcription::AudioChunk,
    model_id: &str,
    parsed: Option<&ParsedOutput>,
    quality: Option<&[u8]>,
) -> Result<(), String> {
    let index = chunk.index;
    let quality_report: serde_json::Value = match quality {
        Some(bytes) => serde_json::from_slice(bytes).map_err(|_| "Invalid quality receipt.")?,
        None => serde_json::Value::Null,
    };
    let runtime = if parsed.is_some() {
        let value = quality_report
            .get("runtime")
            .ok_or("Missing runtime version in local quality report.")?;
        let get = |name| -> Result<String, String> {
            let value = value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .ok_or("Invalid local runtime version.")?;
            if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
                return Err("Invalid local runtime version.".into());
            }
            Ok(value.to_string())
        };
        let versions = RuntimeVersions {
            engine: get("engine")?,
            engine_version: get("engine_version")?,
            ctranslate2_version: get("ctranslate2_version")?,
            python_version: get("python_version")?,
        };
        if versions.engine != "faster-whisper" {
            return Err("Unexpected local model engine.".into());
        }
        Some(versions)
    } else {
        None
    };
    let mut summary = LocalAsrSummary {
        processed_chunks: 1,
        ..LocalAsrSummary::default()
    };
    if let Some(parsed) = parsed {
        summary.segment_count = parsed.segments.len();
        summary.review_segment_count = parsed.review_count;
        summary.gap_count = parsed.gap_count;
        summary.detected_languages = parsed.languages.clone();
    }
    let stored = StoredReceipt {
        receipt: LocalChunkReceipt {
            schema_version: 1,
            chunk_index: index,
            audio_start_seconds: chunk.start_ms as f64 / 1000.0,
            audio_duration_seconds: quality_report
                .get("duration_seconds")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            core_start_seconds: chunk.core_start_ms as f64 / 1000.0,
            core_end_seconds: chunk.core_end_ms as f64 / 1000.0,
            model_id: model_id.into(),
            runtime,
            summary,
            review_ranges: parsed.map_or_else(Vec::new, |p| p.review_ranges.clone()),
            no_owned_audio: parsed.is_none(),
        },
        quality_report,
    };
    std::fs::create_dir_all(directory)
        .map_err(|_| "Cannot prepare the local quality receipt directory.")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .map_err(|_| "Cannot stage a local quality receipt.")?;
    serde_json::to_writer(file.as_file_mut(), &stored)
        .map_err(|_| "Cannot write a local quality receipt.")?;
    file.flush()
        .map_err(|_| "Cannot flush the local quality receipt.")?;
    file.as_file()
        .sync_all()
        .map_err(|_| "Cannot save the local quality receipt.")?;
    file.persist(directory.join(format!("chunk-{index}.json")))
        .map_err(|_| "Cannot commit the local quality receipt.")?;
    Ok(())
}

pub(super) fn load_receipt(directory: &Path, index: usize) -> Result<LocalChunkReceipt, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(directory.join(format!("chunk-{index}.json")))
        .map_err(|_| "Local quality receipt is missing.")?
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read local quality receipt.")?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err("Local quality receipt exceeds 32 MiB.".into());
    }
    let stored: StoredReceipt =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid local quality receipt.")?;
    if stored.receipt.schema_version != 1 || stored.receipt.chunk_index != index {
        return Err("Invalid local quality receipt identity.".into());
    }
    Ok(stored.receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcription::TranscriptSegment;

    #[test]
    fn receipt_survives_restart_and_preserves_model_versions_and_flags_without_paths() {
        let directory = tempfile::tempdir().unwrap();
        let parsed = ParsedOutput {
            segments: vec![TranscriptSegment {
                start_seconds: 901.0,
                end_seconds: 902.0,
                text: "spoken".into(),
                speaker: None,
            }],
            review_count: 1,
            gap_count: 1,
            languages: vec!["pt".into()],
            review_ranges: vec![ReviewRange {
                start_seconds: 901.0,
                end_seconds: 902.0,
                reasons: vec!["chunk_boundary".into()],
            }],
        };
        let quality = br#"{"runtime":{"engine":"faster-whisper","engine_version":"test-1","ctranslate2_version":"test-2","python_version":"3.14"},"review_segments":[1]}"#;
        let chunk = crate::transcription::AudioChunk {
            index: 1,
            start_ms: 898000,
            end_ms: 1800000,
            core_start_ms: 900000,
            core_end_ms: 1800000,
        };
        write_receipt(
            directory.path(),
            chunk,
            "local-model-v1:test",
            Some(&parsed),
            Some(quality),
        )
        .unwrap();
        // A fresh read does not depend on LocalAsrRuntime or its cancellation token.
        let restored = load_receipt(directory.path(), 1).unwrap();
        assert_eq!(restored.model_id, "local-model-v1:test");
        assert_eq!(restored.runtime.unwrap().engine_version, "test-1");
        assert_eq!(restored.review_ranges[0].start_seconds, 901.0);
        assert_eq!(restored.review_ranges[0].reasons, vec!["chunk_boundary"]);
        assert!(directory.path().read_dir().unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        assert!(load_receipt(directory.path(), 2).is_err());
    }
}

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::transcription::{AudioChunk, TranscriptSegment};

use super::LocalAsrSetup;

pub(super) const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

pub(super) struct ValidatedSetup {
    pub python: PathBuf,
    pub model: PathBuf,
}

fn absolute_path(value: &str) -> Result<PathBuf, String> {
    if value.len() > 4096 || value.chars().any(char::is_control) || !Path::new(value).is_absolute()
    {
        return Err("Choose an existing absolute local path.".into());
    }
    Path::new(value)
        .canonicalize()
        .map_err(|_| "The selected local path does not exist.".into())
}

pub(super) fn validate_setup(setup: &LocalAsrSetup) -> Result<ValidatedSetup, String> {
    let python = absolute_path(&setup.python_path)?;
    if !python.is_file() || python.metadata().map_err(|_| "Cannot read Python.")?.len() == 0 {
        return Err("Choose the installed Python executable.".into());
    }
    // Windows batch files would require a shell; only a direct executable is accepted.
    #[cfg(windows)]
    if python
        .extension()
        .is_none_or(|extension| !extension.eq_ignore_ascii_case("exe"))
    {
        return Err("Choose python.exe from the installed runtime or virtual environment.".into());
    }
    let model = absolute_path(&setup.model_path)?;
    if !model.is_dir()
        || ["config.json", "tokenizer.json", "model.bin"]
            .iter()
            .any(|name| {
                let file = model.join(name);
                !file.is_file() || file.metadata().map_or(true, |metadata| metadata.len() == 0)
            })
    {
        return Err("Choose an existing CTranslate2 model directory containing config.json, tokenizer.json and model.bin.".into());
    }
    if setup.language.as_ref().is_some_and(|language| {
        !(2..=3).contains(&language.len())
            || !language.bytes().all(|byte| byte.is_ascii_lowercase())
    }) {
        return Err(
            "Use a lowercase two- or three-letter speech language code, or leave it empty.".into(),
        );
    }
    Ok(ValidatedSetup { python, model })
}

#[derive(Deserialize)]
struct TranscriptOutput {
    schema_version: u32,
    source: String,
    model_path: String,
    duration_seconds: f64,
    segments: Vec<OutputSegment>,
}

#[derive(Deserialize)]
struct OutputSegment {
    start: f64,
    end: f64,
    text: String,
    language: Option<String>,
    review_reasons: Vec<String>,
}

#[derive(Deserialize)]
struct QualityOutput {
    duration_seconds: f64,
    segment_count: usize,
    review_segment_count: usize,
    review_segments: Vec<usize>,
    gaps_at_least_5_seconds: Vec<Gap>,
}

#[derive(Deserialize)]
struct Gap {
    start: f64,
    end: f64,
}

pub(super) struct ParsedOutput {
    pub segments: Vec<TranscriptSegment>,
    pub review_count: usize,
    pub gap_count: usize,
    pub languages: Vec<String>,
    pub review_ranges: Vec<super::receipts::ReviewRange>,
}

/// Validate source identity and local timestamps before applying the source offset.
/// The embedded script assigns word ownership before grouping. The segment
/// midpoint is checked only for SQLite compatibility; it never discards text.
pub(super) fn parse_output(
    transcript: &[u8],
    quality: &[u8],
    wav: &Path,
    model: &Path,
    actual_seconds: f64,
    source_seconds: f64,
    chunk: AudioChunk,
) -> Result<ParsedOutput, String> {
    let invalid = || "The local model returned invalid transcript or quality output.".to_string();
    if transcript.len() as u64 > MAX_OUTPUT_BYTES || quality.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(invalid());
    }
    let output: TranscriptOutput = serde_json::from_slice(transcript).map_err(|_| invalid())?;
    let report: QualityOutput = serde_json::from_slice(quality).map_err(|_| invalid())?;
    if output.schema_version != 1
        || Path::new(&output.source).canonicalize().ok().as_deref() != Some(wav)
        || Path::new(&output.model_path).canonicalize().ok().as_deref() != Some(model)
        || !output.duration_seconds.is_finite()
        || output.duration_seconds <= 0.0
        || (output.duration_seconds - actual_seconds).abs() > 0.002
        || (report.duration_seconds - output.duration_seconds).abs() > 0.001
        || !report.duration_seconds.is_finite()
        || output.segments.len() > 20_000
        || report.segment_count != output.segments.len()
    {
        return Err(invalid());
    }
    let expected_review: Vec<usize> = output
        .segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| !segment.review_reasons.is_empty())
        .map(|(index, _)| index + 1)
        .collect();
    if report.review_segment_count != expected_review.len()
        || report.review_segments != expected_review
        || report.gaps_at_least_5_seconds.len() > 20_001
        || report.gaps_at_least_5_seconds.iter().any(|gap| {
            !gap.start.is_finite()
                || !gap.end.is_finite()
                || gap.start < 0.0
                || gap.end <= gap.start
                || gap.end > actual_seconds + 0.002
        })
    {
        return Err(invalid());
    }
    let mut parsed = ParsedOutput {
        segments: Vec::new(),
        review_count: 0,
        gap_count: report.gaps_at_least_5_seconds.len(),
        languages: Vec::new(),
        review_ranges: Vec::new(),
    };
    let mut previous = -1.0;
    for segment in output.segments {
        if !segment.start.is_finite()
            || !segment.end.is_finite()
            || segment.start < 0.0
            || segment.end <= segment.start
            || segment.end > actual_seconds + 0.002
            || segment.start < previous
            || segment.text.trim().is_empty()
            || segment.text.len() > 16_384
            || segment
                .text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
            || segment.review_reasons.len() > 16
            || segment.review_reasons.iter().any(|reason| {
                !matches!(
                    reason.as_str(),
                    "low_model_confidence"
                        | "possible_nonspeech"
                        | "possible_repetition"
                        | "no_word_timestamps"
                        | "chunk_boundary"
                )
            })
        {
            return Err(invalid());
        }
        previous = segment.start;
        let start = chunk.start_ms as f64 / 1000.0 + segment.start;
        let end = (chunk.start_ms as f64 / 1000.0 + segment.end).min(source_seconds);
        let midpoint_ms = (start + end) * 500.0;
        if midpoint_ms < chunk.core_start_ms as f64 || midpoint_ms >= chunk.core_end_ms as f64 {
            return Err(invalid());
        }
        if end <= start {
            return Err(invalid());
        }
        if let Some(language) = segment.language {
            if language.len() > 16
                || !language
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
            {
                return Err(invalid());
            }
            if !parsed.languages.contains(&language) {
                parsed.languages.push(language);
            }
        }
        parsed.review_count += usize::from(!segment.review_reasons.is_empty());
        if !segment.review_reasons.is_empty() {
            parsed.review_ranges.push(super::receipts::ReviewRange {
                start_seconds: start,
                end_seconds: end,
                reasons: segment.review_reasons,
            });
        }
        parsed.segments.push(TranscriptSegment {
            start_seconds: start,
            end_seconds: end,
            text: segment.text,
            speaker: None,
        });
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let wav = root.path().join("audio.wav");
        std::fs::write(&wav, b"fixture").unwrap();
        let wav = wav.canonicalize().unwrap();
        let model = root.path().canonicalize().unwrap();
        (root, wav, model)
    }

    #[test]
    fn validates_offset_ownership_and_quality_before_import() {
        let (_root, wav, model) = fixture();
        let output = json!({"schema_version":1,"source":wav,"model_path":model,
            "duration_seconds":10,"segments":[
                {"start":3,"end":4,"text":"keep source time","language":"pt","review_reasons":["chunk_boundary"]}]});
        let quality = json!({"duration_seconds":10,"segment_count":1,"review_segment_count":1,
            "review_segments":[1],"gaps_at_least_5_seconds":[{"start":4,"end":10}]});
        let chunk = AudioChunk {
            index: 1,
            start_ms: 298000,
            end_ms: 308000,
            core_start_ms: 300000,
            core_end_ms: 306000,
        };
        let parse = |o: &serde_json::Value, q: &serde_json::Value| {
            parse_output(
                &serde_json::to_vec(o).unwrap(),
                &serde_json::to_vec(q).unwrap(),
                &wav,
                &model,
                10.0,
                600.0,
                chunk,
            )
        };
        let result = parse(&output, &quality).unwrap();
        assert_eq!(result.segments.len(), 1);
        assert_eq!(result.segments[0].start_seconds, 301.0);
        assert_eq!(result.review_count, 1);
        for bad in [json!(-1), json!(11), json!("NaN")] {
            let mut invalid = output.clone();
            invalid["segments"][0]["end"] = bad;
            assert!(parse(&invalid, &quality).is_err());
        }
        let mut bad = quality.clone();
        bad["review_segments"] = json!([2]);
        assert!(parse(&output, &bad).is_err());
        let mut bad = output.clone();
        bad["source"] = json!("missing-file.wav");
        assert!(parse(&bad, &quality).is_err());
    }

    #[test]
    fn rejects_relative_runtime_missing_model_and_language_injection() {
        let setup = LocalAsrSetup {
            python_path: "python".into(),
            model_path: "model".into(),
            language: None,
        };
        assert!(validate_setup(&setup).is_err());
        let root = tempfile::tempdir().unwrap();
        let python = root.path().join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        std::fs::write(&python, b"fixture").unwrap();
        let mut setup = LocalAsrSetup {
            python_path: python.to_string_lossy().into(),
            model_path: root.path().to_string_lossy().into(),
            language: None,
        };
        assert!(validate_setup(&setup).is_err());
        for name in ["config.json", "model.bin", "tokenizer.json"] {
            std::fs::write(root.path().join(name), b"fixture").unwrap();
        }
        assert!(validate_setup(&setup).is_ok());
        setup.language = Some("pt --bad".into());
        assert!(validate_setup(&setup).is_err());
    }
}
